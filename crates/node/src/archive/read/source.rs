//! A published log as the CLIs name it: a path on disk, a node's or a mirror's public origin, or the log's place in an
//! archive's bucket. A node's origin serves `/v1/archive/manifest.jsonl`, listing the completed segments, each read
//! from the origin or, for one the node no longer holds, from the bucket its `308` names, and each checked against its
//! listing. A bucket's log, at `<public origin>/log/<chain id>/<vault>/`, lists nothing and has no end of its own: it
//! is read through the vault's checkpoint at its `latestSeq` and `latestRoot` (`read_through`), from the bucket alone.
//! Either is downloaded into a directory of its own, so memory holds one segment or range, and the CLI then reads that
//! directory, or the log file in it, as it reads a log on disk: the trust is the same, and the download needs disk for
//! the whole log.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, bail, ensure};
use engine_core::eip712::B256;
use engine_core::types::AccountId;
use reqwest::Url;

use crate::archive::log_prefix;
use crate::archive::read::{Bucket, read_through};
use crate::log::MANIFEST;
use crate::standby::archive::{archive_client, bootstrap_archive};

/// The file a log read from a bucket is written to, in its download's directory.
const BUCKET_LOG: &str = "log.jsonl";

/// A published log, as `--log` names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishedLog {
    /// A log directory, a downloaded archive, or a single log file.
    Path(PathBuf),
    /// An archive's public origin, a node's or a mirror's, over `http` or `https`.
    Url(Url),
    /// A log in an archive's bucket: `<public origin>/log/<chain id>/<vault>/`, its vault in lower case.
    Bucket(Url),
}

/// A published log on disk, downloaded into a directory removed with it when its source is a URL.
#[derive(Debug)]
pub struct LocalLog {
    path: PathBuf,
    /// The download's directory, removed with the log.
    downloaded: Option<PathBuf>,
}

impl FromStr for PublishedLog {
    type Err = anyhow::Error;

    /// A bucket's log for a URL whose path ends `log/<chain id>/<vault>`, a node's origin for any other URL, and a
    /// path otherwise.
    /// Error cases:
    /// - the URL does not parse, or names a query, a fragment or credentials
    fn from_str(arg: &str) -> anyhow::Result<Self> {
        if arg.starts_with("http://") || arg.starts_with("https://") {
            let url = Url::parse(arg).with_context(|| format!("{arg} is not a URL"))?;
            ensure!(url.query().is_none() && url.fragment().is_none(), "an archive's URL names no query or fragment");
            // An archive is public: a URL that carried credentials would print them with every message naming it.
            ensure!(url.username().is_empty() && url.password().is_none(), "an archive's URL carries no credentials");
            return Ok(bucket_log(&url).map_or(Self::Url(url), Self::Bucket));
        }
        Ok(Self::Path(PathBuf::from(arg)))
    }
}

impl PublishedLog {
    /// The log on disk: the path as it is, or the archive at a node's URL, or a bucket's log through the vault's
    /// checkpoint `through` names, its seq and root, downloaded into a new directory under the system's temporary
    /// directory (`TMPDIR`).
    /// Error cases:
    /// - a bucket's log without `through`, or `through` for any other log, which holds its own end
    /// - a node's archive serves no manifest, or a segment cannot be read whole as its listing names it (see
    ///   `bootstrap_archive`), or the bucket does not hold the log through the checkpoint (see `read_through`)
    /// - the directory cannot be written
    pub fn local(&self, through: Option<(u64, B256)>) -> anyhow::Result<LocalLog> {
        self.local_in(&std::env::temp_dir(), through)
    }

    /// `local`, downloading into a new directory under `parent`.
    /// Error cases:
    /// - see `local`
    pub(crate) fn local_in(&self, parent: &Path, through: Option<(u64, B256)>) -> anyhow::Result<LocalLog> {
        let local = self.local_until(parent, through, async {
            let _ = tokio::signal::ctrl_c().await;
        })?;
        if let Some(dir) = &local.downloaded {
            remove_on_interrupt(dir.clone());
        }
        Ok(local)
    }

