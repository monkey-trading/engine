//! Daily snapshots as `engine replay --snapshots` names them: a directory of them, or a node's public origin. From a
//! URL, the node's manifest lists the snapshots, and each a range of the log can start from, of a checkpoint at or
//! before the log's last, is downloaded from the node's disk, or from the archive's bucket where the node redirects
//! it (`308`), checked against its listing, into a directory of its own removed with it. The replay then reads that
//! directory as one on disk, and checks each snapshot against the log as it checks one there: the trust is the same,
//! and the download needs disk for the snapshots.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use reqwest::Url;

use crate::archive::PublishedLog;
use crate::daily_snapshots::{Takes, canonical, download, manifest};
use crate::standby::archive::archive_client;

/// Daily snapshots, as `--snapshots` names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishedSnapshots {
    /// A directory of daily snapshots, as `/v1/archive/snapshots/` serves them.
    Path(PathBuf),
    /// A node's public origin, over `http` or `https`.
    Url(Url),
}

/// Daily snapshots on disk, downloaded into a directory removed with it when their source is a URL.
#[derive(Debug)]
pub struct LocalSnapshots {
    path: PathBuf,
    downloaded: bool,
}

impl FromStr for PublishedSnapshots {
    type Err = anyhow::Error;

    /// A URL for an argument that starts `http://` or `https://`, a path otherwise, as `--log` reads one.
    /// Error cases:
    /// - the URL does not parse, or names a query, a fragment or credentials
    fn from_str(arg: &str) -> anyhow::Result<Self> {
        Ok(match arg.parse::<PublishedLog>()? {
            PublishedLog::Path(path) => Self::Path(path),
            PublishedLog::Url(url) => Self::Url(url),
        })
    }
}

impl PublishedSnapshots {
    /// The snapshots on disk for a replay of the log directory at `log`: the directory as it is, or those the node
    /// at the URL lists at or before the log's last checkpoint, downloaded into a new directory under the system's
    /// temporary directory (`TMPDIR`).
    /// Error cases:
    /// - the log's last checkpoint cannot be read, the node serves no manifest, or a snapshot cannot be read whole as
    ///   its listing names it (see `download_listed`), or the directory cannot be written
    pub fn local(&self, log: &Path) -> anyhow::Result<LocalSnapshots> {
        let url = match self {
            Self::Path(path) => return Ok(LocalSnapshots { path: path.clone(), downloaded: false }),
            Self::Url(url) => url,
        };
        let last = crate::log::last_checkpoint(log)?.map(|checkpoint| checkpoint.seq);
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos());
        let dir = std::env::temp_dir().join(format!("engine-snapshots-{}-{nanos}", std::process::id()));
        let local = LocalSnapshots { path: dir, downloaded: true };
        eprintln!("downloading the daily snapshots at {url} into {}", local.path.display());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        runtime.block_on(download_listed(&archive_client(false)?, url, &local.path, last))?;
        Ok(local)
    }
}

impl LocalSnapshots {
    /// Where the snapshots are on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for LocalSnapshots {
    /// Removes downloaded snapshots; a directory the caller named stays.
    fn drop(&mut self) {
        if self.downloaded {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// Downloads into `dir` over `http` each snapshot the node at `url` lists under its canonical name at or before the
/// checkpoint at seq `last`, the log's last, and none without one, each checked against its listing; how many.
/// Error cases:
/// - the node serves no manifest, or a snapshot cannot be read whole as its listing names it, from the node or from
///   the object its redirect names (`download`), or `dir` cannot be written
pub(crate) async fn download_listed(
    http: &reqwest::Client,
    url: &Url,
    dir: &Path,
    last: Option<u64>,
) -> anyhow::Result<usize> {
    let origin = url.as_str().trim_end_matches('/');
    let listed = manifest(http, origin).await.with_context(|| format!("the daily snapshots at {url}"))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let wanted: Vec<_> =
        listed.iter().filter(|listing| canonical(listing) && last.is_some_and(|last| listing.seq <= last)).collect();
    for listing in &wanted {
        let takes = Takes { bucket: None, this_format: false };
        download(http, origin, dir, listing, takes)
            .await
            .with_context(|| format!("daily snapshot {}", listing.file))?;
    }
    Ok(wanted.len())
}

#[cfg(test)]
pub(crate) mod tests;
