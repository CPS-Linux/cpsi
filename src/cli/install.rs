use crate::{
    database::{InstalledDatabase, InstalledPackage},
    dependency, package,
    repository::{parquet::Repository, sync::RepositoryConfig, validate_repository_name},
    signature::{load_public_key_from, verify_file_with_sig},
    util::{
        constants,
        net::{self, Download},
    },
};
use cps_common::{
    architecture::Architecture, errors::CpsiError, package::Package, version::Version,
};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
};

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Filesystem locations used by the install pipeline.
///
/// Production commands use [`InstallContext::system`]. Tests and image-building
/// tools can inject an alternate root and state directories without touching
/// the host CPSI installation.
#[derive(Clone, Debug)]
pub struct InstallContext {
    pub root: PathBuf,
    pub database_dir: PathBuf,
    pub downloads_dir: PathBuf,
    pub repositories_dir: PathBuf,
    pub config_dir: PathBuf,
    pub keys_dir: PathBuf,
}

impl InstallContext {
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/"),
            database_dir: PathBuf::from(constants::INSTALLED_DATABASE_DIRECTORY),
            downloads_dir: PathBuf::from(constants::TEMP_DOWNLOAD_LOCATION),
            repositories_dir: PathBuf::from(constants::REPOSITORIES_DIRECTORY),
            config_dir: PathBuf::from(constants::REPOSITORIES_CONFIG_DIRECTORY),
            keys_dir: PathBuf::from(constants::PUBLIC_KEYS_DIRECTORY),
        }
    }
}

impl Default for InstallContext {
    fn default() -> Self {
        Self::system()
    }
}

/// Install specified packages using the system CPSI directories.
pub fn install(package_names: &[String]) -> Result<(), CpsiError> {
    install_with_context(package_names, &InstallContext::system())
}

/// Install specified packages using caller-provided filesystem locations.
pub fn install_with_context(
    package_names: &[String],
    context: &InstallContext,
) -> Result<(), CpsiError> {
    install_with_context_and_mode(package_names, context, false)
}

/// Shared install/upgrade pipeline. Upgrade mode is intentionally crate-local;
/// the public entry point lives in `cli::upgrade`.
pub(crate) fn install_with_context_allow_upgrade(
    package_names: &[String],
    context: &InstallContext,
) -> Result<(), CpsiError> {
    install_with_context_and_mode(package_names, context, true)
}

fn install_with_context_and_mode(
    package_names: &[String],
    context: &InstallContext,
    allow_upgrade: bool,
) -> Result<(), CpsiError> {
    validate_context(context)?;
    if package_names.is_empty() {
        return Ok(());
    }

    let repository = Repository::load_registered_from(
        &context.config_dir,
        &context.keys_dir,
        &context.repositories_dir,
    )?;
    let configs = load_repository_configs(&context.config_dir)?;
    let mut database = InstalledDatabase::load_from(&context.database_dir)?;
    let targets = select_targets(package_names, &repository, &database, allow_upgrade)?;

    if targets.is_empty() {
        return Ok(());
    }

    let resolved = dependency::resolve::resolve(&targets, &repository)?;
    println!("Packages to process: {}", resolved.len());

    for candidate in resolved {
        if !should_install_resolved(candidate, &database, allow_upgrade) {
            continue;
        }

        let config = configs
            .get(&candidate.repository)
            .ok_or_else(|| CpsiError::RepositoryNotFound(candidate.repository.clone()))?;
        install_one(
            candidate,
            repository.architecture(),
            config,
            context,
            &mut database,
        )?;
    }

    Ok(())
}

fn select_targets<'a>(
    package_names: &[String],
    repository: &'a Repository,
    database: &InstalledDatabase,
    allow_upgrade: bool,
) -> Result<Vec<&'a Package>, CpsiError> {
    let mut targets = Vec::new();

    for name in package_names {
        let candidate = repository
            .find_package(name)
            .ok_or_else(|| CpsiError::PackageNotFound(name.clone()))?;

        match database.get_package(name) {
            None => targets.push(candidate),
            Some(installed) => match compare_candidate(installed, candidate) {
                Ordering::Equal => println!(
                    "Skipping {}: version {}-k{} is already installed",
                    candidate.name, candidate.version, candidate.release
                ),
                Ordering::Greater if allow_upgrade => targets.push(candidate),
                Ordering::Less if allow_upgrade => {
                    return Err(CpsiError::DowngradeNotAllowed(format!(
                        "{} {}-k{} -> {}-k{}",
                        candidate.name,
                        installed.version,
                        installed.release,
                        candidate.version,
                        candidate.release
                    )));
                }
                _ => eprintln!(
                    "warning: {} {}-k{} is already installed; repository version {}-k{} requires `cpsi upgrade`",
                    installed.name,
                    installed.version,
                    installed.release,
                    candidate.version,
                    candidate.release
                ),
            },
        }
    }

    Ok(targets)
}

