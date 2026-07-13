use crate::{
    repository::validate_repository_name,
    signature::{load_public_key_from, validate_public_key, verify_file_with_sig},
    util::{
        constants,
        net::{self, Download},
    },
};
use cps_common::errors::CpsiError;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    ffi::OsString,
    fs,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepositoryConfig {
    pub repo_name: String,
    pub url: String,
    pub public_key: String,
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default = "trusted_by_default")]
    pub trusted: bool,
}

impl RepositoryConfig {
    pub fn load_repositories() -> Result<Vec<Self>, CpsiError> {
        Self::load_repositories_from(Path::new(constants::REPOSITORIES_CONFIG_DIRECTORY))
    }

    /// Load repository configurations from an explicit directory.
    pub fn load_repositories_from(config_dir: &Path) -> Result<Vec<Self>, CpsiError> {
        let read_dir = match fs::read_dir(config_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Err(CpsiError::NoRepositories);
            }
            Err(error) => return Err(CpsiError::Io(error)),
        };

        let mut config_paths = Vec::new();
        for entry in read_dir {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "toml")
            {
                config_paths.push(path);
            }
        }
        config_paths.sort();

        let mut repositories = Vec::with_capacity(config_paths.len());
        let mut names = HashSet::new();
        for path in config_paths {
            let file_content = fs::read_to_string(&path)?;
            let mut repository: Self = toml::from_str(&file_content)
                .map_err(|error| CpsiError::Toml(error.to_string()))?;
            repository.normalize_and_validate()?;

            if !names.insert(repository.repo_name.clone()) {
                return Err(CpsiError::Io(io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "duplicate repository configuration: {}",
                        repository.repo_name
                    ),
                )));
            }

            if !repository.trusted {
                eprintln!(
                    "warning: repository '{}' is untrusted; signature verification will be skipped",
                    repository.repo_name
                );
            }
            repositories.push(repository);
        }

        repositories.sort_by(|left, right| left.repo_name.cmp(&right.repo_name));
        Ok(repositories)
    }

    pub fn find(repo_name: &str) -> Result<Self, CpsiError> {
        Self::find_in(
            repo_name,
            Path::new(constants::REPOSITORIES_CONFIG_DIRECTORY),
        )
    }

    /// Find a repository configuration in an explicit directory.
    pub fn find_in(repo_name: &str, config_dir: &Path) -> Result<Self, CpsiError> {
        validate_repository_name(repo_name)?;
        Self::load_repositories_from(config_dir)?
            .into_iter()
            .find(|repository| repository.repo_name == repo_name)
            .ok_or_else(|| CpsiError::RepositoryNotFound(repo_name.to_string()))
    }

    fn normalize_and_validate(&mut self) -> Result<(), CpsiError> {
        validate_repository_name(&self.repo_name)?;
        self.url = self.url.trim_end_matches('/').to_string();
        self.public_key = self.public_key.trim().to_string();
        validate_public_key(&self.public_key)?;
        if self.fingerprint.is_empty() {
            self.fingerprint = sha256::digest(&self.public_key);
        }
        Ok(())
    }
}

pub async fn sync() -> Result<(), CpsiError> {
    sync_with_paths(
        Path::new(constants::REPOSITORIES_CONFIG_DIRECTORY),
        Path::new(constants::PUBLIC_KEYS_DIRECTORY),
        Path::new(constants::REPOSITORIES_DIRECTORY),
    )
    .await
}

/// Synchronize every configured repository whose name starts with `prefix`.
pub async fn sync_prefix(prefix: &str) -> Result<(), CpsiError> {
    sync_prefix_with_paths(
        prefix,
        Path::new(constants::REPOSITORIES_CONFIG_DIRECTORY),
        Path::new(constants::PUBLIC_KEYS_DIRECTORY),
        Path::new(constants::REPOSITORIES_DIRECTORY),
    )
    .await
}

/// Synchronize repositories using explicit configuration, key, and cache paths.
pub async fn sync_with_paths(
    config_dir: &Path,
    keys_dir: &Path,
    cache_dir: &Path,
) -> Result<(), CpsiError> {
    let repositories = RepositoryConfig::load_repositories_from(config_dir)?;
    sync_repositories_to(repositories, keys_dir, cache_dir).await
}

