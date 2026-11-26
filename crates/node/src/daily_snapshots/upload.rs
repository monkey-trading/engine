//! The daily snapshots' uploader: for the node's life, under `[daily_snapshots]` and `[archive]` with both of its
//! keys, it reads the snapshots' manifest every `POLL_EVERY` and puts each listed snapshot the host holds into the
//! bucket as it is published, newest first, at `snapshots/<chain id>/<vault>/<file>` (`SnapshotBucket`), the file as
//! written. An upload is create-only and sends only the bytes the listing names: the file is hashed as it streams,
//! and its last chunk goes only once the whole matches, so a file that differs from its listing creates nothing, and
//! pages Warning `daily_snapshot_unreadable`. A snapshot counts as confirmed only once a read of its object from the
//! bucket's public origin, over TLS that ends in the enclave, has the listing's length and SHA-256; one the bucket
//! already holds, as another member's upload or this node's before a restart, is read back with no upload. An object
//! that reads back otherwise stays, since nothing may replace it, and pages Warning `archive_object_mismatch`. A
//! failure is retried with backoff, and a snapshot that waits `[archive] lag_alert` for its confirmation pages
//! Warning `archive_upload_lagging`, keyed `objects = "daily_snapshots"` apart from the log's ranges.
//!
//! The host's copy of a snapshot is removed once this process has confirmed it and it is stamped more than
//! `local_window` before the newest listed, but for the newest such one, from which a replay of the window's
//! commands, as the history's re-derivation of an indexed command's rows, starts. The manifest keeps every listing,
//! so the archive redirects a request for a removed snapshot to its object (`api::archive`). A snapshot not
//! confirmed stays whatever its age: the host holds the window's snapshots and one before them, and those the
//! bucket has not taken. A removal that fails is tried again at each pass, and pages Warning
//! `daily_snapshot_unremovable` until one succeeds.
//!
//! The snapshots and their manifest are the host's. What makes a snapshot usable is its signature and its root, which
//! every reader checks, so the uploader checks only that what goes up, and what reads back, is what the listing names.
//! A snapshot goes up before the vault finalizes its checkpoint, so one of a tail a takeover sets aside can hold the key
//! of a later snapshot of the same offset in the successor's log: that one then reads back otherwise, and stays on the
//! host, which serves it, while every reader refuses the object, whose checkpoint the log does not hold.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow};
use engine_core::types::Millis;
use futures_util::stream::try_unfold;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::task::spawn_blocking;
use tokio::time::{Instant, sleep};
use tracing::{Instrument, info, info_span, warn};

use crate::archive::{Digested, Store};
use crate::config::{NodeConfig, Values};
use crate::daily_snapshots::writer::sync_dir;
use crate::daily_snapshots::{Listed, canonical, read_manifest, snapshots_prefix};
use crate::metrics::Metrics;

/// How often the manifest is read for snapshots to upload and copies to remove.
const POLL_EVERY: Duration = if cfg!(test) { Duration::from_millis(20) } else { Duration::from_secs(10) };
/// The wait after a failed pass; each failure in a row doubles it, up to `MAX_RETRY`.
const FIRST_RETRY: Duration = if cfg!(test) { Duration::from_millis(20) } else { Duration::from_secs(1) };
const MAX_RETRY: Duration = if cfg!(test) { Duration::from_millis(200) } else { Duration::from_secs(300) };
/// How long one upload or read-back of a snapshot may take, its body included: a snapshot of 100,000 accounts is
/// about 420 MB.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// The bytes of a snapshot read from its file at a time.
const CHUNK_BYTES: usize = 256 * 1024;
/// A snapshot's media type.
const JSON: &str = "application/json";

