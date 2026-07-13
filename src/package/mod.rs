use cps_common::{
    architecture::Architecture, dependency::Dependency, errors::CpsiError, package::Package,
    version::Version,
};
use serde::Deserialize;
use std::{
    fs::{self, File},
    io::{BufReader, Read},
    ops::Deref,
    path::{Component, Path, PathBuf},
    process::Command,
    str::FromStr,
};

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

/// Metadata read from a package's `.pkg/info` file.
///
/// The on-disk representation uses strings for versions, architectures, and
/// dependencies. `PackageInfo` exposes the normalized shared `Package` type so
/// callers do not need to repeat those conversions.
#[derive(Debug, Clone)]
pub struct PackageInfo {
    pub package: Package,
}

impl PackageInfo {
    pub fn into_package(self) -> Package {
        self.package
    }
}

impl Deref for PackageInfo {
    type Target = Package;

    fn deref(&self) -> &Self::Target {
        &self.package
    }
}

/// Result of installing an already extracted package.
#[derive(Debug)]
pub struct InstallOutcome {
    pub package_info: PackageInfo,
    /// Destination paths installed below the selected root.
    pub files: Vec<PathBuf>,
    /// A post-install failure is non-fatal, but is retained for callers that
    /// want to surface it in addition to the warning printed here.
    pub post_script_error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawArchitectures {
    One(String),
    Many(Vec<String>),
}

impl RawArchitectures {
    fn parse(self) -> Result<Vec<Architecture>, CpsiError> {
        let values = match self {
            Self::One(value) => vec![value],
            Self::Many(values) => values,
        };

        if values.is_empty() {
            return Err(CpsiError::InvalidPackage(
                "package architecture list is empty".to_string(),
            ));
        }

        values
            .into_iter()
            .map(|value| Architecture::from_str(&value))
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct RawPackageInfo {
    name: String,
    version: String,
    release: u32,
    arch: RawArchitectures,

    #[serde(default)]
    description: String,
    #[serde(default)]
    license: String,
    #[serde(default)]
    package_size: u64,
    #[serde(default)]
    installed_size: u64,

    #[serde(default, rename = "depends", alias = "dependencies")]
    dependencies: Vec<String>,
    #[serde(default)]
    provides: Vec<String>,
    #[serde(default)]
    repository: String,
}

/// Install a package into the system root.
pub async fn install(package_path: &Path) -> Result<(), CpsiError> {
    install_to_root(package_path, Path::new("/"))
        .await
        .map(|_| ())
}

/// Install a package below an alternate root and return its installed-file
/// manifest. The alternate root makes the installation flow testable without
/// requiring root privileges.
pub async fn install_to_root(
    package_path: &Path,
    root: &Path,
) -> Result<InstallOutcome, CpsiError> {
    let package_path = package_path.to_path_buf();
    let root = root.to_path_buf();

    tokio::task::spawn_blocking(move || {
        let temporary = tempfile::Builder::new().prefix("cpsi-install-").tempdir()?;
        extract_clos(&package_path, temporary.path())?;
        install_extracted_with_outcome(temporary.path(), &root)
    })
    .await
    .map_err(|error| {
        CpsiError::InvalidPackage(format!("package installation worker failed: {error}"))
    })?
}

/// Extract a plain tar or zstd-compressed tar `.clos` archive into `dest`.
/// Compression is detected by the zstd frame magic rather than the extension.
pub fn extract_clos(clos_path: &Path, dest: &Path) -> Result<PathBuf, CpsiError> {
    fs::create_dir_all(dest)?;

    let mut probe = File::open(clos_path)?;
    let mut magic = [0_u8; 4];
    let bytes_read = probe.read(&mut magic)?;
    drop(probe);

    if bytes_read == magic.len() && magic == ZSTD_MAGIC {
        let file = File::open(clos_path)?;
        let decoder = zstd::stream::read::Decoder::new(BufReader::new(file))?;
        unpack_archive(decoder, dest)?;
    } else {
        unpack_archive(BufReader::new(File::open(clos_path)?), dest)?;
    }

    Ok(dest.to_path_buf())
}

/// Parse and normalize `.pkg/info` from an extracted package.
pub fn read_package_info(extracted_dir: &Path) -> Result<PackageInfo, CpsiError> {
    let info_path = extracted_dir.join(".pkg/info");
    let contents = fs::read_to_string(&info_path)?;
    let raw: RawPackageInfo =
        toml::from_str(&contents).map_err(|error| CpsiError::Toml(error.to_string()))?;

    validate_package_name(&raw.name)?;

    let version = Version::from_str(&raw.version).map_err(|_| {
        CpsiError::InvalidPackage(format!(
            "invalid version in {}: {}",
            info_path.display(),
            raw.version
        ))
    })?;
    let arch = raw.arch.parse()?;
    let dependencies: Result<Vec<Dependency>, CpsiError> = raw
        .dependencies
        .into_iter()
        .map(|dependency| Dependency::from_str(&dependency))
        .collect();

    Ok(PackageInfo {
        package: Package {
            name: raw.name,
            version,
            release: raw.release,
            arch,
            dependencies: dependencies?,
            description: raw.description,
            provides: raw.provides,
            license: raw.license,
            package_size: raw.package_size,
            installed_size: raw.installed_size,
            repository: raw.repository,
        },
    })
}

/// Run a package script with `/bin/sh` and fail on a non-zero exit status.
pub fn run_script(script_path: &Path) -> Result<(), CpsiError> {
    let working_directory = script_path.parent().unwrap_or_else(|| Path::new("/"));
    run_script_in(script_path, working_directory)
}

/// Copy all payload entries below `data_dir` into `root`, overwriting existing
/// regular files as specified by the package format.
pub fn install_data_files(data_dir: &Path, root: &Path) -> Result<(), CpsiError> {
    install_data_files_with_manifest(data_dir, root).map(|_| ())
}

/// Return the destination paths represented by a package's `data/` directory,
/// without following symbolic links.
pub fn list_data_files(data_dir: &Path, root: &Path) -> Result<Vec<PathBuf>, CpsiError> {
    let entries = collect_data_entries(data_dir)?;
    Ok(entries
        .into_iter()
        .filter(|entry| !entry.is_directory)
        .map(|entry| root.join(entry.relative_path))
        .collect())
}

/// Install payload files and return their destination paths for the
/// installed-package database.
pub fn install_data_files_with_manifest(
    data_dir: &Path,
    root: &Path,
) -> Result<Vec<PathBuf>, CpsiError> {
    let entries = collect_data_entries(data_dir)?;
    fs::create_dir_all(root)?;

    for entry in &entries {
        let destination = root.join(&entry.relative_path);
        if entry.is_directory {
            ensure_destination_directory(&destination)?;
        } else if entry.is_symlink {
            copy_symbolic_link(&entry.source_path, &destination, &entry.relative_path)?;
        } else {
            copy_regular_file(&entry.source_path, &destination)?;
        }
    }

    Ok(entries
        .into_iter()
        .filter(|entry| !entry.is_directory)
        .map(|entry| root.join(entry.relative_path))
        .collect())
}

/// Run the complete script-and-copy flow for a package that has already been
/// extracted. This split lets the CLI inspect `list_data_files` and perform
/// conflict checks before any script or filesystem mutation.
pub fn install_extracted(extracted_dir: &Path, root: &Path) -> Result<Vec<PathBuf>, CpsiError> {
    install_extracted_with_outcome(extracted_dir, root).map(|outcome| outcome.files)
}

/// Detailed variant of [`install_extracted`] used by callers that also need
/// normalized metadata or the non-fatal post-install error.
pub fn install_extracted_with_outcome(
    extracted_dir: &Path,
    root: &Path,
) -> Result<InstallOutcome, CpsiError> {
    let package_info = read_package_info(extracted_dir)?;
    let data_dir = extracted_dir.join("data");
    let scripts_dir = extracted_dir.join(".pkg/scripts");
    let pre_script = scripts_dir.join("pre");
    let post_script = scripts_dir.join("post");

    if !data_dir.is_dir() {
        return Err(CpsiError::InvalidPackage(format!(
            "missing data directory: {}",
            data_dir.display()
        )));
    }
    if !post_script.is_file() {
        return Err(CpsiError::InvalidPackage(format!(
            "missing post-install script: {}",
            post_script.display()
        )));
    }

    if pre_script.try_exists()? {
        if !pre_script.is_file() {
            return Err(CpsiError::InvalidPackage(format!(
                "pre-install script is not a file: {}",
                pre_script.display()
            )));
        }
        run_script_in(&pre_script, extracted_dir)?;
    }

    let files = install_data_files_with_manifest(&data_dir, root)?;
    let post_script_error = match run_script_in(&post_script, extracted_dir) {
        Ok(()) => None,
        Err(error) => {
            let message = error.to_string();
            eprintln!(
                "warning: post-install script for {} failed: {}",
                package_info.name, message
            );
            Some(message)
        }
    };

    Ok(InstallOutcome {
        package_info,
        files,
        post_script_error,
    })
}

fn unpack_archive<R: Read>(reader: R, dest: &Path) -> Result<(), CpsiError> {
    let mut archive = tar::Archive::new(reader);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.into_owned();
        validate_relative_path(&entry_path, "archive entry")?;

        let entry_type = entry.header().entry_type();
        if (entry_type.is_symlink() || entry_type.is_hard_link())
            && let Some(target) = entry.link_name()?
        {
            validate_archive_link(&entry_path, &target, entry_type.is_symlink())?;
        }

        if !entry.unpack_in(dest)? {
            return Err(CpsiError::InvalidPackage(format!(
                "archive entry escapes extraction directory: {}",
                entry_path.display()
            )));
        }
    }

    Ok(())
}

fn validate_archive_link(
    entry_path: &Path,
    target: &Path,
    target_is_relative_to_entry: bool,
) -> Result<(), CpsiError> {
    if target.is_absolute() {
        return Err(CpsiError::InvalidPackage(format!(
            "archive link has an absolute target: {} -> {}",
            entry_path.display(),
            target.display()
        )));
    }

    let resolved = if target_is_relative_to_entry {
        entry_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(target)
    } else {
        target.to_path_buf()
    };
    validate_relative_path(&resolved, "archive link target")
}

fn validate_relative_path(path: &Path, kind: &str) -> Result<(), CpsiError> {
    let mut depth = 0_usize;

    for component in path.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CpsiError::InvalidPackage(format!(
                    "{kind} escapes package root: {}",
                    path.display()
                )));
            }
        }
    }

    Ok(())
}

