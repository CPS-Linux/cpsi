use crate::{
    repository::sync::{RepositoryConfig, repository_index_path},
    signature::{load_public_key_from, verify_packages_parquet},
    util::constants,
};
use cps_common::{
    architecture::Architecture, errors::CpsiError, package::Package,
    repository::RepositoryParquetFormat,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Repository {
    packages: HashMap<String, Package>,
    architecture: Architecture,
}

impl Repository {
    pub fn load() -> Result<Self, CpsiError> {
        Self::load_registered_from(
            constants::REPOSITORIES_CONFIG_DIRECTORY,
            constants::PUBLIC_KEYS_DIRECTORY,
            constants::REPOSITORIES_DIRECTORY,
        )
    }

    /// Load only configured repository caches. Trusted indexes are verified
    /// again here so a modified cache cannot bypass `cpsi update`.
    pub fn load_registered_from(
        config_dir: impl AsRef<Path>,
        keys_dir: impl AsRef<Path>,
        cache_dir: impl AsRef<Path>,
    ) -> Result<Self, CpsiError> {
        let architecture = Architecture::host()?;
        let repositories = RepositoryConfig::load_repositories_from(config_dir.as_ref())?;
        let mut sources = Vec::new();

        for repository in repositories {
            let index_path = repository_index_path(cache_dir.as_ref(), &repository.repo_name)?;
            if !index_path.is_file() {
                continue;
            }

            if repository.trusted {
                let public_key = match load_public_key_from(
                    &repository.repo_name,
                    keys_dir.as_ref(),
                ) {
                    Ok(key) => {
                        if key != repository.public_key {
                            return Err(CpsiError::SignatureVerificationFailed(format!(
                                "stored key for repository '{}' does not match its configuration",
                                repository.repo_name
                            )));
                        }
                        key
                    }
                    Err(CpsiError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        repository.public_key.clone()
                    }
                    Err(error) => return Err(error),
                };
                verify_packages_parquet(&index_path, &public_key)?;
            } else {
                eprintln!(
                    "warning: using unverified cache from repository '{}'",
                    repository.repo_name
                );
            }

            sources.push((index_path, repository.repo_name));
        }

        if sources.is_empty() {
            return Err(CpsiError::NoRepositories);
        }
        Self::load_sources_for_arch(sources, architecture)
    }

    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, CpsiError> {
        Self::load_from_for_arch(path, Architecture::host()?)
    }

    pub fn load_from_for_arch(
        path: impl AsRef<Path>,
        architecture: Architecture,
    ) -> Result<Self, CpsiError> {
        let parquet_files = find_all_parquet(path.as_ref())?;
        if parquet_files.is_empty() {
            return Err(CpsiError::NoRepositories);
        }

        let sources = parquet_files
            .into_iter()
            .map(|file| {
                let repository_name = file
                    .file_stem()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| {
                        CpsiError::InvalidPackage(format!(
                            "invalid repository cache name: {}",
                            file.display()
                        ))
                    })?
                    .to_string();
                Ok((file, repository_name))
            })
            .collect::<Result<Vec<_>, CpsiError>>()?;

        Self::load_sources_for_arch(sources, architecture)
    }

    fn load_sources_for_arch(
        sources: Vec<(PathBuf, String)>,
        architecture: Architecture,
    ) -> Result<Self, CpsiError> {
        let mut packages = Vec::new();
        for (file, repository_name) in sources {
            let mut loaded = load_packages(&file)?;
            for package in &mut loaded {
                // The cache file that was selected by local configuration is the
                // authority for the source repository, not untrusted index data.
                package.repository = repository_name.clone();
            }
            packages.extend(loaded);
        }

        Self::from_packages_for_arch(packages, architecture)
    }

    pub fn from_packages(packages: Vec<Package>) -> Result<Self, CpsiError> {
        Self::from_packages_for_arch(packages, Architecture::host()?)
    }

    pub fn from_packages_for_arch(
        packages: Vec<Package>,
        architecture: Architecture,
    ) -> Result<Self, CpsiError> {
        let mut compatible = HashMap::new();

        for package in packages
            .into_iter()
            .filter(|package| supports_architecture(package, architecture))
        {
            if compatible.contains_key(&package.name) {
                return Err(CpsiError::DuplicatePackage(package.name));
            }
            compatible.insert(package.name.clone(), package);
        }

        Ok(Self {
            packages: compatible,
            architecture,
        })
    }

    pub fn find_package<T: AsRef<str>>(&self, package: T) -> Option<&Package> {
        self.packages.get(package.as_ref())
    }

    pub fn packages(&self) -> impl Iterator<Item = &Package> {
        self.packages.values()
    }

    pub fn architecture(&self) -> Architecture {
        self.architecture
    }
}

pub fn supports_architecture(package: &Package, architecture: Architecture) -> bool {
    package.arch.is_empty() || package.arch.contains(&architecture)
}

fn find_all_parquet(dir: &Path) -> Result<Vec<PathBuf>, io::Error> {
    let mut list = Vec::new();

    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "parquet")
        {
            list.push(entry.path());
        }
    }
    list.sort();

    Ok(list)
}

fn load_packages(file: &Path) -> Result<RepositoryParquetFormat, CpsiError> {
    let file = fs::File::open(file)?;
    let mut packages: RepositoryParquetFormat = Vec::new();

    let reader =
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)?.build()?;

    for batch in reader {
        let batch = batch.map_err(|error| CpsiError::InvalidPackage(error.to_string()))?;
        let mut loaded: RepositoryParquetFormat = serde_arrow::from_record_batch(&batch)
            .map_err(|error| CpsiError::InvalidPackage(error.to_string()))?;
        packages.append(&mut loaded);
    }

    Ok(packages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cps_common::{dependency::Dependency, version::Version};

    fn package(name: &str, arch: Architecture) -> Package {
        Package {
            name: name.to_string(),
            version: Version::from("1.0.0"),
            release: 1,
            arch: vec![arch],
            dependencies: Vec::<Dependency>::new(),
            description: String::new(),
            provides: Vec::new(),
            license: String::new(),
            package_size: 0,
            installed_size: 0,
            repository: "test".to_string(),
        }
    }

    #[test]
    fn filters_before_duplicate_detection() {
        let repository = Repository::from_packages_for_arch(
            vec![
                package("demo", Architecture::X86_64),
                package("demo", Architecture::Aarch64),
            ],
            Architecture::X86_64,
        )
        .unwrap();

        assert_eq!(
            repository.find_package("demo").unwrap().arch,
            vec![Architecture::X86_64]
        );
    }

    #[test]
    fn excludes_incompatible_packages() {
        let repository = Repository::from_packages_for_arch(
            vec![package("demo", Architecture::Aarch64)],
            Architecture::X86_64,
        )
        .unwrap();

        assert!(repository.find_package("demo").is_none());
    }
}