/// The uploader, and what it knows of the bucket.
pub(crate) struct SnapshotUploader {
    store: Store,
    /// The key prefix of this log's snapshots (`snapshots_prefix`).
    prefix: String,
    dir: PathBuf,
    /// How long, in sequenced time, the host keeps a confirmed snapshot (`local_window`).
    window_ms: Millis,
    lag_alert: Duration,
    metrics: Arc<Metrics>,
    /// The snapshots on the host this process has confirmed, by file, as they were listed then.
    confirmed: BTreeMap<String, Listed>,
    /// The snapshots whose object reads back otherwise, or whose file differs from its listing, by file, as they were
    /// listed then: the host keeps them, and they are not tried again while so listed.
    refused: BTreeMap<String, Listed>,
    /// When each snapshot waiting for its confirmation was first seen waiting.
    waiting: BTreeMap<String, Instant>,
    /// Whether `archive_upload_lagging` is raised for the snapshots; unknown at a start, since the alert router keeps an
    /// episode open across a restart, so the first check that finds nothing waiting resolves it.
    raised: Option<bool>,
    /// Whether `daily_snapshot_unremovable` is raised; unknown at a start, as `raised`.
    unremovable: Option<bool>,
}

/// What an attempt at a snapshot found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// Its object holds the bytes its listing names.
    Confirmed,
    /// Its object holds other bytes, or its file differs from its listing: it stays on the host.
    Refused,
}

/// A snapshot's file as it streams into an upload.
struct Reading {
    file: tokio::fs::File,
    hash: Sha256,
    /// The bytes of the listing not yet read.
    left: u64,
    /// The chunk read last, sent only once the next is read, or, the last, once the whole matches the listing.
    held: Option<Vec<u8>>,
    sha256: String,
    /// Set when the file turns out to differ from its listing.
    damaged: Arc<AtomicBool>,
    /// Whether the last chunk has gone.
    done: bool,
}

impl SnapshotUploader {
    /// The uploader of `cfg`'s `[daily_snapshots]` to the bucket of its `[archive]`, with the keys `values` holds; none
    /// without either section or either key, whose absence the start raised as `secret_missing`.
    /// Error cases:
    /// - the bucket's client or URL does not build, or the custody vault does not parse
    pub(crate) fn for_config(
        cfg: &NodeConfig,
        values: &Values,
        metrics: &Arc<Metrics>,
    ) -> anyhow::Result<Option<Self>> {
        let (Some(daily), Some(section)) = (&cfg.daily_snapshots, &cfg.archive) else { return Ok(None) };
        let keys = (values.get(&section.access_key_id_secret), values.get(&section.secret_access_key_secret));
        let (Some(key_id), Some(secret)) = keys else { return Ok(None) };
        let store = Store::new(section, (key_id, secret), Store::client()?)?;
        let prefix = snapshots_prefix(cfg.custody.chain_id, cfg.custody.vault.parse().context("custody.vault")?);
        let timing = (daily.local_window, section.lag_alert);
        Ok(Some(Self::new((store, prefix), daily.dir.clone(), timing, metrics.clone())))
    }

    /// An uploader of the snapshots in `dir` to `store` under the log's key `prefix`, keeping each on the host for
    /// `local_window`, and warning once one has waited `lag_alert` for its confirmation.
    pub(crate) fn new(
        (store, prefix): (Store, String),
        dir: PathBuf,
        (local_window, lag_alert): (Duration, Duration),
        metrics: Arc<Metrics>,
    ) -> Self {
        let window_ms = Millis::try_from(local_window.as_millis()).unwrap_or(Millis::MAX);
        let (confirmed, refused, waiting) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
        let (raised, unremovable) = (None, None);
        Self { store, prefix, dir, window_ms, lag_alert, metrics, confirmed, refused, waiting, raised, unremovable }
    }

    /// Uploads and removes in passes for the node's life, every `POLL_EVERY`, or after a failed pass with backoff.
    pub(crate) async fn run(mut self) -> anyhow::Result<()> {
        let mut retry = FIRST_RETRY;
        loop {
            match self.pass().await {
                Ok(()) => {
                    retry = FIRST_RETRY;
                    sleep(POLL_EVERY).await;
                }
                Err(e) => {
                    self.metrics.snapshot_archive.failures.inc();
                    warn!("a pass over the daily snapshots' archive failed, and is retried in {retry:?}: {e:#}");
                    sleep(retry).await;
                    retry = (retry * 2).min(MAX_RETRY);
                }
            }
        }
    }