fn validate_package_name(name: &str) -> Result<(), CpsiError> {
    if name.trim().is_empty()
        || matches!(name, "." | "..")
        || name.contains('/')
        || name.contains('\\')
    {
        return Err(CpsiError::InvalidPackage(format!(
            "invalid package name: {name:?}"
        )));
    }

    Ok(())
}

fn run_script_in(script_path: &Path, working_directory: &Path) -> Result<(), CpsiError> {
    let status = Command::new("/bin/sh")
        .arg(script_path)
        .current_dir(working_directory)
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(CpsiError::ScriptFailed(format!(
            "{} exited with {}",
            script_path.display(),
            status
        )))
    }
}

#[derive(Debug)]
struct DataEntry {
    source_path: PathBuf,
    relative_path: PathBuf,
    is_directory: bool,
    is_symlink: bool,
}

fn collect_data_entries(data_dir: &Path) -> Result<Vec<DataEntry>, CpsiError> {
    let metadata = fs::symlink_metadata(data_dir)?;
    if !metadata.file_type().is_dir() {
        return Err(CpsiError::InvalidPackage(format!(
            "package data path is not a directory: {}",
            data_dir.display()
        )));
    }

    let mut entries = Vec::new();
    collect_data_entries_from(data_dir, data_dir, &mut entries)?;
    entries.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(entries)
}