    /// `local_in`, a download stopping when `interrupt` completes, which removes it as a failed one is.
    /// Error cases:
    /// - see `local`, or `interrupt` completes before the download does
    fn local_until(
        &self,
        parent: &Path,
        through: Option<(u64, B256)>,
        interrupt: impl Future<Output = ()>,
    ) -> anyhow::Result<LocalLog> {
        match (self, through) {
            (Self::Path(path), None) => Ok(LocalLog { path: path.clone(), downloaded: None }),
            (Self::Url(url), None) => {
                let dir = download_dir(parent);
                let local = LocalLog { path: dir.clone(), downloaded: Some(dir) };
                eprintln!("downloading the archive at {url} into {}", local.path.display());
                interruptible(download(&archive_client(false)?, url, &local.path), interrupt)?;
                Ok(local)
            }
            (Self::Bucket(url), Some(through)) => {
                let dir = download_dir(parent);
                let local = LocalLog { path: dir.join(BUCKET_LOG), downloaded: Some(dir.clone()) };
                eprintln!("reading the log in the bucket at {url} through seq {} into {}", through.0, dir.display());
                std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
                let mut file = BufWriter::new(File::create(&local.path).context("creating the log's file")?);
                let (http, log) = (archive_client(false)?, Bucket::new(url, "")?);
                interruptible(read_through(&http, &log, through, &mut file), interrupt)?;
                file.flush().context("writing the log's file")?;
                Ok(local)
            }
            (Self::Bucket(url), None) => bail!(
                "{url} is a log in an archive's bucket, which has no end of its own: `engine replay` and `engine \
                 exit-proof` read it through the vault's checkpoint that their --seq and --state-root name, its \
                 latestSeq and latestRoot"
            ),
            (Self::Path(_) | Self::Url(_), Some(_)) => bail!(
                "--state-root names where a log in an archive's bucket ends, and a path or a node's archive holds \
                 its own"
            ),
        }
    }
}

impl PublishedLog {
    /// `local`, with the segments a log directory lacks, as a node's local window removes them, put back into it
    /// first from the archive the node config at `config` names (`archive::restore_whole`), so the directory reads
    /// from genesis.
    /// Error cases:
    /// - as `local`, or the config does not load or names no `[archive]`, or a lacking segment cannot be read from
    ///   the archive or written
    pub fn local_filled(&self, through: Option<(u64, B256)>, config: Option<&Path>) -> anyhow::Result<LocalLog> {
        let local = self.local(through)?;
        if let (Self::Path(dir), Some(config)) = (self, config)
            && dir.is_dir()
        {
            let cfg = crate::config::NodeConfig::load(config)?;
            ensure!(
                cfg.archive.is_some(),
                "{} names no [archive] to put the log's segments back from",
                config.display()
            );
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            let put = runtime.block_on(crate::archive::restore_whole(&cfg, dir))?;
            eprintln!("put back {put} segments of {} from the archive", dir.display());
        }
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
    /// Removes a download; a path the caller named stays.
    fn drop(&mut self) {
        if let Some(dir) = &self.downloaded {
            let _ = std::fs::remove_dir_all(dir);
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

/// A new directory under `parent`, named for this process and the time.
fn download_dir(parent: &Path) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |since| since.as_nanos());
    parent.join(format!("engine-archive-{}-{nanos}", std::process::id()))
}

/// Runs `download` to its end on a runtime of its own, or stops it when `interrupt` completes, so that a Ctrl-C
/// returns through the caller, which removes the partial download, instead of ending the process with it in place.
/// Error cases:
/// - the runtime does not build, `download` fails, or `interrupt` completes first
fn interruptible(
    download: impl Future<Output = anyhow::Result<()>>,
    interrupt: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    runtime()?.block_on(async {
        tokio::select! {
            done = download => done,
            () = interrupt => bail!("the download was interrupted"),
        }
    })
}

/// Once the download's own handler is installed, a Ctrl-C no longer ends the process by default, so this keeps one:
/// a thread that waits for it, removes `dir` and exits with the status a Ctrl-C gives.
fn remove_on_interrupt(dir: PathBuf) {
    std::thread::spawn(move || {
        let Ok(runtime) = runtime() else { return };
        if runtime.block_on(tokio::signal::ctrl_c()).is_ok() {
            let _ = std::fs::remove_dir_all(&dir);
            std::process::exit(130);
        }
    });
}

/// The runtime a download runs on.
/// Error cases:
/// - the runtime does not build
fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().context("building the download's runtime")
}

/// The log's place in a bucket, `<origin>/log/<chain id>/<vault>/` as the uploader keys it, when `url`'s path ends
/// with those three parts; none otherwise.
fn bucket_log(url: &Url) -> Option<Url> {
    let parts: Vec<&str> = url.path_segments()?.filter(|part| !part.is_empty()).collect();
    let [origin @ .., "log", chain_id, vault] = parts.as_slice() else { return None };
    let (chain_id, vault) = (chain_id.parse::<u64>().ok()?, vault.parse::<AccountId>().ok()?);
    let mut log = url.clone();
    log.set_path(&format!(
        "/{}{}",
        origin.iter().map(|part| format!("{part}/")).collect::<String>(),
        log_prefix(chain_id, vault)
    ));
    Some(log)
}

#[cfg(test)]
mod tests;