    /// One pass: reads the manifest, confirms in the bucket each listed snapshot the host holds that this process
    /// has not, newest first, until one fails, removes the host's copies the window no longer keeps, and checks how
    /// long a snapshot has waited.
    /// Error cases:
    /// - the manifest cannot be read, a file cannot be read, the bucket cannot take or serve a snapshot, or a copy
    ///   cannot be removed
    pub(crate) async fn pass(&mut self) -> anyhow::Result<()> {
        let dir = self.dir.clone();
        let manifest = spawn_blocking(move || read_manifest(&dir)).await?;
        let read = manifest.is_ok();
        let passed = match manifest {
            Ok(listed) => self.go_through(&listed).await,
            Err(e) => Err(e),
        };
        // A start whose manifest cannot be read cannot see what waits, and leaves an earlier process's episodes as they are.
        if read || self.raised.is_some() {
            self.check();
        }
        passed
    }

    /// Confirms in the bucket each snapshot of `listed` the host holds that this process has not, newest first, until
    /// one fails, checking before each how long the oldest has waited, then removes the host's copies the window no
    /// longer keeps.
    /// Error cases:
    /// - as `pass`, but for the manifest
    async fn go_through(&mut self, listed: &[Listed]) -> anyhow::Result<()> {
        // A listing the manifest no longer names, as one a start drops past the log's last checkpoint, is forgotten.
        let files: BTreeSet<&str> = listed.iter().map(|listing| listing.file.as_str()).collect();
        self.confirmed.retain(|file, _| files.contains(file.as_str()));
        self.refused.retain(|file, _| files.contains(file.as_str()));
        // A name its offset does not give is no path or key of a snapshot's, as one the host wrote over the manifest.
        for listing in listed.iter().filter(|listing| !canonical(listing)) {
            if self.refused.insert(listing.file.clone(), listing.clone()).as_ref() != Some(listing) {
                warn!(
                    alert = "daily_snapshot_unreadable",
                    snapshot = %listing.file,
                    "the manifest lists a daily snapshot under a name its offset does not give, so it does not go up to \
                     the archive"
                );
            }
        }
        // Every snapshot to confirm waits from when it was first seen waiting, before any transfer of the pass.
        let mut due = Vec::new();
        for listing in listed.iter().rev() {
            let known = |taken: &BTreeMap<String, Listed>| taken.get(&listing.file) == Some(listing);
            if !known(&self.confirmed)
                && !known(&self.refused)
                && tokio::fs::try_exists(self.dir.join(&listing.file)).await?
            {
                due.push(listing);
            }
        }
        let waits: BTreeSet<&str> = due.iter().map(|listing| listing.file.as_str()).collect();
        self.waiting.retain(|file, _| waits.contains(file.as_str()));
        for listing in &due {
            self.waiting.entry(listing.file.clone()).or_insert_with(Instant::now);
        }
        let mut failed = None;
        for listing in due {
            self.check();
            let span = info_span!("archive_snapshot", id = %self.key(listing));
            match self.archive(listing).instrument(span).await {
                Ok(outcome) => {
                    self.waiting.remove(&listing.file);
                    let taken = match outcome {
                        Outcome::Confirmed => &mut self.confirmed,
                        Outcome::Refused => &mut self.refused,
                    };
                    taken.insert(listing.file.clone(), listing.clone());
                }
                // The rest wait for the next pass, which a failure delays.
                Err(e) => {
                    failed = Some(e.context(format!("daily snapshot {}", listing.file)));
                    break;
                }
            }
        }
        let removed = self.remove_aged(listed).await;
        match (failed, removed) {
            (Some(e), _) | (None, Err(e)) => Err(e),
            (None, Ok(())) => Ok(()),
        }
    }

    /// The object key of `listing`'s snapshot.
    fn key(&self, listing: &Listed) -> String {
        format!("{}{}", self.prefix, listing.file)
    }