fn collect_data_entries_from(
    data_dir: &Path,
    current: &Path,
    entries: &mut Vec<DataEntry>,
) -> Result<(), CpsiError> {
    let mut children: Vec<_> = fs::read_dir(current)?.collect::<Result<_, _>>()?;
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        let source_path = child.path();
        let relative_path = source_path
            .strip_prefix(data_dir)
            .map_err(|_| {
                CpsiError::InvalidPackage(format!(
                    "payload path escaped data directory: {}",
                    source_path.display()
                ))
            })?
            .to_path_buf();
        validate_relative_path(&relative_path, "payload path")?;

        let metadata = fs::symlink_metadata(&source_path)?;
        let file_type = metadata.file_type();
        let is_directory = file_type.is_dir();
        let is_symlink = file_type.is_symlink();

        if !is_directory && !is_symlink && !file_type.is_file() {
            return Err(CpsiError::InvalidPackage(format!(
                "unsupported payload file type: {}",
                source_path.display()
            )));
        }

        entries.push(DataEntry {
            source_path: source_path.clone(),
            relative_path,
            is_directory,
            is_symlink,
        });

        if is_directory {
            collect_data_entries_from(data_dir, &source_path, entries)?;
        }
    }

    Ok(())
}

fn ensure_destination_directory(path: &Path) -> Result<(), CpsiError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => return Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if fs::metadata(path).is_ok_and(|target| target.is_dir()) {
                return Ok(());
            }
            fs::remove_file(path)?;
        }
        Ok(_) => fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    fs::create_dir_all(path)?;
    Ok(())
}