fn should_install_resolved(
    candidate: &Package,
    database: &InstalledDatabase,
    allow_upgrade: bool,
) -> bool {
    let Some(installed) = database.get_package(&candidate.name) else {
        return true;
    };

    match compare_candidate(installed, candidate) {
        Ordering::Greater if allow_upgrade => true,
        Ordering::Equal => {
            println!(
                "Skipping {}: version {}-k{} is already installed",
                candidate.name, candidate.version, candidate.release
            );
            false
        }
        _ => {
            eprintln!(
                "warning: skipping {} {}-k{} because {}-k{} is installed",
                candidate.name,
                candidate.version,
                candidate.release,
                installed.version,
                installed.release
            );
            false
        }
    }
}

pub(crate) fn candidate_is_newer(installed: &InstalledPackage, candidate: &Package) -> bool {
    compare_candidate(installed, candidate) == Ordering::Greater
}

fn compare_candidate(installed: &InstalledPackage, candidate: &Package) -> Ordering {
    compare_version_release(
        &candidate.version,
        candidate.release,
        &installed.version,
        installed.release,
    )
}

fn compare_version_release(
    left_version: &Version,
    left_release: u32,
    right_version: &Version,
    right_release: u32,
) -> Ordering {
    left_version
        .cmp(right_version)
        .then_with(|| left_release.cmp(&right_release))
}

fn install_one(
    index_package: &Package,
    architecture: Architecture,
    repository: &RepositoryConfig,
    context: &InstallContext,
    database: &mut InstalledDatabase,
) -> Result<(), CpsiError> {
    println!(
        "Preparing {} {}-k{}",
        index_package.name, index_package.version, index_package.release
    );

    let mut artifact = acquire_artifact(
        index_package,
        architecture,
        repository,
        &context.downloads_dir,
        &context.keys_dir,
    )?;

    println!("Extracting {}", index_package.name);
    let extracted = tempfile::Builder::new().prefix("cpsi-package-").tempdir()?;
    package::extract_clos(artifact.path(), extracted.path())?;
    let archive_info = package::read_package_info(extracted.path())?;
    validate_archive_metadata(index_package, &archive_info.package, architecture)?;

    // A verified artifact only becomes visible in the persistent cache after
    // it has also been shown to match the selected index row.
    artifact.commit()?;

    let data_dir = extracted.path().join("data");
    let planned_files = package::list_data_files(&data_dir, &context.root)?;
    for path in &planned_files {
        if let Some(owner) = database.has_file_conflict(path, &index_package.name) {
            return Err(CpsiError::FileConflict(
                path.display().to_string(),
                owner.to_string(),
            ));
        }
    }

    let previously_owned = database.files_for_package(&index_package.name);
    println!("Installing {}", index_package.name);
    let installed_files = package::install_extracted(extracted.path(), &context.root)?;
    debug_assert_eq!(planned_files, installed_files);
    remove_stale_files(&previously_owned, &installed_files, &context.root)?;

    println!(
        "Registering {} in the installed database",
        index_package.name
    );
    database.add_package(index_package)?;
    database.add_files(&index_package.name, &installed_files)?;
    database.save()?;

    println!(
        "Installed {} {}-k{}",
        index_package.name, index_package.version, index_package.release
    );
    Ok(())
}

