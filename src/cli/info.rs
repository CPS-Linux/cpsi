use crate::{database::InstalledDatabase, repository::parquet::Repository, util::constants};
use cps_common::{
    architecture::Architecture, dependency::Dependency, errors::CpsiError, package::Package,
    version::Version,
};
use std::{io::ErrorKind, path::PathBuf};

#[derive(Clone, Debug)]
pub struct InfoContext {
    pub database_dir: PathBuf,
    pub repositories_dir: PathBuf,
    pub repositories_config_dir: PathBuf,
    pub keys_dir: PathBuf,
    verify_repository_signatures: bool,
}

impl InfoContext {
    pub fn new(database_dir: impl Into<PathBuf>, repositories_dir: impl Into<PathBuf>) -> Self {
        Self {
            database_dir: database_dir.into(),
            repositories_dir: repositories_dir.into(),
            repositories_config_dir: PathBuf::new(),
            keys_dir: PathBuf::new(),
            verify_repository_signatures: false,
        }
    }

    pub fn with_verified_repositories(
        database_dir: impl Into<PathBuf>,
        repositories_dir: impl Into<PathBuf>,
        repositories_config_dir: impl Into<PathBuf>,
        keys_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            database_dir: database_dir.into(),
            repositories_dir: repositories_dir.into(),
            repositories_config_dir: repositories_config_dir.into(),
            keys_dir: keys_dir.into(),
            verify_repository_signatures: true,
        }
    }
}