fn copy_regular_file(source: &Path, destination: &Path) -> Result<(), CpsiError> {
    if let Some(parent) = destination.parent() {
        ensure_destination_directory(parent)?;
    }

    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            return Err(CpsiError::InvalidPackage(format!(
                "cannot overwrite directory with file: {}",
                destination.display()
            )));
        }
        Ok(metadata) if metadata.file_type().is_symlink() => fs::remove_file(destination)?,
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    fs::copy(source, destination)?;
    Ok(())
}

fn copy_symbolic_link(
    source: &Path,
    destination: &Path,
    relative_path: &Path,
) -> Result<(), CpsiError> {
    let target = fs::read_link(source)?;
    validate_archive_link(&Path::new("data").join(relative_path), &target, true)?;

    if let Some(parent) = destination.parent() {
        ensure_destination_directory(parent)?;
    }

    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            return Err(CpsiError::InvalidPackage(format!(
                "cannot overwrite directory with symbolic link: {}",
                destination.display()
            )));
        }
        Ok(_) => fs::remove_file(destination)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(target, destination)?;

    #[cfg(not(unix))]
    return Err(CpsiError::InvalidPackage(
        "symbolic-link payloads are unsupported on this platform".to_string(),
    ));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cps_common::dependency::ComparisonOperator;
    use std::io::Write;

    const INFO: &str = r#"
