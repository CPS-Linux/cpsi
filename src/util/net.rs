use futures_util::{StreamExt, TryStreamExt, stream};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::{Client, StatusCode, blocking::Client as BlockingClient};
use std::{
    fs as std_fs, io,
    path::{Path, PathBuf},
};
use thiserror::Error;
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
};

const DEFAULT_PARALLEL_DOWNLOADS: usize = 4;

#[derive(Debug, Clone)]
pub struct Download {
    pub url: String,
    pub destination: PathBuf,
    pub label: Option<String>,
}

impl Download {
    pub fn new<U, P>(url: U, destination: P) -> Self
    where
        U: Into<String>,
        P: Into<PathBuf>,
    {
        Self {
            url: url.into(),
            destination: destination.into(),
            label: None,
        }
    }

    pub fn with_label<L>(mut self, label: L) -> Self
    where
        L: Into<String>,
    {
        self.label = Some(label.into());
        self
    }
}

#[derive(Debug, Error)]
pub enum NetError {
    #[error("request failed for {url}: {source}")]
    Request {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("download failed for {url}: HTTP {status}")]
    HttpStatus { url: String, status: StatusCode },

    #[error("I/O failed for {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub async fn download_files<I>(downloads: I) -> Result<Vec<PathBuf>, NetError>
where
    I: IntoIterator<Item = Download>,
{
    download_files_with_limit(downloads, DEFAULT_PARALLEL_DOWNLOADS).await
}

pub async fn download_files_with_limit<I>(
    downloads: I,
    parallel_downloads: usize,
) -> Result<Vec<PathBuf>, NetError>
where
    I: IntoIterator<Item = Download>,
{
    let client = Client::new();
    let multi = MultiProgress::new();
    let limit = parallel_downloads.max(1);

    let mut completed: Vec<(usize, PathBuf)> = stream::iter(downloads.into_iter().enumerate())
        .map(|(index, download)| {
            let client = client.clone();
            let multi = multi.clone();
            async move {
                download_file_with_progress(client, multi, download)
                    .await
                    .map(|path| (index, path))
            }
        })
        .buffer_unordered(limit)
        .try_collect()
        .await?;

    completed.sort_by_key(|(index, _)| *index);

    Ok(completed.into_iter().map(|(_, path)| path).collect())
}

pub fn download_file(download: Download) -> Result<PathBuf, NetError> {
    let client = BlockingClient::new();
    let mut response = client
        .get(&download.url)
        .send()
        .map_err(|source| NetError::Request {
            url: download.url.clone(),
            source,
        })?;

    let status = response.status();
    if !status.is_success() {
        return Err(NetError::HttpStatus {
            url: download.url,
            status,
        });
    }

    if let Some(parent) = download
        .destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        std_fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    }

    let mut file = std_fs::File::create(&download.destination)
        .map_err(|source| io_error(&download.destination, source))?;
    io::copy(&mut response, &mut file).map_err(|source| io_error(&download.destination, source))?;
    io::Write::flush(&mut file).map_err(|source| io_error(&download.destination, source))?;

    Ok(download.destination)
}

async fn download_file_with_progress(
    client: Client,
    multi: MultiProgress,
    download: Download,
) -> Result<PathBuf, NetError> {
    let response = client
        .get(&download.url)
        .send()
        .await
        .map_err(|source| NetError::Request {
            url: download.url.clone(),
            source,
        })?;

    let status = response.status();
    if !status.is_success() {
        return Err(NetError::HttpStatus {
            url: download.url,
            status,
        });
    }

    if let Some(parent) = download
        .destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .await
            .map_err(|source| io_error(parent, source))?;
    }

    let mut file = File::create(&download.destination)
        .await
        .map_err(|source| io_error(&download.destination, source))?;

    let total = response.content_length().unwrap_or(0);
    let progress = multi.add(ProgressBar::new(total));
    progress.set_style(progress_style(total > 0));
    progress.set_message(
        download
            .label
            .clone()
            .unwrap_or_else(|| download.destination.display().to_string()),
    );

    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|source| NetError::Request {
            url: download.url.clone(),
            source,
        })?
    {
        file.write_all(&chunk)
            .await
            .map_err(|source| io_error(&download.destination, source))?;
        progress.inc(chunk.len() as u64);
    }

    file.flush()
        .await
        .map_err(|source| io_error(&download.destination, source))?;

    progress.finish_with_message(format!(
        "{}",
        download
            .label
            .unwrap_or_else(|| download.destination.display().to_string())
    ));

    Ok(download.destination)
}

fn progress_style(has_total: bool) -> ProgressStyle {
    let template = if has_total {
        "{spinner:.green} {msg:30} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA {eta})"
    } else {
        "{spinner:.green} {msg:30} {bytes} ({bytes_per_sec})"
    };

    ProgressStyle::with_template(template)
        .expect("progress template is valid")
        .progress_chars("#>-")
}

fn io_error(path: impl AsRef<Path>, source: io::Error) -> NetError {
    NetError::Io {
        path: path.as_ref().to_path_buf(),
        source,
    }
}
