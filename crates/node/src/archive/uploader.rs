//! The archive's uploader: for the node's life, it follows the manifest of the log writer in use and puts each
//! whole range below the vault's finalized checkpoint into the bucket in order, confirming each by a read from the
//! bucket's public origin. It takes a finalized checkpoint as a bound only once the writer's copy holds it at the
//! vault's root, so a copy that holds another history past an earlier checkpoint uploads none of it, and raises
//! Warning `archive_copy_superseded`. A failure is retried with backoff at the same range, so the bucket's ranges stay a
//! prefix of the log. A range whose object reads back as other bytes than the node's copy builds is never
//! confirmed and raises Warning `archive_object_mismatch`; since nothing may replace the object, the node uploads
//! nothing past it rather than leave its own copy's later ranges beside a log the bucket holds otherwise.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};
use tokio::sync::watch;
use tokio::task::spawn_blocking;
use tokio::time::{Instant, sleep};
use tracing::{Instrument, info, info_span, warn};

use crate::archive::backlog::Backlog;
use crate::archive::{
    Created, Finality, Object, RANGE_SEQS, Store, ZSTD, checkpoint_root, compress, holds, log_prefix, range_bytes,
    range_key, same_root, stored_bound, whole_ranges,
};
use crate::checkpoints::CheckpointRpc;
use crate::config::{NodeConfig, Values};
use crate::log::Manifest;
use crate::metrics::Metrics;

/// The wait after a failure; each failure in a row doubles it, up to `MAX_RETRY`.
const FIRST_RETRY: Duration = if cfg!(test) { Duration::from_millis(20) } else { Duration::from_secs(1) };
const MAX_RETRY: Duration = if cfg!(test) { Duration::from_millis(200) } else { Duration::from_secs(300) };
/// How often the vault's finalized checkpoint is read while a whole range waits for it.
const FINALITY_EVERY: Duration = if cfg!(test) { Duration::from_millis(20) } else { Duration::from_secs(60) };
/// How often the backlog is checked while the uploader waits on its writer, so a quiet writer still raises
/// `archive_upload_lagging`.
const RECHECK_EVERY: Duration = if cfg!(test) { Duration::from_millis(20) } else { Duration::from_secs(60) };

/// The manifest a log writer publishes, the authority on what each of its segments holds.
pub(crate) type Source = watch::Receiver<Manifest>;
/// Where each log writer the node runs hands the uploader its manifest: the serving sequencer's, and a
/// following member's copy's. They write the same directory, one at a time.
pub(crate) type Sources = watch::Sender<Option<Source>>;

/// The uploader, and the manifests it is handed.
pub(crate) struct Uploader<F = CheckpointRpc> {
    sources: watch::Receiver<Option<Source>>,
    work: Work<F>,
}

/// What the uploader works with across sources.
struct Work<F> {
    store: Store,
    /// The key prefix of this log's objects (`log_prefix`).
    prefix: String,
    dir: PathBuf,
    metrics: Arc<Metrics>,
    backlog: Backlog,
    /// The vault's finality; without a vault, every whole range may go up.
    finality: Option<F>,
    /// The finalized checkpoints the current copy is checked against.
    anchor: Anchor,
    /// When the vault's finality was last read.
    read_at: Option<Instant>,
    /// The ranges already reported as mismatched.
    mismatched: BTreeSet<u64>,
    /// The first range whose object reads back otherwise: the uploads stop there whatever source follows.
    stalled_at: Option<u64>,
    /// The range before the first the bucket lacks, when the resume left it unread because the copy does not
    /// hold it below a finalized checkpoint at the vault's root yet; it is read back before the next goes up.
    unread: Option<u64>,
}

/// The vault's finalized checkpoints, as the copy in use holds them.
#[derive(Default)]
struct Anchor {
    /// The newest finalized checkpoint the copy holds at the vault's root: ranges below it may go up.
    held: Option<(u64, String)>,
    /// A newer finalized checkpoint, read from the vault, that the copy's completed segments do not hold yet.
    pending: Option<(u64, String)>,
    /// A finalized checkpoint the copy holds otherwise, or not at all: nothing past `held` goes up from it.
    differs: Option<u64>,
    /// Whether `archive_copy_superseded` is raised, for this copy or an earlier one.
    raised: bool,
}

/// What a read of a range's object found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Found {
    /// The object holds the node's copy of the range.
    Confirmed,
    /// The object holds something else, and stays: the uploads stop at it.
    Mismatched,
}