name = "demo"
version = "1.2.3"
release = 4
arch = "x86_64"
description = "Demo package"
license = "MIT"
package_size = 123
installed_size = 456
depends = ["libc>=2.3.4", "shell"]
provides = ["demo-provider"]
"#;

    fn test_directory(prefix: &str) -> tempfile::TempDir {
        let base = Path::new("/tmp/opencode");
        fs::create_dir_all(base).unwrap();
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(base)
            .unwrap()
    }

    fn create_extracted_package(base: &Path, pre: Option<&str>, post: &str) -> PathBuf {
        let extracted = base.join("extracted");
        fs::create_dir_all(extracted.join(".pkg/scripts")).unwrap();
        fs::create_dir_all(extracted.join("data/usr/share")).unwrap();
        fs::write(extracted.join(".pkg/info"), INFO).unwrap();
        if let Some(pre) = pre {
            fs::write(extracted.join(".pkg/scripts/pre"), pre).unwrap();
        }
        fs::write(extracted.join(".pkg/scripts/post"), post).unwrap();
        fs::write(extracted.join("data/usr/share/demo.txt"), "new payload").unwrap();
        extracted
    }

    fn create_tar(source: &Path, destination: &Path, zstd_compressed: bool) {
        if zstd_compressed {
            let file = File::create(destination).unwrap();
            let encoder = zstd::stream::Encoder::new(file, 0).unwrap();
            let mut builder = tar::Builder::new(encoder);
            builder.append_dir_all(".", source).unwrap();
            let encoder = builder.into_inner().unwrap();
            encoder.finish().unwrap();
        } else {
            let file = File::create(destination).unwrap();
            let mut builder = tar::Builder::new(file);
            builder.append_dir_all(".", source).unwrap();
            builder.finish().unwrap();
        }
    }

    fn write_octal(field: &mut [u8], value: u64) {
        let value = format!("{:0width$o}\0", value, width = field.len() - 1);
        field.copy_from_slice(value.as_bytes());
    }

    fn create_raw_tar_with_path(destination: &Path, name: &str, contents: &[u8]) {
        let mut header = [0_u8; 512];
        header[..name.len()].copy_from_slice(name.as_bytes());
        write_octal(&mut header[100..108], 0o644);
        write_octal(&mut header[108..116], 0);
        write_octal(&mut header[116..124], 0);
        write_octal(&mut header[124..136], contents.len() as u64);
        write_octal(&mut header[136..148], 0);
        header[148..156].fill(b' ');
        header[156] = b'0';
        header[257..263].copy_from_slice(b"ustar\0");
        header[263..265].copy_from_slice(b"00");
        let checksum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
        let checksum = format!("{:06o}\0 ", checksum);
        header[148..156].copy_from_slice(checksum.as_bytes());

        let mut file = File::create(destination).unwrap();
        file.write_all(&header).unwrap();
        file.write_all(contents).unwrap();
        let padding = (512 - contents.len() % 512) % 512;
        file.write_all(&vec![0; padding]).unwrap();
        file.write_all(&[0; 1024]).unwrap();
    }

    #[test]
    fn reads_and_normalizes_package_info() {
        let temporary = test_directory("cpsi-info-");
        let extracted = create_extracted_package(temporary.path(), None, "exit 0\n");

        let info = read_package_info(&extracted).unwrap();

        assert_eq!(info.name, "demo");
        assert_eq!(info.version, Version::from("1.2.3"));
        assert_eq!(info.release, 4);
        assert_eq!(info.arch, vec![Architecture::X86_64]);
        assert_eq!(info.license, "MIT");
        assert_eq!(info.package_size, 123);
        assert_eq!(info.installed_size, 456);
        assert_eq!(info.dependencies.len(), 2);
        assert_eq!(info.dependencies[0].operator, Some(ComparisonOperator::Gte));
        assert_eq!(info.dependencies[0].version, Some(Version::from("2.3.4")));
    }

    #[test]
    fn extracts_plain_tar_and_zstd_tar() {
        let temporary = test_directory("cpsi-extract-");
        let source = create_extracted_package(temporary.path(), None, "exit 0\n");

        for (filename, compressed) in [("plain.clos", false), ("zstd.clos", true)] {
            let archive = temporary.path().join(filename);
            let destination = temporary.path().join(format!("out-{compressed}"));
            create_tar(&source, &archive, compressed);

            assert_eq!(extract_clos(&archive, &destination).unwrap(), destination);
            assert_eq!(
                fs::read_to_string(destination.join("data/usr/share/demo.txt")).unwrap(),
                "new payload"
            );
        }
    }

    #[test]
    fn rejects_archive_path_traversal() {
        let temporary = test_directory("cpsi-traversal-");
        let archive = temporary.path().join("malicious.clos");
        let destination = temporary.path().join("output");
        let escaped = temporary.path().join("escaped");
        create_raw_tar_with_path(&archive, "../escaped", b"not allowed");

        assert!(matches!(
            extract_clos(&archive, &destination),
            Err(CpsiError::InvalidPackage(_))
        ));
        assert!(!escaped.exists());
    }

    #[test]
    fn installs_extracted_package_and_returns_manifest() {
        let temporary = test_directory("cpsi-install-");
        let pre_marker = temporary.path().join("pre-ran");
        let post_marker = temporary.path().join("post-ran");
        let pre = format!("printf pre > '{}'\n", pre_marker.display());
        let post = format!("printf post > '{}'\n", post_marker.display());
        let extracted = create_extracted_package(temporary.path(), Some(&pre), post.as_str());
        let root = temporary.path().join("root");
        fs::create_dir_all(root.join("usr/share")).unwrap();
        fs::write(root.join("usr/share/demo.txt"), "old payload").unwrap();

        let outcome = install_extracted_with_outcome(&extracted, &root).unwrap();

        assert_eq!(
            fs::read_to_string(root.join("usr/share/demo.txt")).unwrap(),
            "new payload"
        );
        assert_eq!(fs::read_to_string(pre_marker).unwrap(), "pre");
        assert_eq!(fs::read_to_string(post_marker).unwrap(), "post");
        assert_eq!(outcome.files, vec![root.join("usr/share/demo.txt")]);
        assert!(outcome.post_script_error.is_none());
    }

    #[test]
    fn pre_script_failure_stops_before_copying() {
        let temporary = test_directory("cpsi-pre-failure-");
        let extracted = create_extracted_package(temporary.path(), Some("exit 7\n"), "exit 0\n");
        let root = temporary.path().join("root");

        assert!(matches!(
            install_extracted(&extracted, &root),
            Err(CpsiError::ScriptFailed(_))
        ));
        assert!(!root.join("usr/share/demo.txt").exists());
    }

    #[test]
    fn post_script_failure_is_non_fatal() {
        let temporary = test_directory("cpsi-post-failure-");
        let extracted = create_extracted_package(temporary.path(), None, "exit 9\n");
        let root = temporary.path().join("root");

        let outcome = install_extracted_with_outcome(&extracted, &root).unwrap();

        assert!(root.join("usr/share/demo.txt").is_file());
        assert!(outcome.post_script_error.is_some());
    }

    #[tokio::test]
    async fn install_to_root_extracts_and_installs() {
        let temporary = test_directory("cpsi-full-install-");
        let source = create_extracted_package(temporary.path(), None, "exit 0\n");
        let archive = temporary.path().join("demo.clos");
        let root = temporary.path().join("root");
        create_tar(&source, &archive, true);

        let outcome = install_to_root(&archive, &root).await.unwrap();

        assert_eq!(outcome.package_info.name, "demo");
        assert_eq!(outcome.files, vec![root.join("usr/share/demo.txt")]);
        assert!(root.join("usr/share/demo.txt").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn preserves_payload_symbolic_links() {
        let temporary = test_directory("cpsi-symlink-");
        let data = temporary.path().join("data");
        fs::create_dir_all(data.join("usr/lib")).unwrap();
        fs::create_dir_all(data.join("usr/bin")).unwrap();
        fs::write(data.join("usr/lib/demo"), "binary").unwrap();
        std::os::unix::fs::symlink("../lib/demo", data.join("usr/bin/demo")).unwrap();
        let root = temporary.path().join("root");

        let files = install_data_files_with_manifest(&data, &root).unwrap();

        assert_eq!(
            fs::read_link(root.join("usr/bin/demo")).unwrap(),
            PathBuf::from("../lib/demo")
        );
        assert_eq!(
            files,
            vec![root.join("usr/bin/demo"), root.join("usr/lib/demo")]
        );
    }
}