    /// Uploads `listing`'s snapshot unless the bucket holds it, then reads it back against the listing.
    /// Error cases:
    /// - the file cannot be opened, or the bucket cannot take or serve the snapshot, or serves none
    async fn archive(&mut self, listing: &Listed) -> anyhow::Result<Outcome> {
        let key = self.key(listing);
        if !self.store.holds(&key).await? {
            let damaged = Arc::new(AtomicBool::new(false));
            let body = checked_body(&self.dir.join(&listing.file), listing, damaged.clone()).await?;
            match self.store.create_from(&key, (body, listing.bytes), JSON, TRANSFER_TIMEOUT).await {
                Ok(created) => info!(?created, bytes = listing.bytes, "uploaded a daily snapshot to the archive"),
                Err(_) if damaged.load(Ordering::Acquire) => {
                    warn!(
                        alert = "daily_snapshot_unreadable",
                        snapshot = %listing.file,
                        "the daily snapshot's file differs from its listing, so it does not go up to the archive, and \
                         the host keeps it"
                    );
                    return Ok(Outcome::Refused);
                }
                Err(e) => return Err(e),
            }
        }
        let read = self.store.digest(&key, listing.bytes, TRANSFER_TIMEOUT).await?;
        match read {
            Digested::Absent => Err(anyhow!("{key} is not in the bucket")),
            Digested::Read { bytes, sha256 } if bytes == listing.bytes && hex::encode(sha256) == listing.sha256 => {
                self.metrics.snapshot_archive.confirmed.inc();
                info!(seq = listing.seq, "confirmed a daily snapshot in the archive");
                Ok(Outcome::Confirmed)
            }
            Digested::Read { .. } | Digested::Oversized => {
                self.metrics.snapshot_archive.mismatches.inc();
                warn!(
                    alert = "archive_object_mismatch",
                    object = %key,
                    seq = listing.seq,
                    "the archive's object for this daily snapshot reads back as other bytes than its listing names, \
                     and cannot be replaced, so the host keeps its copy: compare the two"
                );
                Ok(Outcome::Refused)
            }
        }
    }

