use cps_common::errors::CpsiError;
use serde::{Deserialize, Serialize};

use crate::util::constants;
use crate::util::net::{self, Download};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
struct Repository {
    pub repo_name: String,
    pub public_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AddTargetRepository {
    pub url: String,
    pub repo_name: String,
    pub public_key: String,
    pub fingerprint: String,
}

impl AddTargetRepository {
    /// download "{url}/repository.json" and parse with `Repository` struct using serde_json
    pub fn new(url: &str) -> Result<Self, CpsiError> {
        let data_file_loc = format!("{}/repository.json", constants::TEMP_DOWNLOAD_LOCATION);

        let download_url = Download::new(format!("{}/repository.json", url), &data_file_loc);

        // download
        if let Err(e) = net::download_file(download_url) {
            return Err(CpsiError::NetError(e.to_string()));
        }

        // read and parse with json
        let data = fs::read_to_string(&data_file_loc)?;
        let repo: Repository = serde_json::from_str(&data).unwrap();

        // fingerprint
        let fingerprint = sha256::digest(&repo.public_key);

        Ok(Self {
            url: url.to_string(),
            repo_name: repo.repo_name,
            public_key: repo.public_key,
            fingerprint: fingerprint,
        })
    }

    pub fn save(&self) -> Result<(), CpsiError> {
        if !Path::new(constants::REPOSITORIES_CONFIG_DIRECTORY).exists() {
            fs::create_dir_all(constants::REPOSITORIES_CONFIG_DIRECTORY)?;
        }

        let toml_file = format!(
            "{}/{}.toml",
            constants::REPOSITORIES_CONFIG_DIRECTORY,
            &self.repo_name
        );

        let mut writer = fs::File::create_new(toml_file)?;

        let toml_str = match toml::to_string(&self) {
            Ok(o) => o,
            Err(e) => return Err(CpsiError::Toml(e.to_string())),
        };
        writer.write_all(toml_str.as_bytes())?;

        Ok(())
    }
}