/// Remove files that were owned by the previous package version but are not
/// present in the replacement manifest. Both lexical and canonical-parent
/// checks are used so an injected root cannot be escaped through `..` or a
/// symlinked parent directory.
fn remove_stale_files(
    previously_owned: &[PathBuf],
    installed_files: &[PathBuf],
    root: &Path,
) -> Result<(), CpsiError> {
    let installed = installed_files.iter().collect::<HashSet<_>>();
    let stale = previously_owned
        .iter()
        .filter(|path| !installed.contains(path))
        .collect::<Vec<_>>();
    if stale.is_empty() {
        return Ok(());
    }

    let canonical_root = fs::canonicalize(root)?;
    for path in stale {
        if path == root || !path.starts_with(root) {
            return Err(CpsiError::Database(format!(
                "refusing to remove stale package path outside installation root: {}",
                path.display()
            )));
        }

        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let parent = path.parent().ok_or_else(|| {
            CpsiError::Database(format!(
                "stale package path has no parent: {}",
                path.display()
            ))
        })?;
        let canonical_parent = fs::canonicalize(parent)?;
        if !canonical_parent.starts_with(&canonical_root) {
            return Err(CpsiError::Database(format!(
                "refusing to remove stale package path through a parent outside installation root: {}",
                path.display()
            )));
        }

        if metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            fs::remove_file(path)?;
        } else {
            return Err(CpsiError::Database(format!(
                "refusing to remove non-file stale package path: {}",
                path.display()
            )));
        }
    }

    Ok(())
}

fn validate_archive_metadata(
    index: &Package,
    archive: &Package,
    architecture: Architecture,
) -> Result<(), CpsiError> {
    compare_metadata_field("name", &index.name, &archive.name, &index.name)?;
    compare_metadata_field("version", &index.version, &archive.version, &index.name)?;
    compare_metadata_field("release", &index.release, &archive.release, &index.name)?;

    if !archive.arch.contains(&architecture)
        || (!index.arch.is_empty() && !index.arch.contains(&architecture))
    {
        return Err(CpsiError::InvalidPackage(format!(
            "archive architecture does not match index for {}",
            index.name
        )));
    }

    compare_metadata_field(
        "dependencies",
        &index.dependencies,
        &archive.dependencies,
        &index.name,
    )?;
    compare_metadata_field(
        "description",
        &index.description,
        &archive.description,
        &index.name,
    )?;
    compare_metadata_field("provides", &index.provides, &archive.provides, &index.name)?;
    compare_metadata_field("license", &index.license, &archive.license, &index.name)?;
    compare_metadata_field(
        "package_size",
        &index.package_size,
        &archive.package_size,
        &index.name,
    )?;
    compare_metadata_field(
        "installed_size",
        &index.installed_size,
        &archive.installed_size,
        &index.name,
    )?;

    Ok(())
}

fn compare_metadata_field<T: PartialEq>(
    field: &str,
    index: &T,
    archive: &T,
    package_name: &str,
) -> Result<(), CpsiError> {
    if index == archive {
        Ok(())
    } else {
        Err(CpsiError::InvalidPackage(format!(
            "archive {field} does not match repository index for {package_name}"
        )))
    }
}

fn load_repository_configs(
    config_dir: &Path,
) -> Result<HashMap<String, RepositoryConfig>, CpsiError> {
    let repositories = RepositoryConfig::load_repositories_from(config_dir)?;
    if repositories.is_empty() {
        return Err(CpsiError::NoRepositories);
    }
    Ok(repositories
        .into_iter()
        .map(|repository| (repository.repo_name.clone(), repository))
        .collect())
}

fn validate_context(context: &InstallContext) -> Result<(), CpsiError> {
    if !context.root.is_absolute() {
        return Err(CpsiError::Database(format!(
            "installation root must be absolute: {}",
            context.root.display()
        )));
    }
    Ok(())
}

fn artifact_filename(package: &Package, architecture: Architecture) -> Result<String, CpsiError> {
    validate_package_component(&package.name)?;
    Ok(format!(
        "{}-{}-k{}-{}.clos",
        package.name, package.version, package.release, architecture
    ))
}

fn validate_package_component(name: &str) -> Result<(), CpsiError> {
    let valid = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'));

    if valid {
        Ok(())
    } else {
        Err(CpsiError::InvalidPackage(format!(
            "unsafe package name: {name}"
        )))
    }
}