/// Synchronize repositories matching `prefix` using explicit paths.
pub async fn sync_prefix_with_paths(
    prefix: &str,
    config_dir: &Path,
    keys_dir: &Path,
    cache_dir: &Path,
) -> Result<(), CpsiError> {
    let repositories = RepositoryConfig::load_repositories_from(config_dir)?;
    let repositories = select_repositories_by_prefix(repositories, prefix)?;
    sync_repositories_to(repositories, keys_dir, cache_dir).await
}

fn select_repositories_by_prefix(
    repositories: Vec<RepositoryConfig>,
    prefix: &str,
) -> Result<Vec<RepositoryConfig>, CpsiError> {
    validate_repository_prefix(prefix)?;

    let repositories: Vec<_> = repositories
        .into_iter()
        .filter(|repository| repository.repo_name.starts_with(prefix))
        .collect();

    if repositories.is_empty() {
        Err(CpsiError::RepositoryNotFound(prefix.to_string()))
    } else {
        Ok(repositories)
    }
}

fn validate_repository_prefix(prefix: &str) -> Result<(), CpsiError> {
    let valid = !prefix.is_empty()
        && prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));

    if valid {
        Ok(())
    } else {
        Err(CpsiError::Io(io::Error::new(
            ErrorKind::InvalidInput,
            format!("invalid repository prefix: {prefix}"),
        )))
    }
}

/// Synchronize an already loaded set of repository configurations.
pub async fn sync_repositories_to(
    repositories: Vec<RepositoryConfig>,
    keys_dir: &Path,
    cache_dir: &Path,
) -> Result<(), CpsiError> {
    if repositories.is_empty() {
        return Err(CpsiError::NoRepositories);
    }
    fs::create_dir_all(cache_dir)?;

    let mut staged_repositories = Vec::with_capacity(repositories.len());
    let mut downloads = Vec::with_capacity(repositories.len() * 2);

    for mut repository in repositories {
        repository.normalize_and_validate()?;
        let public_key = if repository.trusted {
            Some(resolve_trusted_public_key(&repository, keys_dir)?)
        } else {
            None
        };

        let final_index = repository_index_path(cache_dir, &repository.repo_name)?;
        let final_signature = append_suffix(&final_index, ".minisign");
        check_existing_cache(
            &repository,
            public_key.as_deref(),
            &final_index,
            &final_signature,
        );

        let staging_id = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
        let staged_index = cache_dir.join(format!(
            ".{}-{}-{staging_id}.parquet.new",
            repository.repo_name,
            std::process::id()
        ));
        let staged_signature = repository
            .trusted
            .then(|| append_suffix(&staged_index, ".minisign"));

        downloads.push(
            Download::new(
                format!("{}/{}", repository.url, constants::PACKAGES_PARQUET),
                &staged_index,
            )
            .with_label(repository.repo_name.clone()),
        );
        if let Some(signature_path) = &staged_signature {
            downloads.push(
                Download::new(
                    format!(
                        "{}/{}",
                        repository.url,
                        constants::PACKAGES_PARQUET_SIGNATURE
                    ),
                    signature_path,
                )
                .with_label(format!("{} signature", repository.repo_name)),
            );
        }

        staged_repositories.push(StagedRepository {
            repository,
            public_key,
            staged_index,
            staged_signature,
            final_index,
            final_signature,
        });
    }

    if let Err(error) = net::download_files(downloads).await {
        cleanup_staged(&staged_repositories);
        return Err(CpsiError::NetError(error.to_string()));
    }

    if let Err(error) = verify_and_commit_staged(&staged_repositories) {
        cleanup_staged(&staged_repositories);
        return Err(error);
    }

    Ok(())
}

#[derive(Debug)]
struct StagedRepository {
    repository: RepositoryConfig,
    public_key: Option<String>,
    staged_index: PathBuf,
    staged_signature: Option<PathBuf>,
    final_index: PathBuf,
    final_signature: PathBuf,
}