impl Uploader<CheckpointRpc> {
    /// The uploader for `cfg`'s `[archive]` with the keys `values` holds, reading the vault's finality through
    /// `[checkpoints]`, and where writers hand it their manifests; none without the section or either key, whose
    /// absence the start raised as `secret_missing`.
    /// Error cases:
    /// - the bucket's client or URL, or the vault's reader, does not build
    pub(crate) fn for_config(
        cfg: &NodeConfig,
        values: &Values,
        metrics: &Arc<Metrics>,
    ) -> anyhow::Result<Option<(Self, Sources)>> {
        let Some(section) = &cfg.archive else { return Ok(None) };
        let keys = (values.get(&section.access_key_id_secret), values.get(&section.secret_access_key_secret));
        let (Some(key_id), Some(secret)) = keys else { return Ok(None) };
        let store = Store::new(section, (key_id, secret), Store::client()?)?;
        let prefix = log_prefix(cfg.custody.chain_id, cfg.custody.vault.parse().context("custody.vault")?);
        let finality = crate::handover::vault_reader(cfg)?;
        let (dir, lag) = (cfg.node.log_dir.clone(), section.lag_alert);
        Ok(Some(Self::new((store, prefix), dir, lag, metrics.clone(), finality)))
    }
}

impl<F: Finality> Uploader<F> {
    /// An uploader of the log in `dir` to `store` under the log's key `prefix`, below the vault's `finality` where
    /// there is one, warning once a range has waited `lag_alert`.
    pub(crate) fn new(
        (store, prefix): (Store, String),
        dir: PathBuf,
        lag_alert: Duration,
        metrics: Arc<Metrics>,
        finality: Option<F>,
    ) -> (Self, Sources) {
        let (sources, handed) = watch::channel(None);
        let backlog = Backlog::new(lag_alert);
        let (anchor, read_at, mismatched, stalled_at) = (Anchor::default(), None, BTreeSet::new(), None);
        let work = Work {
            store,
            prefix,
            dir,
            metrics,
            backlog,
            finality,
            anchor,
            read_at,
            mismatched,
            stalled_at,
            unread: None,
        };
        (Self { sources: handed, work }, sources)
    }

    /// Uploads from each manifest handed to it in turn, until its sources close.
    /// Error cases:
    /// - every sender of its sources is gone
    pub(crate) async fn run(mut self) -> anyhow::Result<()> {
        loop {
            let source = self.sources.borrow_and_update().clone();
            if let Some(source) = source {
                // A writer that stops, as at a handover, leaves its manifest closed until the next one arrives.
                tokio::select! {
                    () = self.work.follow(source) => {}
                    changed = self.sources.changed() => {
                        changed.context("the archive's sources closed")?;
                        continue;
                    }
                }
            }
            self.sources.changed().await.context("the archive's sources closed")?;
        }
    }
}

impl<F: Finality> Work<F> {
    /// Uploads what `manifest`'s writer makes whole, until the writer is gone.
    async fn follow(&mut self, mut manifest: Source) {
        // Until its writer has read the log, a manifest lists nothing, and would resume at the first range.
        if manifest.wait_for(|view| view.authority.is_some()).await.is_err() {
            return;
        }
        let mut retry = FIRST_RETRY;
        // Another copy is checked afresh, first against the checkpoint the last one held.
        self.anchor = Anchor { pending: self.anchor.held.take(), raised: self.anchor.raised, ..Anchor::default() };
        self.read_at = None;
        self.backlog.resuming();
        let mut next = loop {
            let view = manifest.borrow().clone();
            match self.resume(&view).await {
                Ok(found) => break found,
                Err(e) => {
                    self.backlog.check(&self.metrics);
                    self.failed(&e, &mut retry).await;
                }
            }
        };
        // A range found otherwise before stays the end, whatever another node has uploaded past it since.
        next = self.stalled_at.map_or(next, |at| next.min(at));
        info!(ranges = next, stalled = ?self.stalled_at, "the archive holds the log's ranges up to here");
        self.backlog.restart(next);
        loop {
            let view = manifest.borrow_and_update().clone();
            let whole = match self.backlog.observe(&view) {
                Ok(whole) => whole,
                Err(e) => {
                    self.failed(&e, &mut retry).await;
                    continue;
                }
            };
            self.backlog.check(&self.metrics);
            if self.stalled_at.is_some() || next >= whole {
                if !wait(&mut manifest, RECHECK_EVERY).await {
                    return;
                }
                continue;
            }
            let span = info_span!("archive_range", id = %range_key(&self.prefix, next));
            let step = match self.below_finality(&view, next).instrument(span.clone()).await {
                Ok(below) if next < below => self.upload(&view, next).instrument(span.clone()).await.map(|found| {
                    if found == Found::Confirmed {
                        next += 1;
                    }
                }),
                Ok(_) => {
                    // The range waits for the vault, read again when due, or for the copy to complete the segment
                    // holding the vault's checkpoint, or for another copy; the writer's change comes first.
                    let due = match self.anchor.pending.is_some() || self.anchor.differs.is_some() {
                        true => RECHECK_EVERY,
                        false => self.read_at.map_or(Duration::ZERO, |at| FINALITY_EVERY.saturating_sub(at.elapsed())),
                    };
                    if !wait(&mut manifest, due).await {
                        return;
                    }
                    continue;
                }
                Err(e) => Err(e),
            };
            match step {
                Ok(()) => retry = FIRST_RETRY,
                Err(e) => {
                    self.backlog.check(&self.metrics);
                    self.failed(&e, &mut retry).instrument(span).await;
                }
            }
        }
    }