fn acquire_artifact(
    package: &Package,
    architecture: Architecture,
    repository: &RepositoryConfig,
    downloads_dir: &Path,
    keys_dir: &Path,
) -> Result<Artifact, CpsiError> {
    validate_repository_name(&repository.repo_name)?;
    let filename = artifact_filename(package, architecture)?;
    let repository_dir = downloads_dir.join(&repository.repo_name);
    fs::create_dir_all(&repository_dir)?;

    let final_package = repository_dir.join(&filename);
    let final_signature = append_suffix(&final_package, ".minisig");
    let public_key = trusted_public_key(repository, keys_dir)?;

    if final_package.is_file() {
        let cache_is_valid = match public_key.as_deref() {
            Some(key) if final_signature.is_file() => {
                verify_file_with_sig(&final_package, &final_signature, key).is_ok()
            }
            Some(_) => false,
            None => true,
        };

        if cache_is_valid {
            println!("Using cached {}", final_package.display());
            return Ok(Artifact::cached(final_package));
        }

        eprintln!(
            "warning: cached package {} failed verification; downloading a replacement",
            final_package.display()
        );
    }

    let sequence = STAGING_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
    let staged_package = repository_dir.join(format!(
        ".{filename}.stage-{}-{sequence}",
        std::process::id()
    ));
    let staged_signature = public_key
        .as_ref()
        .map(|_| append_suffix(&staged_package, ".minisig"));
    remove_if_present(&staged_package)?;
    if let Some(signature) = &staged_signature {
        remove_if_present(signature)?;
    }

    let staged = StagedArtifact {
        staged_package: staged_package.clone(),
        staged_signature: staged_signature.clone(),
        final_package: final_package.clone(),
        final_signature: public_key.as_ref().map(|_| final_signature),
        committed: false,
    };

    println!("Downloading {filename}");
    net::download_file(Download::new(
        format!("{}/{}", repository.url.trim_end_matches('/'), filename),
        &staged_package,
    ))
    .map_err(|error| CpsiError::NetError(error.to_string()))?;

    if let (Some(key), Some(signature_path)) = (public_key.as_deref(), staged_signature.as_deref())
    {
        println!("Downloading and verifying {filename}.minisig");
        net::download_file(Download::new(
            format!(
                "{}/{}.minisig",
                repository.url.trim_end_matches('/'),
                filename
            ),
            signature_path,
        ))
        .map_err(|error| CpsiError::NetError(error.to_string()))?;
        verify_file_with_sig(&staged_package, signature_path, key)?;
    } else {
        eprintln!(
            "warning: repository '{}' is untrusted; package signature verification was skipped",
            repository.repo_name
        );
    }

    Ok(Artifact {
        path: staged_package,
        staged: Some(staged),
    })
}

