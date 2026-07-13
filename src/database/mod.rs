use arrow::datatypes::FieldRef;
use cps_common::{
    architecture::Architecture, errors::CpsiError, package::Package, version::Version,
};
use parquet::arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_arrow::schema::{SchemaLike, TracingOptions};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const DATABASE_DIRECTORY: &str = "/var/lib/cpsi";
const PACKAGES_FILE: &str = "packages.parquet";
const FILES_FILE: &str = "files.parquet";

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledPackage {
    pub name: String,
    pub version: Version,
    pub release: u32,
    pub arch: Vec<Architecture>,
    pub install_time: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledFile {
    pub package: String,
    pub path: String,
}

#[derive(Debug)]
pub struct InstalledDatabase {
    directory: PathBuf,
    packages: Vec<InstalledPackage>,
    files: Vec<InstalledFile>,
}

impl InstalledDatabase {
    pub fn load() -> Result<Self, CpsiError> {
        Self::load_from(DATABASE_DIRECTORY)
    }

    /// Load an installed database from a caller-provided directory.
    ///
    /// This is also the path-injection entry point used by commands and tests
    /// that must not write to the system database under `/var/lib/cpsi`.
    pub fn load_from<P: AsRef<Path>>(directory: P) -> Result<Self, CpsiError> {
        let directory = directory.as_ref().to_path_buf();
        fs::create_dir_all(&directory)?;

        let packages_path = directory.join(PACKAGES_FILE);
        let files_path = directory.join(FILES_FILE);

        let mut packages = read_if_present(&packages_path)?;
        let files = normalize_loaded_files(read_if_present(&files_path)?)?;

        packages.sort_by(|left: &InstalledPackage, right| left.name.cmp(&right.name));
        if let Some(duplicate) = packages
            .windows(2)
            .find(|items| items[0].name == items[1].name)
        {
            return Err(CpsiError::Database(format!(
                "duplicate installed package entry: {}",
                duplicate[0].name
            )));
        }

        Ok(Self {
            directory,
            packages,
            files,
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn packages(&self) -> &[InstalledPackage] {
        &self.packages
    }

    pub fn files(&self) -> &[InstalledFile] {
        &self.files
    }

    pub fn get_package(&self, package_name: &str) -> Option<&InstalledPackage> {
        self.packages
            .iter()
            .find(|package| package.name == package_name)
    }

    pub fn find_package(&self, package_name: &str) -> Option<&InstalledPackage> {
        self.get_package(package_name)
    }

    pub fn files_for_package(&self, package_name: &str) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|file| file.package == package_name)
            .map(|file| PathBuf::from(&file.path))
            .collect()
    }

    pub fn add_package(&mut self, package: &Package) -> Result<(), CpsiError> {
        let installed = InstalledPackage {
            name: package.name.clone(),
            version: package.version.clone(),
            release: package.release,
            arch: package.arch.clone(),
            install_time: current_unix_timestamp()?,
        };

        match self
            .packages
            .iter_mut()
            .find(|existing| existing.name == package.name)
        {
            Some(existing) => *existing = installed,
            None => self.packages.push(installed),
        }
        self.packages
            .sort_by(|left, right| left.name.cmp(&right.name));

        Ok(())
    }

    /// Replace the complete owned-file set for `package_name`.
    ///
    /// Every path is normalized syntactically without following symlinks.
    /// Validation and collision detection happen before the old rows are
    /// removed, so an error leaves the in-memory database unchanged.
    pub fn add_files(&mut self, package_name: &str, files: &[PathBuf]) -> Result<(), CpsiError> {
        let normalized = files
            .iter()
            .map(|path| normalize_path_string(path))
            .collect::<Result<BTreeSet<_>, _>>()?;

        for path in &normalized {
            if let Some(owner) = self.has_file_conflict(Path::new(path), package_name) {
                return Err(CpsiError::FileConflict(path.clone(), owner.to_string()));
            }
        }

        self.files.retain(|file| file.package != package_name);
        self.files
            .extend(normalized.into_iter().map(|path| InstalledFile {
                package: package_name.to_string(),
                path,
            }));
        sort_files(&mut self.files);

        Ok(())
    }

    pub fn remove_package(&mut self, package_name: &str) -> Result<(), CpsiError> {
        self.packages.retain(|package| package.name != package_name);
        self.files.retain(|file| file.package != package_name);
        Ok(())
    }

    pub fn find_owner(&self, path: &Path) -> Option<&str> {
        let normalized = normalize_path_string(path).ok()?;
        self.files
            .iter()
            .find(|file| file.path == normalized)
            .map(|file| file.package.as_str())
    }

    pub fn has_file_conflict(&self, path: &Path, package_name: &str) -> Option<&str> {
        self.find_owner(path).filter(|owner| *owner != package_name)
    }

    pub fn save(&self) -> Result<(), CpsiError> {
        fs::create_dir_all(&self.directory)?;

        let packages_path = self.directory.join(PACKAGES_FILE);
        let files_path = self.directory.join(FILES_FILE);
        let packages_temp = temporary_path(&packages_path)?;
        let files_temp = temporary_path(&files_path)?;

        let result = (|| {
            write_parquet(&packages_temp, &self.packages)?;
            write_parquet(&files_temp, &self.files)?;

            fs::rename(&packages_temp, &packages_path)?;
            fs::rename(&files_temp, &files_path)?;
            sync_directory(&self.directory)?;

            Ok(())
        })();

        // These paths no longer exist following successful renames. Cleanup is
        // still required when serialization, writing, or a rename fails.
        remove_if_present(&packages_temp);
        remove_if_present(&files_temp);

        result
    }
}

fn current_unix_timestamp() -> Result<i64, CpsiError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            CpsiError::Database(format!("system clock is before Unix epoch: {error}"))
        })?
        .as_secs();

    i64::try_from(seconds)
        .map_err(|_| CpsiError::Database("current timestamp does not fit in i64".to_string()))
}