    /// The first range the bucket lacks, by a binary search of the whole ranges' keys: the uploader puts them in
    /// order, so they are a prefix. A stale `404` from a cache in front of the bucket only moves the search back,
    /// and the ranges after it go up again as objects already there. The range before the first it lacks is read
    /// back once the copy holds it below a finalized checkpoint at the vault's root, so a node whose copy of it
    /// differs stops there rather than upload past it, and a copy that holds another history past that checkpoint,
    /// as a returning operator's superseded tail does, is not read against the successor's objects.
    /// Error cases:
    /// - a range cannot be built, or the bucket cannot be asked
    async fn resume(&mut self, view: &Manifest) -> anyhow::Result<u64> {
        let (mut low, mut high) = (0, whole_ranges(view)?);
        while low < high {
            let middle = low + (high - low) / 2;
            match self.store.holds(&range_key(&self.prefix, middle)).await? {
                true => low = middle + 1,
                false => high = middle,
            }
        }
        self.unread = None;
        let Some(last) = low.checked_sub(1) else { return Ok(0) };
        if last >= self.below_finality(view, last).await? {
            self.unread = Some(last);
            return Ok(low);
        }
        match self.read_back(view, last).await? {
            Found::Confirmed => Ok(low),
            Found::Mismatched => Ok(last),
        }
    }

    /// Uploads range `index`, after reading back the range before it if the resume left that unread.
    /// Error cases:
    /// - see `range` and `read_back`
    async fn upload(&mut self, view: &Manifest, index: u64) -> anyhow::Result<Found> {
        if let Some(last) = self.unread {
            let found = self.read_back(view, last).await?;
            self.unread = None;
            if found == Found::Mismatched {
                return Ok(found);
            }
        }
        self.range(view, index).await
    }

    /// Reads range `index`'s object back against the node's copy of it.
    /// Error cases:
    /// - see `build` and `check`
    async fn read_back(&mut self, view: &Manifest, index: u64) -> anyhow::Result<Found> {
        let raw = self.build(view, index).await?;
        self.check(index, raw).await
    }

    /// How many ranges, from the first, end below the newest finalized checkpoint that `view`'s copy holds at the
    /// vault's root, every range without a vault. Once range `next` reaches that bound, a newer finalized checkpoint
    /// is read from the vault when a read is due, and taken as the bound once the copy's completed segments hold it
    /// at the vault's root; held otherwise, or not at all, it raises `archive_copy_superseded` and the bound stays.
    /// Error cases:
    /// - the vault cannot be read, or the copy's segment holding the checkpoint cannot be
    async fn below_finality(&mut self, view: &Manifest, next: u64) -> anyhow::Result<u64> {
        let Some(finality) = &self.finality else { return Ok(u64::MAX) };
        let below = |anchor: &Anchor| anchor.held.as_ref().map_or(0, |(seq, _)| seq / RANGE_SEQS);
        if next < below(&self.anchor) || self.anchor.differs.is_some() {
            return Ok(below(&self.anchor));
        }
        if self.anchor.pending.is_none() && self.read_at.is_none_or(|at| at.elapsed() >= FINALITY_EVERY) {
            self.read_at = Some(Instant::now());
            let finalized = finality.finalized().await.context("reading the vault's finalized checkpoint")?;
            let held = self.anchor.held.as_ref().map(|(seq, _)| *seq);
            self.anchor.pending = finalized.filter(|(seq, _)| Some(*seq) > held);
        }
        let Some((seq, root)) = self.anchor.pending.clone() else { return Ok(below(&self.anchor)) };
        if seq >= view.unlisted_from()? {
            return Ok(below(&self.anchor));
        }
        let (dir, read) = (self.dir.clone(), view.clone());
        let copy = spawn_blocking(move || checkpoint_root(&dir, &read, seq)).await??;
        match copy.as_deref().is_some_and(|copy| same_root(copy, &root)) {
            true => {
                self.anchor.held = self.anchor.pending.take();
                if std::mem::take(&mut self.anchor.raised) {
                    info!(
                        alert_resolved = "archive_copy_superseded",
                        "this node's copy of the log holds the vault's finalized checkpoint at seq {seq}"
                    );
                }
            }
            false => self.superseded(seq, &root, copy.as_deref()),
        }
        Ok(below(&self.anchor))
    }