/// Verify every trusted staged index before committing any repository cache.
fn verify_and_commit_staged(staged_repositories: &[StagedRepository]) -> Result<(), CpsiError> {
    for staged in staged_repositories {
        if staged.repository.trusted {
            let public_key = staged.public_key.as_deref().ok_or_else(|| {
                CpsiError::SignatureVerificationFailed(format!(
                    "missing public key for repository {}",
                    staged.repository.repo_name
                ))
            })?;
            let signature_path = staged.staged_signature.as_deref().ok_or_else(|| {
                CpsiError::SignatureVerificationFailed(format!(
                    "missing signature for repository {}",
                    staged.repository.repo_name
                ))
            })?;
            verify_file_with_sig(&staged.staged_index, signature_path, public_key)?;
        }
    }

    for staged in staged_repositories {
        if let Some(staged_signature) = &staged.staged_signature {
            fs::rename(staged_signature, &staged.final_signature)?;
        } else {
            remove_file_if_exists(&staged.final_signature)?;
        }
        fs::rename(&staged.staged_index, &staged.final_index)?;
    }

    Ok(())
}

fn resolve_trusted_public_key(
    repository: &RepositoryConfig,
    keys_dir: &Path,
) -> Result<String, CpsiError> {
    match load_public_key_from(&repository.repo_name, keys_dir) {
        Ok(stored_key) => {
            if stored_key != repository.public_key {
                return Err(CpsiError::SignatureVerificationFailed(format!(
                    "stored key for repository '{}' does not match its configuration",
                    repository.repo_name
                )));
            }
            Ok(stored_key)
        }
        Err(CpsiError::Io(error)) if error.kind() == ErrorKind::NotFound => {
            // Configurations written before the dedicated key directory was
            // introduced still contain the user-approved key.
            Ok(repository.public_key.clone())
        }
        Err(error) => Err(error),
    }
}

fn check_existing_cache(
    repository: &RepositoryConfig,
    public_key: Option<&str>,
    index_path: &Path,
    signature_path: &Path,
) {
    if !index_path.exists() {
        return;
    }

    if !repository.trusted {
        eprintln!(
            "warning: skipping cached index verification for untrusted repository '{}'",
            repository.repo_name
        );
        return;
    }

    let verification = public_key
        .ok_or_else(|| {
            CpsiError::SignatureVerificationFailed("missing repository public key".to_string())
        })
        .and_then(|key| verify_file_with_sig(index_path, signature_path, key));
    if let Err(error) = verification {
        eprintln!(
            "warning: cached index for repository '{}' could not be verified: {error}",
            repository.repo_name
        );
    }
}

fn cleanup_staged(staged_repositories: &[StagedRepository]) {
    for staged in staged_repositories {
        let _ = fs::remove_file(&staged.staged_index);
        if let Some(signature_path) = &staged.staged_signature {
            let _ = fs::remove_file(signature_path);
        }
    }
}

fn remove_file_if_exists(path: &Path) -> Result<(), CpsiError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CpsiError::Io(error)),
    }
}

pub fn repository_index_path(cache_dir: &Path, repo_name: &str) -> Result<PathBuf, CpsiError> {
    validate_repository_name(repo_name)?;
    Ok(cache_dir.join(format!("{repo_name}.parquet")))
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path_with_suffix = OsString::from(path.as_os_str());
    path_with_suffix.push(suffix);
    PathBuf::from(path_with_suffix)
}