fn read_if_present<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, CpsiError> {
    match File::open(path) {
        Ok(file) => read_parquet(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn read_parquet<T: DeserializeOwned>(file: File) -> Result<Vec<T>, CpsiError> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;
    let mut values = Vec::new();

    for batch in reader {
        let batch = batch.map_err(database_error)?;
        let mut decoded: Vec<T> = serde_arrow::from_record_batch(&batch).map_err(database_error)?;
        values.append(&mut decoded);
    }

    Ok(values)
}

fn write_parquet<T>(path: &Path, values: &[T]) -> Result<(), CpsiError>
where
    T: Serialize + DeserializeOwned,
{
    let options = TracingOptions::default().enums_without_data_as_strings(true);
    let fields = Vec::<FieldRef>::from_type::<T>(options).map_err(database_error)?;
    let batch = serde_arrow::to_record_batch(&fields, &values).map_err(database_error)?;

    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    {
        let mut writer = ArrowWriter::try_new(&mut file, batch.schema(), None)?;
        writer.write(&batch)?;
        writer.close()?;
    }
    file.sync_all()?;

    Ok(())
}

fn normalize_loaded_files(files: Vec<InstalledFile>) -> Result<Vec<InstalledFile>, CpsiError> {
    let mut ownership = BTreeMap::<String, String>::new();

    for file in files {
        if file.package.is_empty() {
            return Err(CpsiError::Database(
                "installed file entry has an empty package name".to_string(),
            ));
        }

        let path = normalize_path_string(Path::new(&file.path))?;
        if let Some(owner) = ownership.get(&path) {
            if owner != &file.package {
                return Err(CpsiError::FileConflict(path, owner.clone()));
            }
            continue;
        }
        ownership.insert(path, file.package);
    }

    let mut files = ownership
        .into_iter()
        .map(|(path, package)| InstalledFile { package, path })
        .collect::<Vec<_>>();
    sort_files(&mut files);
    Ok(files)
}

fn normalize_path_string(path: &Path) -> Result<String, CpsiError> {
    if !path.is_absolute() {
        return Err(CpsiError::Database(format!(
            "installed file path must be absolute: {}",
            path.display()
        )));
    }

    let mut normalized = PathBuf::from("/");
    let mut depth = 0_usize;

    for component in path.components() {
        match component {
            Component::Prefix(_) => {
                return Err(CpsiError::Database(format!(
                    "unsupported installed file path: {}",
                    path.display()
                )));
            }
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return Err(CpsiError::Database(format!(
                        "installed file path escapes root: {}",
                        path.display()
                    )));
                }
                normalized.pop();
                depth -= 1;
            }
            Component::Normal(component) => {
                normalized.push(component);
                depth += 1;
            }
        }
    }

    if depth == 0 {
        return Err(CpsiError::Database(
            "the filesystem root cannot be owned by a package".to_string(),
        ));
    }

    normalized.into_os_string().into_string().map_err(|path| {
        CpsiError::Database(format!(
            "installed file path is not valid UTF-8: {}",
            PathBuf::from(path).display()
        ))
    })
}