    /// Reports the copy in use as holding the vault's finalized checkpoint at `seq`, rooted at `root`, otherwise
    /// (`copy`), and keeps its uploads below the checkpoint it last held.
    fn superseded(&mut self, seq: u64, root: &str, copy: Option<&str>) {
        (self.anchor.differs, self.anchor.raised) = (Some(seq), true);
        let held = self.anchor.held.as_ref().map(|(seq, _)| *seq);
        // The seqs and roots are in the message, not fields, which key the alert's episode: the resolution
        // carries none, and ends the episode whatever checkpoint the copy was raised for.
        warn!(
            alert = "archive_copy_superseded",
            "the vault finalized the checkpoint at seq {seq} with root {root}, and this node's copy of the log holds \
             root {} there, so it uploads none of its ranges past the last finalized checkpoint it holds, seq \
             {held:?}: a member sets a superseded tail aside as it follows the operator, and the uploads go on from \
             the copy it rebuilds",
            copy.unwrap_or("none")
        );
    }

    /// Uploads range `index`, and reads it back.
    /// Error cases:
    /// - the range cannot be built, or the bucket cannot take or serve it
    async fn range(&mut self, view: &Manifest, index: u64) -> anyhow::Result<Found> {
        let raw = self.build(view, index).await?;
        let kept = raw.clone();
        let stored = spawn_blocking(move || compress(&kept)).await??;
        let (key, length) = (range_key(&self.prefix, index), stored.len() as u64);
        let created = self.store.create(&key, stored, ZSTD).await?;
        if created == Created::New {
            self.metrics.archive.objects_created.inc();
            self.metrics.archive.stored_bytes.inc_by(length);
        }
        let bytes = raw.len() as u64;
        let found = self.check(index, raw).await?;
        if found == Found::Confirmed {
            self.metrics.archive.confirmed_bytes.inc_by(bytes);
            self.backlog.passed();
            info!(?created, bytes, stored = length, "confirmed in the archive");
        }
        Ok(found)
    }

    /// Range `index` as the node's copy holds it, read off the runtime.
    /// Error cases:
    /// - see `range_bytes`
    async fn build(&self, view: &Manifest, index: u64) -> anyhow::Result<Arc<Vec<u8>>> {
        let (dir, read) = (self.dir.clone(), view.clone());
        Ok(Arc::new(spawn_blocking(move || range_bytes(&dir, &read, index)).await??))
    }

    /// Whether range `index`'s object holds `raw`, the node's copy of it, reporting a mismatch.
    /// Error cases:
    /// - the bucket cannot serve the object, or serves none
    async fn check(&mut self, index: u64, raw: Arc<Vec<u8>>) -> anyhow::Result<Found> {
        let key = range_key(&self.prefix, index);
        let found = match self.store.read(&key, stored_bound(&raw).saturating_mul(2)).await? {
            Object::Absent => return Err(anyhow!("{key} is not in the bucket")),
            Object::Oversized => Found::Mismatched,
            Object::Read(object) => match spawn_blocking(move || holds(&object, &raw)).await? {
                true => Found::Confirmed,
                false => Found::Mismatched,
            },
        };
        if found == Found::Mismatched {
            self.mismatch(index);
        }
        Ok(found)
    }

    /// Reports range `index`'s object as other than the node's copy, once, and stops the uploads there.
    fn mismatch(&mut self, index: u64) {
        self.stalled_at = Some(self.stalled_at.map_or(index, |at| at.min(index)));
        self.metrics.archive.mismatches.inc();
        if self.mismatched.insert(index) {
            warn!(
                alert = "archive_object_mismatch",
                object = %range_key(&self.prefix, index),
                first_seq = index * RANGE_SEQS,
                "the archive's object for this range differs from this node's copy of it and cannot be replaced, so \
                 this node uploads nothing past it and its host keeps the log: compare the two"
            );
        }
    }

    /// Records a failed step, and waits `retry` before the next, doubling it up to `MAX_RETRY`.
    async fn failed(&self, e: &anyhow::Error, retry: &mut Duration) {
        self.metrics.archive.failures.inc();
        warn!("an upload to the archive failed, and is retried in {retry:?}: {e:#}");
        sleep(*retry).await;
        *retry = (*retry * 2).min(MAX_RETRY);
    }
}

/// Waits for `manifest`'s writer to change it, or `at_most`; `false` once the writer is gone.
async fn wait(manifest: &mut Source, at_most: Duration) -> bool {
    tokio::select! {
        changed = manifest.changed() => changed.is_ok(),
        () = sleep(at_most) => true,
    }
}

#[cfg(test)]
mod tests;