fn trusted_by_default() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const PUBLIC_KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const PREHASHED_SIGNATURE: &str = concat!(
        "untrusted comment: signature from minisign secret key\n",
        "RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/",
        "z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\n",
        "trusted comment: timestamp:1556193335\tfile:test\n",
        "y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==\n",
    );
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_directory(label: &str) -> PathBuf {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cpsi-repository-sync-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn repository(trusted: bool) -> RepositoryConfig {
        RepositoryConfig {
            repo_name: "core".to_string(),
            url: "https://example.test".to_string(),
            public_key: PUBLIC_KEY.to_string(),
            fingerprint: String::new(),
            trusted,
        }
    }

    fn named_repository(repo_name: &str) -> RepositoryConfig {
        RepositoryConfig {
            repo_name: repo_name.to_string(),
            ..repository(true)
        }
    }

    #[test]
    fn loads_old_configs_as_trusted_and_skips_non_toml_files() {
        let dir = temp_directory("config");
        fs::write(
            dir.join("core.toml"),
            format!(
                "repo_name = \"core\"\nurl = \"https://example.test/\"\npublic_key = \"{PUBLIC_KEY}\"\n"
            ),
        )
        .unwrap();
        fs::write(dir.join("README"), "ignored").unwrap();

        let loaded = RepositoryConfig::load_repositories_from(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].trusted);
        assert_eq!(loaded[0].url, "https://example.test");
        assert_eq!(loaded[0].fingerprint, sha256::digest(PUBLIC_KEY));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn finds_repository_in_explicit_directory() {
        let dir = temp_directory("find");
        fs::write(
            dir.join("core.toml"),
            toml::to_string(&repository(true)).unwrap(),
        )
        .unwrap();

        assert_eq!(
            RepositoryConfig::find_in("core", &dir).unwrap().repo_name,
            "core"
        );
        assert!(matches!(
            RepositoryConfig::find_in("missing", &dir),
            Err(CpsiError::RepositoryNotFound(name)) if name == "missing"
        ));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn selects_every_repository_matching_prefix() {
        let selected = select_repositories_by_prefix(
            vec![
                named_repository("core"),
                named_repository("core-testing"),
                named_repository("extra"),
            ],
            "core",
        )
        .unwrap();

        assert_eq!(
            selected
                .iter()
                .map(|repository| repository.repo_name.as_str())
                .collect::<Vec<_>>(),
            ["core", "core-testing"]
        );
    }

    #[test]
    fn reports_missing_or_invalid_repository_prefix() {
        assert!(matches!(
            select_repositories_by_prefix(vec![named_repository("core")], "missing"),
            Err(CpsiError::RepositoryNotFound(prefix)) if prefix == "missing"
        ));
        assert!(select_repositories_by_prefix(vec![named_repository("core")], "../core").is_err());
        assert!(select_repositories_by_prefix(vec![named_repository("core")], "").is_err());
    }

    #[test]
    fn accepts_prefixes_that_are_only_partial_repository_names() {
        let selected =
            select_repositories_by_prefix(vec![named_repository(".hidden")], ".").unwrap();

        assert_eq!(selected[0].repo_name, ".hidden");
    }

    #[test]
    fn invalid_signature_does_not_replace_existing_cache() {
        let dir = temp_directory("atomic");
        let final_index = dir.join("core.parquet");
        let final_signature = dir.join("core.parquet.minisign");
        let staged_index = dir.join(".core.parquet.new");
        let staged_signature = dir.join(".core.parquet.new.minisign");
        fs::write(&final_index, b"old cache").unwrap();
        fs::write(&staged_index, b"tampered").unwrap();
        fs::write(&staged_signature, PREHASHED_SIGNATURE).unwrap();

        let staged = StagedRepository {
            repository: repository(true),
            public_key: Some(PUBLIC_KEY.to_string()),
            staged_index,
            staged_signature: Some(staged_signature),
            final_index: final_index.clone(),
            final_signature,
        };

        assert!(matches!(
            verify_and_commit_staged(&[staged]),
            Err(CpsiError::SignatureVerificationFailed(_))
        ));
        assert_eq!(fs::read(final_index).unwrap(), b"old cache");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn valid_signature_commits_cache() {
        let dir = temp_directory("commit");
        let final_index = dir.join("core.parquet");
        let final_signature = dir.join("core.parquet.minisign");
        let staged_index = dir.join(".core.parquet.new");
        let staged_signature = dir.join(".core.parquet.new.minisign");
        fs::write(&staged_index, b"test").unwrap();
        fs::write(&staged_signature, PREHASHED_SIGNATURE).unwrap();

        let staged = StagedRepository {
            repository: repository(true),
            public_key: Some(PUBLIC_KEY.to_string()),
            staged_index,
            staged_signature: Some(staged_signature),
            final_index: final_index.clone(),
            final_signature: final_signature.clone(),
        };

        verify_and_commit_staged(&[staged]).unwrap();
        assert_eq!(fs::read(final_index).unwrap(), b"test");
        assert!(final_signature.is_file());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stored_key_must_match_configuration() {
        let dir = temp_directory("key-mismatch");
        fs::write(dir.join("core.pub"), PUBLIC_KEY.replace('R', "S")).unwrap();

        assert!(resolve_trusted_public_key(&repository(true), &dir).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