impl Default for InfoContext {
    fn default() -> Self {
        Self {
            database_dir: PathBuf::from(constants::INSTALLED_DATABASE_DIRECTORY),
            repositories_dir: PathBuf::from(constants::REPOSITORIES_DIRECTORY),
            repositories_config_dir: PathBuf::from(constants::REPOSITORIES_CONFIG_DIRECTORY),
            keys_dir: PathBuf::from(constants::PUBLIC_KEYS_DIRECTORY),
            verify_repository_signatures: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageDetails {
    pub name: String,
    pub version: Version,
    pub release: u32,
    pub arch: Vec<Architecture>,
    pub description: String,
    pub license: String,
    pub package_size: u64,
    pub installed_size: u64,
    pub dependencies: Vec<Dependency>,
    pub provides: Vec<String>,
    pub installed: bool,
}

/// Print package information using CPSI's system database and repository cache.
pub fn info(package_name: &str) -> Result<(), CpsiError> {
    let details = info_with_context(package_name, &InfoContext::default())?;
    print!("{}", render_package_info(&details));
    Ok(())
}

/// Resolve package information using caller-provided database and repository paths.
pub fn info_with_context(
    package_name: &str,
    context: &InfoContext,
) -> Result<PackageDetails, CpsiError> {
    let database = InstalledDatabase::load_from(&context.database_dir)?;
    let repository = load_optional_repository(context)?;

    package_details_from_sources(
        package_name,
        database.get_package(package_name),
        repository
            .as_ref()
            .and_then(|repository| repository.find_package(package_name)),
    )
}

/// Merge installed state with optional repository metadata.
///
/// Installed version, release, and architecture are authoritative. Repository
/// metadata supplies descriptive fields that are intentionally not duplicated
/// in the installed database.
pub fn package_details_from_sources(
    package_name: &str,
    installed: Option<&crate::database::InstalledPackage>,
    repository_package: Option<&Package>,
) -> Result<PackageDetails, CpsiError> {
    match (installed, repository_package) {
        (Some(installed), metadata) => Ok(PackageDetails {
            name: installed.name.clone(),
            version: installed.version.clone(),
            release: installed.release,
            arch: installed.arch.clone(),
            description: metadata
                .map(|package| package.description.clone())
                .unwrap_or_default(),
            license: metadata
                .map(|package| package.license.clone())
                .unwrap_or_default(),
            package_size: metadata.map(|package| package.package_size).unwrap_or(0),
            installed_size: metadata.map(|package| package.installed_size).unwrap_or(0),
            dependencies: metadata
                .map(|package| package.dependencies.clone())
                .unwrap_or_default(),
            provides: metadata
                .map(|package| package.provides.clone())
                .unwrap_or_default(),
            installed: true,
        }),
        (None, Some(package)) => Ok(PackageDetails {
            name: package.name.clone(),
            version: package.version.clone(),
            release: package.release,
            arch: package.arch.clone(),
            description: package.description.clone(),
            license: package.license.clone(),
            package_size: package.package_size,
            installed_size: package.installed_size,
            dependencies: package.dependencies.clone(),
            provides: package.provides.clone(),
            installed: false,
        }),
        (None, None) => Err(CpsiError::PackageNotFound(package_name.to_string())),
    }
}

/// Produce the stable human-readable representation used by `cpsi info`.
pub fn render_package_info(details: &PackageDetails) -> String {
    let architecture = join_display(&details.arch);
    let dependencies = join_display(&details.dependencies);
    let provides = join_strings(&details.provides);
    let description = nonempty_or_unknown(&details.description);
    let license = nonempty_or_unknown(&details.license);

    format!(
        "Name: {}\nVersion: {}\nRelease: {}\nArchitecture: {}\nInstalled: {}\nDescription: {}\nLicense: {}\nPackage Size: {} B\nInstalled Size: {} B\nDepends: {}\nProvides: {}\n",
        details.name,
        details.version,
        details.release,
        architecture,
        if details.installed { "yes" } else { "no" },
        description,
        license,
        details.package_size,
        details.installed_size,
        dependencies,
        provides,
    )
}

fn load_optional_repository(context: &InfoContext) -> Result<Option<Repository>, CpsiError> {
    let result = if context.verify_repository_signatures {
        Repository::load_registered_from(
            &context.repositories_config_dir,
            &context.keys_dir,
            &context.repositories_dir,
        )
    } else {
        Repository::load_from(&context.repositories_dir)
    };

    match result {
        Ok(repository) => Ok(Some(repository)),
        Err(CpsiError::NoRepositories) => Ok(None),
        Err(CpsiError::Io(error)) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn join_display<T: std::fmt::Display>(values: &[T]) -> String {
    if values.is_empty() {
        "None".to_string()
    } else {
        values
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn join_strings(values: &[String]) -> String {
    if values.is_empty() {
        "None".to_string()
    } else {
        values.join(", ")
    }
}

fn nonempty_or_unknown(value: &str) -> &str {
    if value.is_empty() { "Unknown" } else { value }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::FieldRef;
    use parquet::arrow::ArrowWriter;
    use serde::Serialize;
    use serde_arrow::schema::{SchemaLike, TracingOptions};
    use std::{
        fs::{self, File},
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join("opencode").join(format!(
                "cpsi-info-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn package(name: &str, version: &str) -> Package {
        Package {
            name: name.to_string(),
            version: Version::from(version),
            release: 2,
            arch: vec![Architecture::host().unwrap()],
            dependencies: vec!["libc>=1.0.0".parse().unwrap()],
            description: format!("{name} description"),
            provides: vec![format!("virtual-{name}")],
            license: "MIT".to_string(),
            package_size: 100,
            installed_size: 200,
            repository: "core".to_string(),
        }
    }

    fn write_repository(directory: &Path, packages: &[Package]) {
        #[derive(Serialize)]
        struct DependencyRow {
            name: String,
            version: Option<Version>,
            operator: Option<String>,
        }

        #[derive(Serialize)]
        struct PackageRow {
            name: String,
            version: Version,
            release: u32,
            arch: Vec<Architecture>,
            dependencies: Vec<DependencyRow>,
            description: String,
            provides: Vec<String>,
            license: String,
            package_size: u64,
            installed_size: u64,
            repository: String,
        }

        fs::create_dir_all(directory).unwrap();
        let options = TracingOptions::default()
            .enums_without_data_as_strings(true)
            .allow_null_fields(true);
        let rows = packages
            .iter()
            .map(|package| PackageRow {
                name: package.name.clone(),
                version: package.version.clone(),
                release: package.release,
                arch: package.arch.clone(),
                dependencies: package
                    .dependencies
                    .iter()
                    .map(|dependency| DependencyRow {
                        name: dependency.name.clone(),
                        version: dependency.version.clone(),
                        operator: dependency.operator.map(|operator| operator.to_string()),
                    })
                    .collect(),
                description: package.description.clone(),
                provides: package.provides.clone(),
                license: package.license.clone(),
                package_size: package.package_size,
                installed_size: package.installed_size,
                repository: package.repository.clone(),
            })
            .collect::<Vec<_>>();
        let fields = Vec::<FieldRef>::from_samples(&rows, options).unwrap();
        let batch = serde_arrow::to_record_batch(&fields, &rows).unwrap();
        let file = File::create(directory.join("core.parquet")).unwrap();
        let mut writer = ArrowWriter::try_new(file, batch.schema(), None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    #[test]
    fn installed_state_wins_and_repository_metadata_supplements_it() {
        let temp = TestDirectory::new("installed");
        let database_dir = temp.0.join("database");
        let repositories_dir = temp.0.join("repositories");
        let installed_package = package("demo", "1.0.0");
        let repository_package = package("demo", "2.0.0");

        let mut database = InstalledDatabase::load_from(&database_dir).unwrap();
        database.add_package(&installed_package).unwrap();
        database.save().unwrap();
        write_repository(&repositories_dir, &[repository_package]);

        let details =
            info_with_context("demo", &InfoContext::new(&database_dir, &repositories_dir)).unwrap();
        assert!(details.installed);
        assert_eq!(details.version, Version::from("1.0.0"));
        assert_eq!(details.description, "demo description");
        assert_eq!(details.dependencies[0].to_string(), "libc>=1.0.0");
    }

    #[test]
    fn falls_back_to_repository_for_uninstalled_package() {
        let temp = TestDirectory::new("repository");
        let database_dir = temp.0.join("database");
        let repositories_dir = temp.0.join("repositories");
        write_repository(&repositories_dir, &[package("demo", "2.0.0")]);

        let details =
            info_with_context("demo", &InfoContext::new(&database_dir, &repositories_dir)).unwrap();
        assert!(!details.installed);
        assert_eq!(details.version, Version::from("2.0.0"));
    }

    #[test]
    fn reports_missing_package() {
        let temp = TestDirectory::new("missing");
        let result = info_with_context(
            "missing",
            &InfoContext::new(temp.0.join("database"), temp.0.join("repositories")),
        );

        assert!(matches!(
            result,
            Err(CpsiError::PackageNotFound(name)) if name == "missing"
        ));
    }

    #[test]
    fn renders_all_requested_fields() {
        let package = package("demo", "1.0.0");
        let details = package_details_from_sources("demo", None, Some(&package)).unwrap();
        let rendered = render_package_info(&details);
        for field in [
            "Name: demo",
            "Version: 1.0.0",
            "Release: 2",
            "Architecture:",
            "Description: demo description",
            "License: MIT",
            "Package Size: 100 B",
            "Installed Size: 200 B",
            "Depends: libc>=1.0.0",
            "Provides: virtual-demo",
        ] {
            assert!(
                rendered.contains(field),
                "missing {field:?} in {rendered:?}"
            );
        }
    }
}
