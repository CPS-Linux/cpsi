use crate::{
    repository::{
        add::{self, repository_config_path},
        sync::{RepositoryConfig, repository_index_path},
        validate_repository_name,
    },
    signature::public_key_path,
    util::constants,
};
use cps_common::errors::CpsiError;
use std::{
    ffi::OsString,
    fs,
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
};

/// Add a trusted repository, retaining the original CLI API.
pub fn add_repository(url: String) -> Result<(), CpsiError> {
    add_repository_with_options(url, false)
}

/// Add a repository, optionally disabling signature verification for development.
pub fn add_repository_with_options(url: String, insecure: bool) -> Result<(), CpsiError> {
    let repository = add::AddTargetRepository::new_with_trust(&url, !insecure)?;

    println!("Repository URL: {}", repository.url);
    println!("Fingerprint:\n{}", repository.fingerprint);

    if insecure {
        eprintln!(
            "warning: adding '{}' as an untrusted repository; signatures will not be verified",
            repository.repo_name
        );
        repository.save()?;
        println!("done");
        return Ok(());
    }

    print!("\nTrust this key? [y/N] ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    if matches!(input.trim(), "y" | "Y") {
        repository.save()?;
        println!("done");
    } else {
        println!("canceled.");
    }

    Ok(())
}

/// Print all configured repositories.
pub fn list_repositories() -> Result<(), CpsiError> {
    let repositories = list_repositories_from(Path::new(constants::REPOSITORIES_CONFIG_DIRECTORY))?;
    for repository in repositories {
        println!(
            "{}\t{}\t{}",
            repository.repo_name, repository.url, repository.fingerprint
        );
    }
    Ok(())
}

/// Load repositories for a list operation from an explicit directory.
pub fn list_repositories_from(config_dir: &Path) -> Result<Vec<RepositoryConfig>, CpsiError> {
    let repositories = RepositoryConfig::load_repositories_from(config_dir)?;
    if repositories.is_empty() {
        return Err(CpsiError::NoRepositories);
    }
    Ok(repositories)
}

/// Remove a repository's configuration, trusted key, and cached index.
pub fn remove_repository(repo_name: &str) -> Result<(), CpsiError> {
    remove_repository_from(
        repo_name,
        Path::new(constants::REPOSITORIES_CONFIG_DIRECTORY),
        Path::new(constants::PUBLIC_KEYS_DIRECTORY),
        Path::new(constants::REPOSITORIES_DIRECTORY),
    )?;
    println!("removed repository '{repo_name}'");
    Ok(())
}

/// Remove repository state rooted at explicit paths.
pub fn remove_repository_from(
    repo_name: &str,
    config_dir: &Path,
    keys_dir: &Path,
    cache_dir: &Path,
) -> Result<(), CpsiError> {
    validate_repository_name(repo_name)?;
    let config_path = repository_config_path(config_dir, repo_name)?;
    match fs::remove_file(&config_path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(CpsiError::RepositoryNotFound(repo_name.to_string()));
        }
        Err(error) => return Err(CpsiError::Io(error)),
    }

    remove_if_exists(&public_key_path(keys_dir, repo_name)?)?;
    let index_path = repository_index_path(cache_dir, repo_name)?;
    remove_if_exists(&index_path)?;
    remove_if_exists(&append_suffix(&index_path, ".minisign"))?;

    Ok(())
}

fn remove_if_exists(path: &Path) -> Result<(), CpsiError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(CpsiError::Io(error)),
    }
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path_with_suffix = OsString::from(path.as_os_str());
    path_with_suffix.push(suffix);
    PathBuf::from(path_with_suffix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const PUBLIC_KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_directory(label: &str) -> PathBuf {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "cpsi-cli-repo-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_repository(config_dir: &Path, repo_name: &str) {
        fs::create_dir_all(config_dir).unwrap();
        fs::write(
            config_dir.join(format!("{repo_name}.toml")),
            format!(
                "repo_name = \"{repo_name}\"\nurl = \"https://example.test\"\npublic_key = \"{PUBLIC_KEY}\"\nfingerprint = \"fingerprint\"\ntrusted = true\n"
            ),
        )
        .unwrap();
    }

    #[test]
    fn lists_repositories_in_name_order() {
        let root = temp_directory("list");
        let config_dir = root.join("repos.d");
        write_repository(&config_dir, "zeta");
        write_repository(&config_dir, "alpha");

        let repositories = list_repositories_from(&config_dir).unwrap();
        assert_eq!(repositories[0].repo_name, "alpha");
        assert_eq!(repositories[1].repo_name, "zeta");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removes_repository_owned_state_only() {
        let root = temp_directory("remove");
        let config_dir = root.join("repos.d");
        let keys_dir = root.join("keys");
        let cache_dir = root.join("cache");
        write_repository(&config_dir, "core");
        write_repository(&config_dir, "extra");
        fs::create_dir_all(&keys_dir).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();
        fs::write(keys_dir.join("core.pub"), PUBLIC_KEY).unwrap();
        fs::write(cache_dir.join("core.parquet"), b"cache").unwrap();
        fs::write(cache_dir.join("core.parquet.minisign"), b"signature").unwrap();

        remove_repository_from("core", &config_dir, &keys_dir, &cache_dir).unwrap();

        assert!(!config_dir.join("core.toml").exists());
        assert!(!keys_dir.join("core.pub").exists());
        assert!(!cache_dir.join("core.parquet").exists());
        assert!(config_dir.join("extra.toml").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reports_missing_repository() {
        let root = temp_directory("missing");
        assert!(matches!(
            remove_repository_from(
                "missing",
                &root.join("repos.d"),
                &root.join("keys"),
                &root.join("cache")
            ),
            Err(CpsiError::RepositoryNotFound(name)) if name == "missing"
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
