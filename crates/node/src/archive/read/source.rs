//! A published log as the CLIs name it: a path on disk, or an archive's URL. A URL is a node's, or a mirror's,
//! public origin: its `/v1/archive/manifest.jsonl` lists the completed segments, each read from the origin or, for
//! one the node no longer holds, from the bucket its `308` names, and each checked against its listing. The log is
//! downloaded a segment at a time into a directory of its own, so memory holds one segment, and the CLI then reads
//! that directory as it reads a log on disk: the trust is the same, and the download needs disk for the whole log.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, ensure};
use reqwest::Url;

use crate::log::MANIFEST;
use crate::standby::archive::{archive_client, bootstrap_archive};

/// A published log, as `--log` names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishedLog {
    /// A log directory, a downloaded archive, or a single log file.
    Path(PathBuf),
    /// An archive's public origin, over `http` or `https`.
    Url(Url),
}

/// A published log on disk, downloaded into a directory removed with it when its source is a URL.
#[derive(Debug)]
pub struct LocalLog {
    path: PathBuf,
    downloaded: bool,
}

impl FromStr for PublishedLog {
    type Err = anyhow::Error;

    /// A URL for an argument that starts `http://` or `https://`, a path otherwise.
    /// Error cases:
    /// - the URL does not parse, or names a query, a fragment or credentials
    fn from_str(arg: &str) -> anyhow::Result<Self> {
        if arg.starts_with("http://") || arg.starts_with("https://") {
            let url = Url::parse(arg).with_context(|| format!("{arg} is not a URL"))?;
            ensure!(url.query().is_none() && url.fragment().is_none(), "an archive's URL names no query or fragment");
            // An archive is public: a URL that carried credentials would print them with every message naming it.
            ensure!(url.username().is_empty() && url.password().is_none(), "an archive's URL carries no credentials");
            return Ok(Self::Url(url));
        }
        Ok(Self::Path(PathBuf::from(arg)))
    }
}

impl PublishedLog {
    /// The log on disk: the path as it is, or the archive at the URL downloaded into a new directory under the
    /// system's temporary directory (`TMPDIR`).
    /// Error cases:
    /// - the archive serves no manifest, or a segment cannot be read whole as its listing names it (see
    ///   `bootstrap_archive`), or the directory cannot be written
    pub fn local(&self) -> anyhow::Result<LocalLog> {
        let url = match self {
            Self::Path(path) => return Ok(LocalLog { path: path.clone(), downloaded: false }),
            Self::Url(url) => url,
        };
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos());
        let dir = std::env::temp_dir().join(format!("engine-archive-{}-{nanos}", std::process::id()));
        let local = LocalLog { path: dir, downloaded: true };
        eprintln!("downloading the archive at {url} into {}", local.path.display());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(download(&archive_client(false)?, url, &local.path))?;
        Ok(local)
    }
}

impl LocalLog {
    /// Where the log is on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LocalLog {
    /// Removes a downloaded copy; a path the caller named stays.
    fn drop(&mut self) {
        if self.downloaded {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Downloads the archive at `url` into `dir` over `http`, every completed segment its manifest lists, each checked
/// against its listing, and the manifest.
/// Error cases:
/// - the archive serves no manifest, or a segment cannot be read as its listing names it, or `dir` cannot be written
pub(crate) async fn download(http: &reqwest::Client, url: &Url, dir: &Path) -> anyhow::Result<()> {
    let origin = [url.as_str().trim_end_matches('/').to_owned()];
    bootstrap_archive(http, &origin, dir).await.with_context(|| format!("downloading {url}"))?;
    ensure!(dir.join(MANIFEST).exists(), "{url} serves no archive manifest");
    Ok(())
}

#[cfg(test)]
mod tests;