fn sort_files(files: &mut [InstalledFile]) {
    files.sort_by(|left, right| {
        left.package
            .cmp(&right.package)
            .then_with(|| left.path.cmp(&right.path))
    });
}

fn temporary_path(destination: &Path) -> Result<PathBuf, CpsiError> {
    let parent = destination.parent().ok_or_else(|| {
        CpsiError::Database(format!(
            "database path has no parent: {}",
            destination.display()
        ))
    })?;
    let file_name = destination.file_name().ok_or_else(|| {
        CpsiError::Database(format!(
            "database path has no file name: {}",
            destination.display()
        ))
    })?;
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);

    Ok(parent.join(format!(
        ".{}.tmp-{}-{sequence}",
        file_name.to_string_lossy(),
        std::process::id()
    )))
}

fn sync_directory(directory: &Path) -> Result<(), CpsiError> {
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn remove_if_present(path: &Path) {
    if let Err(error) = fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!(
            "warning: failed to remove temporary database file {}: {error}",
            path.display()
        );
    }
}

fn database_error(error: impl std::fmt::Display) -> CpsiError {
    CpsiError::Database(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cps_common::dependency::Dependency;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let sequence = TEST_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join("opencode").join(format!(
                "cpsi-database-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn package(name: &str, version: &str, release: u32) -> Package {
        Package {
            name: name.to_string(),
            version: Version::from(version),
            release,
            arch: vec![Architecture::X86_64],
            dependencies: Vec::<Dependency>::new(),
            description: format!("{name} package"),
            provides: Vec::new(),
            license: "MIT".to_string(),
            package_size: 10,
            installed_size: 20,
            repository: "test".to_string(),
        }
    }

    #[test]
    fn missing_database_is_empty_and_empty_parquet_round_trips() {
        let temp = TestDirectory::new();
        let database_dir = temp.path().join("database");

        let database = InstalledDatabase::load_from(&database_dir).unwrap();
        assert!(database_dir.is_dir());
        assert!(database.packages().is_empty());
        assert!(database.files().is_empty());

        database.save().unwrap();
        assert!(database_dir.join(PACKAGES_FILE).is_file());
        assert!(database_dir.join(FILES_FILE).is_file());

        let reloaded = InstalledDatabase::load_from(database_dir).unwrap();
        assert!(reloaded.packages().is_empty());
        assert!(reloaded.files().is_empty());
    }

    #[test]
    fn parquet_round_trip_preserves_packages_architectures_and_files() {
        let temp = TestDirectory::new();
        let mut database = InstalledDatabase::load_from(temp.path()).unwrap();
        let mut pkg = package("demo", "1.2.3", 4);
        pkg.arch.push(Architecture::Aarch64);

        database.add_package(&pkg).unwrap();
        database
            .add_files(
                "demo",
                &[
                    PathBuf::from("/usr/bin/demo"),
                    PathBuf::from("/usr/lib/../share/demo/data"),
                ],
            )
            .unwrap();
        database.save().unwrap();

        let reloaded = InstalledDatabase::load_from(temp.path()).unwrap();
        let installed = reloaded.get_package("demo").unwrap();
        assert_eq!(installed.version, Version::from("1.2.3"));
        assert_eq!(installed.release, 4);
        assert_eq!(
            installed.arch,
            vec![Architecture::X86_64, Architecture::Aarch64]
        );
        assert_eq!(
            reloaded.files_for_package("demo"),
            vec![
                PathBuf::from("/usr/bin/demo"),
                PathBuf::from("/usr/share/demo/data")
            ]
        );
        assert_eq!(
            reloaded.find_owner(Path::new("/usr/bin/demo")),
            Some("demo")
        );
    }

    #[test]
    fn adding_a_package_and_files_replaces_previous_rows() {
        let temp = TestDirectory::new();
        let mut database = InstalledDatabase::load_from(temp.path()).unwrap();

        database.add_package(&package("demo", "1.0.0", 1)).unwrap();
        database
            .add_files("demo", &[PathBuf::from("/usr/bin/old")])
            .unwrap();
        database.add_package(&package("demo", "2.0.0", 2)).unwrap();
        database
            .add_files("demo", &[PathBuf::from("/usr/bin/new")])
            .unwrap();

        assert_eq!(database.packages().len(), 1);
        assert_eq!(
            database.get_package("demo").unwrap().version,
            Version::from("2.0.0")
        );
        assert_eq!(
            database.files_for_package("demo"),
            vec![PathBuf::from("/usr/bin/new")]
        );
    }

    #[test]
    fn detects_conflicts_after_normalization_without_losing_old_rows() {
        let temp = TestDirectory::new();
        let mut database = InstalledDatabase::load_from(temp.path()).unwrap();
        database
            .add_files("first", &[PathBuf::from("/usr/bin/tool")])
            .unwrap();

        assert_eq!(
            database.has_file_conflict(Path::new("/usr/lib/../bin/tool"), "second"),
            Some("first")
        );
        assert_eq!(
            database.has_file_conflict(Path::new("/usr/bin/tool"), "first"),
            None
        );

        let error = database
            .add_files("second", &[PathBuf::from("/usr/lib/../bin/tool")])
            .unwrap_err();
        assert!(matches!(error, CpsiError::FileConflict(_, _)));
        assert_eq!(
            database.files_for_package("first"),
            vec![PathBuf::from("/usr/bin/tool")]
        );
        assert!(database.files_for_package("second").is_empty());
    }

    #[test]
    fn remove_package_removes_only_its_owned_files() {
        let temp = TestDirectory::new();
        let mut database = InstalledDatabase::load_from(temp.path()).unwrap();
        database.add_package(&package("one", "1.0.0", 1)).unwrap();
        database.add_package(&package("two", "1.0.0", 1)).unwrap();
        database
            .add_files("one", &[PathBuf::from("/usr/bin/one")])
            .unwrap();
        database
            .add_files("two", &[PathBuf::from("/usr/bin/two")])
            .unwrap();

        database.remove_package("one").unwrap();

        assert!(database.get_package("one").is_none());
        assert!(database.files_for_package("one").is_empty());
        assert!(database.get_package("two").is_some());
        assert_eq!(database.find_owner(Path::new("/usr/bin/two")), Some("two"));
    }

    #[test]
    fn rejects_relative_root_and_root_escaping_paths() {
        let temp = TestDirectory::new();
        let mut database = InstalledDatabase::load_from(temp.path()).unwrap();

        for path in ["usr/bin/tool", "/", "/../usr/bin/tool"] {
            assert!(
                database.add_files("demo", &[PathBuf::from(path)]).is_err(),
                "{path} should be rejected"
            );
        }
        assert!(database.files().is_empty());
    }
}