fn trusted_public_key(
    repository: &RepositoryConfig,
    keys_dir: &Path,
) -> Result<Option<String>, CpsiError> {
    if !repository.trusted {
        return Ok(None);
    }

    match load_public_key_from(&repository.repo_name, keys_dir) {
        Ok(stored) if stored == repository.public_key => Ok(Some(stored)),
        Ok(_) => Err(CpsiError::SignatureVerificationFailed(format!(
            "stored key for repository '{}' does not match its configuration",
            repository.repo_name
        ))),
        Err(CpsiError::Io(error)) if error.kind() == ErrorKind::NotFound => {
            Ok(Some(repository.public_key.clone()))
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
struct Artifact {
    path: PathBuf,
    staged: Option<StagedArtifact>,
}

impl Artifact {
    fn cached(path: PathBuf) -> Self {
        Self { path, staged: None }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn commit(&mut self) -> Result<(), CpsiError> {
        let Some(staged) = self.staged.take() else {
            return Ok(());
        };
        let final_path = staged.final_package.clone();
        staged.commit()?;
        self.path = final_path;
        Ok(())
    }
}

#[derive(Debug)]
struct StagedArtifact {
    staged_package: PathBuf,
    staged_signature: Option<PathBuf>,
    final_package: PathBuf,
    final_signature: Option<PathBuf>,
    committed: bool,
}

impl StagedArtifact {
    fn commit(mut self) -> Result<(), CpsiError> {
        if let (Some(staged), Some(final_path)) = (&self.staged_signature, &self.final_signature) {
            fs::rename(staged, final_path)?;
        }
        fs::rename(&self.staged_package, &self.final_package)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagedArtifact {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = fs::remove_file(&self.staged_package);
        if let Some(signature) = &self.staged_signature {
            let _ = fs::remove_file(signature);
        }
    }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut result = OsString::from(path.as_os_str());
    result.push(suffix);
    PathBuf::from(result)
}

fn remove_if_present(path: &Path) -> Result<(), CpsiError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::FieldRef;
    use cps_common::dependency::{ComparisonOperator, Dependency};
    use parquet::arrow::ArrowWriter;
    use serde_arrow::schema::{SchemaLike, TracingOptions};
    use std::fs::File;

    const PUBLIC_KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const PREHASHED_SIGNATURE: &str = concat!(
        "untrusted comment: signature from minisign secret key\n",
        "RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/",
        "z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\n",
        "trusted comment: timestamp:1556193335\tfile:test\n",
        "y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==\n",
    );

    fn test_directory(prefix: &str) -> tempfile::TempDir {
        let base = std::env::temp_dir().join("opencode");
        fs::create_dir_all(&base).unwrap();
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in(base)
            .unwrap()
    }

    fn package(version: &str, release: u32) -> Package {
        Package {
            name: "demo".to_string(),
            version: Version::from(version),
            release,
            arch: vec![Architecture::X86_64],
            dependencies: Vec::<Dependency>::new(),
            description: "demo package".to_string(),
            provides: vec!["demo-provider".to_string()],
            license: "MIT".to_string(),
            package_size: 10,
            installed_size: 20,
            repository: "test".to_string(),
        }
    }

    fn installed(version: &str, release: u32) -> InstalledPackage {
        InstalledPackage {
            name: "demo".to_string(),
            version: Version::from(version),
            release,
            arch: vec![Architecture::X86_64],
            install_time: 0,
        }
    }

    fn write_repository_index(path: &Path, index_package: &Package) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Sample tracing avoids serde_arrow's type-tracing limitation for the
        // symbol-renamed ComparisonOperator variants (`=`, `>=`, ...).
        let mut schema_sample = index_package.clone();
        schema_sample.dependencies.push(Dependency {
            name: "schema-sample".to_string(),
            version: Some(Version::from("1.0.0")),
            operator: Some(ComparisonOperator::Gte),
        });
        let fields = Vec::<FieldRef>::from_samples(
            std::slice::from_ref(&schema_sample),
            TracingOptions::default().enums_without_data_as_strings(true),
        )
        .unwrap();
        let packages = vec![index_package.clone()];
        let batch = serde_arrow::to_record_batch(&fields, &packages).unwrap();
        let file = File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, batch.schema(), None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    fn write_cached_clos(base: &Path, destination: &Path, pre_marker: &Path, post_marker: &Path) {
        let source = base.join("clos-source");
        fs::create_dir_all(source.join(".pkg/scripts")).unwrap();
        fs::create_dir_all(source.join("data/usr/bin")).unwrap();
        fs::write(
            source.join(".pkg/info"),
            r#"
name = "demo"
version = "1.2.3"
release = 4
arch = "x86_64"
description = "demo package"
license = "MIT"
package_size = 10
installed_size = 20
depends = []
provides = ["demo-provider"]
"#,
        )
        .unwrap();
        fs::write(
            source.join(".pkg/scripts/pre"),
            format!("printf pre > '{}'\n", pre_marker.display()),
        )
        .unwrap();
        fs::write(
            source.join(".pkg/scripts/post"),
            format!("printf post > '{}'\n", post_marker.display()),
        )
        .unwrap();
        fs::write(source.join("data/usr/bin/demo"), "offline payload").unwrap();

        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        let file = File::create(destination).unwrap();
        let mut builder = tar::Builder::new(file);
        builder.append_dir_all(".", source).unwrap();
        builder.finish().unwrap();
    }

    fn prepare_offline_install(base: &Path) -> (InstallContext, PathBuf, PathBuf, PathBuf) {
        let context = InstallContext {
            root: base.join("root"),
            database_dir: base.join("database"),
            downloads_dir: base.join("downloads"),
            repositories_dir: base.join("repositories"),
            config_dir: base.join("repos.d"),
            keys_dir: base.join("keys"),
        };
        let index_package = package("1.2.3", 4);
        let config = RepositoryConfig {
            repo_name: "test".to_string(),
            url: "http://127.0.0.1:1".to_string(),
            public_key: PUBLIC_KEY.to_string(),
            fingerprint: String::new(),
            trusted: false,
        };
        fs::create_dir_all(&context.config_dir).unwrap();
        fs::write(
            context.config_dir.join("test.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        write_repository_index(
            &context.repositories_dir.join("test.parquet"),
            &index_package,
        );

        let cached_package = context.downloads_dir.join("test/demo-1.2.3-k4-x86_64.clos");
        let pre_marker = base.join("pre-ran");
        let post_marker = base.join("post-ran");
        write_cached_clos(base, &cached_package, &pre_marker, &post_marker);

        let installed_file = context.root.join("usr/bin/demo");
        (context, installed_file, pre_marker, post_marker)
    }

    #[test]
    fn builds_safe_artifact_filename() {
        assert_eq!(
            artifact_filename(&package("1.2.3", 4), Architecture::X86_64).unwrap(),
            "demo-1.2.3-k4-x86_64.clos"
        );

        let mut unsafe_package = package("1.2.3", 4);
        unsafe_package.name = "../escape".to_string();
        assert!(artifact_filename(&unsafe_package, Architecture::X86_64).is_err());
    }

    #[test]
    fn compares_version_before_release() {
        assert!(candidate_is_newer(
            &installed("1.0.0", 1),
            &package("1.0.0", 2)
        ));
        assert!(candidate_is_newer(
            &installed("1.9.9", 99),
            &package("2.0.0", 1)
        ));
        assert!(!candidate_is_newer(
            &installed("2.0.0", 1),
            &package("1.9.9", 99)
        ));
    }

    #[test]
    fn target_selection_skips_reinstalls_and_rejects_downgrades() {
        let temporary = test_directory("cpsi-install-selection-");
        let mut database = InstalledDatabase::load_from(temporary.path()).unwrap();
        database.add_package(&package("1.0.0", 1)).unwrap();
        let names = vec!["demo".to_string()];

        let newer =
            Repository::from_packages_for_arch(vec![package("1.0.0", 2)], Architecture::X86_64)
                .unwrap();
        assert!(
            select_targets(&names, &newer, &database, false)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            select_targets(&names, &newer, &database, true)
                .unwrap()
                .len(),
            1
        );

        let older =
            Repository::from_packages_for_arch(vec![package("0.9.0", 99)], Architecture::X86_64)
                .unwrap();
        assert!(matches!(
            select_targets(&names, &older, &database, true),
            Err(CpsiError::DowngradeNotAllowed(_))
        ));
    }

    #[test]
    fn validates_archive_metadata() {
        let index = package("1.2.3", 4);
        let mut archive = index.clone();
        archive.repository.clear();
        assert!(validate_archive_metadata(&index, &archive, Architecture::X86_64).is_ok());

        archive.release = 5;
        assert!(matches!(
            validate_archive_metadata(&index, &archive, Architecture::X86_64),
            Err(CpsiError::InvalidPackage(_))
        ));
    }

    #[test]
    fn reuses_a_valid_signed_cache_without_network() {
        let temporary = test_directory("cpsi-install-cache-");
        let downloads = temporary.path().join("downloads");
        let repository_dir = downloads.join("test");
        fs::create_dir_all(&repository_dir).unwrap();
        let final_package = repository_dir.join("demo-1.2.3-k4-x86_64.clos");
        fs::write(&final_package, b"test").unwrap();
        fs::write(
            append_suffix(&final_package, ".minisig"),
            PREHASHED_SIGNATURE,
        )
        .unwrap();
        let config = RepositoryConfig {
            repo_name: "test".to_string(),
            url: "http://127.0.0.1:1".to_string(),
            public_key: PUBLIC_KEY.to_string(),
            fingerprint: String::new(),
            trusted: true,
        };

        let artifact = acquire_artifact(
            &package("1.2.3", 4),
            Architecture::X86_64,
            &config,
            &downloads,
            &temporary.path().join("keys"),
        )
        .unwrap();

        assert_eq!(artifact.path(), final_package);
        assert!(artifact.staged.is_none());
    }

    #[test]
    fn offline_install_runs_end_to_end_and_registers_ownership() {
        let temporary = test_directory("cpsi-install-e2e-");
        let (context, installed_file, pre_marker, post_marker) =
            prepare_offline_install(temporary.path());

        install_with_context(&["demo".to_string()], &context).unwrap();

        assert_eq!(
            fs::read_to_string(&installed_file).unwrap(),
            "offline payload"
        );
        assert_eq!(fs::read_to_string(pre_marker).unwrap(), "pre");
        assert_eq!(fs::read_to_string(post_marker).unwrap(), "post");
        let database = InstalledDatabase::load_from(&context.database_dir).unwrap();
        let installed = database.get_package("demo").unwrap();
        assert_eq!(installed.version, Version::from("1.2.3"));
        assert_eq!(installed.release, 4);
        assert_eq!(database.find_owner(&installed_file), Some("demo"));
    }

    #[test]
    fn file_conflict_stops_before_pre_script_and_copy() {
        let temporary = test_directory("cpsi-install-conflict-");
        let (context, installed_file, pre_marker, post_marker) =
            prepare_offline_install(temporary.path());
        fs::create_dir_all(installed_file.parent().unwrap()).unwrap();
        fs::write(&installed_file, "existing owner payload").unwrap();

        let mut owner = package("1.0.0", 1);
        owner.name = "owner".to_string();
        let mut database = InstalledDatabase::load_from(&context.database_dir).unwrap();
        database.add_package(&owner).unwrap();
        database
            .add_files("owner", std::slice::from_ref(&installed_file))
            .unwrap();
        database.save().unwrap();

        assert!(matches!(
            install_with_context(&["demo".to_string()], &context),
            Err(CpsiError::FileConflict(_, _))
        ));
        assert_eq!(
            fs::read_to_string(&installed_file).unwrap(),
            "existing owner payload"
        );
        assert!(!pre_marker.exists());
        assert!(!post_marker.exists());
        let database = InstalledDatabase::load_from(&context.database_dir).unwrap();
        assert!(database.get_package("demo").is_none());
        assert_eq!(database.find_owner(&installed_file), Some("owner"));
    }

    #[test]
    fn upgrade_removes_files_stale_from_the_new_manifest() {
        let temporary = test_directory("cpsi-upgrade-stale-");
        let (context, installed_file, _, _) = prepare_offline_install(temporary.path());
        let stale_file = context.root.join("usr/share/demo-old");
        fs::create_dir_all(stale_file.parent().unwrap()).unwrap();
        fs::write(&stale_file, "obsolete payload").unwrap();

        let old_package = package("1.0.0", 1);
        let mut database = InstalledDatabase::load_from(&context.database_dir).unwrap();
        database.add_package(&old_package).unwrap();
        database
            .add_files("demo", std::slice::from_ref(&stale_file))
            .unwrap();
        database.save().unwrap();

        install_with_context_allow_upgrade(&["demo".to_string()], &context).unwrap();

        assert!(!stale_file.exists());
        assert!(installed_file.is_file());
        let database = InstalledDatabase::load_from(&context.database_dir).unwrap();
        assert_eq!(database.find_owner(&stale_file), None);
        assert_eq!(database.find_owner(&installed_file), Some("demo"));
        assert_eq!(
            database.get_package("demo").unwrap().version,
            Version::from("1.2.3")
        );
    }

    #[test]
    fn stale_deletion_refuses_paths_outside_the_injected_root() {
        let temporary = test_directory("cpsi-stale-root-");
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let outside_file = outside.join("keep");
        fs::write(&outside_file, "keep").unwrap();

        assert!(matches!(
            remove_stale_files(std::slice::from_ref(&outside_file), &[], &root),
            Err(CpsiError::Database(_))
        ));
        assert_eq!(fs::read_to_string(outside_file).unwrap(), "keep");
    }

    #[cfg(unix)]
    #[test]
    fn stale_deletion_refuses_symlinked_parent_outside_root() {
        let temporary = test_directory("cpsi-stale-symlink-");
        let root = temporary.path().join("root");
        let outside = temporary.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("redirect")).unwrap();
        let outside_file = outside.join("keep");
        fs::write(&outside_file, "keep").unwrap();
        let stale_path = root.join("redirect/keep");

        assert!(matches!(
            remove_stale_files(std::slice::from_ref(&stale_path), &[], &root),
            Err(CpsiError::Database(_))
        ));
        assert_eq!(fs::read_to_string(outside_file).unwrap(), "keep");
    }
}