    /// Removes from the host each confirmed snapshot of `listed` stamped more than the window before the newest, but
    /// for the newest such one.
    /// Error cases:
    /// - a file cannot be removed, or the directory synced
    async fn remove_aged(&mut self, listed: &[Listed]) -> anyhow::Result<()> {
        let listed: Vec<&Listed> = listed.iter().filter(|listing| canonical(listing)).collect();
        let Some(newest) = listed.iter().map(|listing| listing.ts).max() else {
            self.resolve_unremovable();
            return Ok(());
        };
        let aged: Vec<&Listed> =
            listed.into_iter().filter(|listing| newest.saturating_sub(listing.ts) > self.window_ms).collect();
        let anchor = aged.iter().copied().max_by_key(|listing| (listing.ts, listing.seq, listing.index));
        let gone: Vec<Listed> = aged
            .into_iter()
            .filter(|listing| Some(*listing) != anchor && self.confirmed.get(&listing.file) == Some(*listing))
            .cloned()
            .collect();
        for listing in &gone {
            // A removal that fails is tried again at the next pass, the snapshot still confirmed.
            match tokio::fs::remove_file(self.dir.join(&listing.file)).await {
                Ok(()) => {
                    self.metrics.snapshot_archive.removed.inc();
                    info!(
                        seq = listing.seq,
                        file = %listing.file,
                        "removed a daily snapshot from the host: the archive keeps it"
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    if self.unremovable.replace(true) != Some(true) {
                        warn!(
                            alert = "daily_snapshot_unremovable",
                            "daily snapshot {} is in the archive and older than the host's window, but cannot be \
                             removed from the host, so the host's snapshots grow past their bound: {e}",
                            listing.file
                        );
                    }
                    return Err(e).with_context(|| format!("removing daily snapshot {}", listing.file));
                }
            }
            self.confirmed.remove(&listing.file);
        }
        if !gone.is_empty() {
            let dir = self.dir.clone();
            spawn_blocking(move || sync_dir(&dir)).await??;
        }
        self.resolve_unremovable();
        Ok(())
    }

    /// Resolves `daily_snapshot_unremovable` unless this process knows it is not raised: a start resolves it once, ending
    /// an episode an earlier process left open, which the router drops when none is.
    fn resolve_unremovable(&mut self) {
        if self.unremovable.replace(false) != Some(false) {
            info!(alert_resolved = "daily_snapshot_unremovable", "the host keeps only the window's daily snapshots");
        }
    }

    /// Sets the metrics, and raises `archive_upload_lagging` for the snapshots once one has waited `lag_alert` for its
    /// confirmation, or resolves it once none has.
    fn check(&mut self) {
        self.check_at(Instant::now());
    }

    /// `check` with the time `now`.
    fn check_at(&mut self, now: Instant) {
        let waited = self.waiting.values().min().map(|since| now.saturating_duration_since(*since));
        let pending = self.waiting.len();
        self.metrics.snapshot_archive.pending.set(i64::try_from(pending).unwrap_or(i64::MAX));
        match waited {
            Some(waited) if waited >= self.lag_alert && self.raised != Some(true) => {
                self.raised = Some(true);
                warn!(
                    alert = "archive_upload_lagging",
                    objects = "daily_snapshots",
                    "{pending} daily snapshots wait for the archive, the oldest for {waited:?}: the host keeps them; \
                     check the bucket, its keys and the node's route to it"
                );
            }
            Some(waited) if waited >= self.lag_alert => {}
            // Unknown at a start: resolved only once nothing waits, as an earlier process's episode may still hold.
            _ if self.raised == Some(true) || (self.raised.is_none() && waited.is_none()) => {
                self.raised = Some(false);
                info!(
                    alert_resolved = "archive_upload_lagging",
                    objects = "daily_snapshots",
                    "the archive holds every daily snapshot that waited"
                );
            }
            _ => {}
        }
    }
}

/// A body streaming the file at `path` as `listing` names it, that sends its last chunk only once the whole has the
/// listing's length and SHA-256, and otherwise ends in an error and sets `damaged`, so the bucket creates nothing
/// from a file that differs from its listing.
/// Error cases:
/// - the file cannot be opened
async fn checked_body(path: &Path, listing: &Listed, damaged: Arc<AtomicBool>) -> anyhow::Result<reqwest::Body> {
    let file = tokio::fs::File::open(path).await.with_context(|| format!("opening {}", path.display()))?;
    let reading = Reading {
        file,
        hash: Sha256::new(),
        left: listing.bytes,
        held: None,
        sha256: listing.sha256.clone(),
        damaged,
        done: false,
    };
    Ok(reqwest::Body::wrap_stream(try_unfold(reading, |mut reading| async move {
        loop {
            if reading.done {
                return Ok(None);
            }
            if reading.left == 0 {
                reading.done = true;
                let longer = reading.file.read(&mut [0u8; 1]).await? > 0;
                let hash = hex::encode(std::mem::take(&mut reading.hash).finalize());
                if longer || hash != reading.sha256 {
                    reading.damaged.store(true, Ordering::Release);
                    return Err(io::Error::other("the daily snapshot's file differs from its listing"));
                }
                let last = reading.held.take();
                return Ok(last.map(|chunk| (chunk, reading)));
            }
            let mut chunk = vec![0; usize::try_from(reading.left).map_or(CHUNK_BYTES, |left| left.min(CHUNK_BYTES))];
            let read = reading.file.read(&mut chunk).await?;
            if read == 0 {
                reading.damaged.store(true, Ordering::Release);
                return Err(io::Error::other("the daily snapshot's file is shorter than its listing"));
            }
            chunk.truncate(read);
            reading.hash.update(&chunk);
            reading.left -= read as u64;
            if let Some(previous) = reading.held.replace(chunk) {
                return Ok(Some((previous, reading)));
            }
        }
    })))
}

#[cfg(test)]
mod tests;
