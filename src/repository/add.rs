use crate::{
    repository::validate_repository_name,
    signature::{public_key_path, save_public_key_to, validate_public_key},
    util::constants,
    util::net::{self, Download},
};
use cps_common::errors::CpsiError;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static METADATA_DOWNLOAD_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize, Deserialize)]
struct RepositoryMetadata {
    repo_name: String,
    public_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddTargetRepository {
    pub url: String,
    pub repo_name: String,
    pub public_key: String,
    pub fingerprint: String,
    pub trusted: bool,
}

impl AddTargetRepository {
    /// Download and validate `{url}/repository.json` for a trusted repository.
    pub fn new(url: &str) -> Result<Self, CpsiError> {
        Self::new_with_trust(url, true)
    }

    /// Download and validate `{url}/repository.json` with an explicit trust state.
    pub fn new_with_trust(url: &str, trusted: bool) -> Result<Self, CpsiError> {
        let data_file_loc = metadata_download_path();
        let metadata_url = format!("{}/repository.json", url.trim_end_matches('/'));
        let download = Download::new(metadata_url, &data_file_loc);

        if let Err(error) = net::download_file(download) {
            let _ = fs::remove_file(&data_file_loc);
            return Err(CpsiError::NetError(error.to_string()));
        }

        let result = fs::read_to_string(&data_file_loc)
            .map_err(CpsiError::from)
            .and_then(|data| Self::from_metadata_json(url, &data, trusted));
        let _ = fs::remove_file(data_file_loc);
        result
    }

    /// Build a repository configuration from already downloaded metadata.
    ///
    /// Keeping parsing separate from network access makes repository enrollment
    /// testable without touching CPSI's system directories or the network.
    pub fn from_metadata_json(url: &str, data: &str, trusted: bool) -> Result<Self, CpsiError> {
        let repository: RepositoryMetadata = serde_json::from_str(data).map_err(|error| {
            CpsiError::NetError(format!("invalid repository metadata: {error}"))
        })?;
        validate_repository_name(&repository.repo_name)?;

        let public_key = repository.public_key.trim().to_string();
        validate_public_key(&public_key)?;
        let fingerprint = sha256::digest(&public_key);

        Ok(Self {
            url: url.trim_end_matches('/').to_string(),
            repo_name: repository.repo_name,
            public_key,
            fingerprint,
            trusted,
        })
    }

    /// Save the repository configuration and public key to CPSI's system paths.
    pub fn save(&self) -> Result<(), CpsiError> {
        self.save_to(
            Path::new(constants::REPOSITORIES_CONFIG_DIRECTORY),
            Path::new(constants::PUBLIC_KEYS_DIRECTORY),
        )
    }

    /// Save the repository configuration and public key under explicit paths.
    pub fn save_to(&self, config_dir: &Path, keys_dir: &Path) -> Result<(), CpsiError> {
        validate_repository_name(&self.repo_name)?;
        validate_public_key(&self.public_key)?;
        fs::create_dir_all(config_dir)?;

        let config_path = repository_config_path(config_dir, &self.repo_name)?;
        if config_path.exists() {
            return Err(CpsiError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("repository already exists: {}", self.repo_name),
            )));
        }

        let toml = toml::to_string(self).map_err(|error| CpsiError::Toml(error.to_string()))?;
        save_public_key_to(&self.repo_name, &self.public_key, keys_dir)?;
        let key_path = public_key_path(keys_dir, &self.repo_name)?;

        let save_result = (|| -> Result<(), CpsiError> {
            let mut writer = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&config_path)?;
            writer.write_all(toml.as_bytes())?;
            writer.flush()?;
            Ok(())
        })();

        if let Err(error) = save_result {
            let _ = fs::remove_file(&config_path);
            let _ = fs::remove_file(key_path);
            return Err(error);
        }

        Ok(())
    }
}

pub fn repository_config_path(config_dir: &Path, repo_name: &str) -> Result<PathBuf, CpsiError> {
    validate_repository_name(repo_name)?;
    Ok(config_dir.join(format!("{repo_name}.toml")))
}

fn metadata_download_path() -> PathBuf {
    let sequence = METADATA_DOWNLOAD_COUNTER.fetch_add(1, Ordering::Relaxed);
    Path::new(constants::TEMP_DOWNLOAD_LOCATION).join(format!(
        ".repository-{}-{sequence}.json",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{repository::sync::RepositoryConfig, signature::load_public_key_from};
    use std::sync::atomic::{AtomicU64, Ordering};

    const PUBLIC_KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_directory(label: &str) -> PathBuf {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cpsi-repository-add-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn parses_and_validates_repository_metadata() {
        let data = format!(r#"{{"repo_name":"core","public_key":"{PUBLIC_KEY}"}}"#);
        let repository =
            AddTargetRepository::from_metadata_json("https://example.test/", &data, true).unwrap();

        assert_eq!(repository.repo_name, "core");
        assert_eq!(repository.url, "https://example.test");
        assert!(repository.trusted);
        assert_eq!(repository.fingerprint, sha256::digest(PUBLIC_KEY));
    }

    #[test]
    fn rejects_unsafe_remote_repository_name() {
        let data = format!(r#"{{"repo_name":"../outside","public_key":"{PUBLIC_KEY}"}}"#);
        assert!(
            AddTargetRepository::from_metadata_json("https://example.test", &data, true).is_err()
        );
    }

    #[test]
    fn saves_config_and_key_to_explicit_directories() {
        let root = temp_directory("save");
        let config_dir = root.join("repos.d");
        let keys_dir = root.join("keys");
        let data = format!(r#"{{"repo_name":"core","public_key":"{PUBLIC_KEY}"}}"#);
        let repository =
            AddTargetRepository::from_metadata_json("https://example.test", &data, false).unwrap();

        repository.save_to(&config_dir, &keys_dir).unwrap();
        let loaded = RepositoryConfig::load_repositories_from(&config_dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(!loaded[0].trusted);
        assert_eq!(load_public_key_from("core", &keys_dir).unwrap(), PUBLIC_KEY);

        fs::remove_dir_all(root).unwrap();
    }
}
