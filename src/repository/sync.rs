use crate::util::{
    constants::{self},
    net::Download,
};
use std::{fs, io::ErrorKind};

use crate::util::net;
use cps_common::errors::CpsiError;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct RepositoryConfig {
    repo_name: String,
    url: String,
    public_key: String,
}

impl RepositoryConfig {
    pub fn load_repositories() -> Result<Vec<RepositoryConfig>, CpsiError> {
        let mut repositories: Vec<RepositoryConfig> = Vec::new();

        let read_dir = match fs::read_dir(constants::REPOSITORIES_CONFIG_DIRECTORY) {
            Ok(o) => o,
            Err(e) => {
                if e.kind() == ErrorKind::NotFound {
                    return Err(CpsiError::NoRepositories);
                } else {
                    return Err(CpsiError::Io(e));
                }
            }
        };

        for entry in read_dir {
            let entry_path = entry?.path();

            if !entry_path.extension().is_some_and(|f| f == "toml") {
                return Err(CpsiError::NoRepositories);
            }

            let file_content = fs::read_to_string(entry_path)?;
            match toml::from_str::<RepositoryConfig>(&file_content) {
                Ok(o) => repositories.push(o),
                Err(e) => {
                    return Err(CpsiError::Toml(e.to_string()));
                }
            }
        }

        Ok(repositories)
    }
}

pub async fn sync() -> Result<(), CpsiError> {
    let mut downloads: Vec<net::Download> = Vec::new();

    let repositories = RepositoryConfig::load_repositories()?;
    if repositories.is_empty() {
        return Err(CpsiError::NoRepositories);
    }

    for repo in repositories {
        downloads.push(
            Download::new(
                format!("{}/Packages.parquet", repo.url),
                format!(
                    "{}/{}.parquet",
                    constants::REPOSITORIES_DIRECTORY,
                    repo.repo_name
                ),
            )
            .with_label(repo.repo_name),
        );
    }

    if let Err(e) = net::download_files(downloads).await {
        return Err(CpsiError::NetError(e.to_string()));
    }

    Ok(())
}
