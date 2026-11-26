//! The log's local window (`[archive] local_window`): the host keeps the segments whose commands fall within the
//! window of sequenced time, counted back from the newest, and every segment the window must not let go; the rest
//! live only in the archive's bucket, which every reader reads them from (`archive::read`, and the archive route's
//! `308`). It runs for the node's life beside the uploader, on whichever log writer is in use.
//!
//! A completed segment leaves the host, oldest first, only when all of these hold:
//! - every range it holds seqs of is below the uploader's count of ranges the bucket holds (`Uploader::archived`),
//!   which the window waits for after a start, so a segment holding a seq of a range not yet whole or not yet
//!   finalized stays, whatever else holds its commands, such as a checkpoint's tail (`uploader::tails`): a log that
//!   stops for good keeps its last segments, whose last range never fills;
//! - every command of it is older than the window: the first command of the segment after it is stamped at least the
//!   window before the newest segment's first command, a lower bound of the log's sequenced time, so the host's clock
//!   brings nothing forward;
//! - it ends before the checkpoint of the newest daily snapshot a start may go on from, and before that of the newest
//!   stamped before the history index's retention, from which the index re-derives a retained command's rows
//!   (`bounds::Floors`);
//! - a read of its seqs from the bucket's public origin, made just before it leaves, gives back the bytes its listing
//!   names (`restore::Sources::read_back`);
//! - no reader of the log inside the node holds it (`log::Holds`), checked under the lock its removal holds; a stream
//!   client's backfill holds none (`Feed::unheld`).
//!
//! A listed segment the host must keep and lacks, as one the host lost, is put back from the bucket or a node origin,
//! as its listing names it, newest first, so the host's segments stay one run; one no source gives back raises
//! Critical `log_segment_missing`. Each alert's state is unknown at a start, since the alert router keeps an episode
//! open across a restart, so the first pass that finds it over resolves it. A segment a reader or the bucket's read-back holds more than `HELD_ALERT` past the
//! window raises Warning `log_window_held`; the archive's wait is `archive_upload_lagging`'s, and the daily snapshots'
//! and the history's holds are the node's own, as long as the gap between the snapshots it holds, which on a rotation
//! member are its own turns' and those it fetched. Passes that keep failing for an hour, as over a directory that no
//! longer holds the newest segment its writer names, raise Warning `log_window_failing`. With `[snapshots]`, whose
//! history index rebuilds from the whole log, the window does not run.

mod bounds;
pub(crate) use bounds::{Checked, Floors, FreshStart, Held, Stamps, fresh_start, millis};
mod report;
mod restore;
pub(crate) use restore::{Sources as RestoreSources, history_start, restore_whole};

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use engine_core::types::Millis;
use tokio::sync::watch;
use tokio::task::spawn_blocking;
use tokio::time::{Instant, sleep};
use tracing::{debug, info, warn};

use crate::archive::{RANGE_SEQS, Sources, Uploader};
use crate::config::{HistorySection, NodeConfig};
use crate::log::{Manifest, SegmentInfo, list_segments};
use crate::metrics::Metrics;

/// How often the window looks at the log's segments on the host.
const PASS_EVERY: Duration = if cfg!(test) { Duration::from_millis(20) } else { Duration::from_secs(60) };
/// The most segments one pass removes, so a first pass over a long log does not hold back the next for long.
const MAX_REMOVALS: usize = 64;
/// The most segments one pass tries to put back, so no pass writes much of the log at once, nor asks for long.
const MAX_RESTORES: usize = 16;
/// How long, in sequenced time, a reader or the bucket's read-back may hold a segment past the window before
/// `log_window_held`.
const HELD_ALERT: Duration = Duration::from_secs(2 * 86_400);
/// How long the passes may keep failing before `log_window_failing`.
const FAILING_ALERT: Duration = if cfg!(test) { Duration::from_millis(100) } else { Duration::from_secs(3600) };

/// The local window, following the log writers the uploader follows.
pub(crate) struct Window {
    dir: PathBuf,
    window: Millis,
    snapshots: PathBuf,
    history: HistorySection,
    sources: watch::Receiver<Option<watch::Receiver<Manifest>>>,
    archived: watch::Receiver<Option<u64>>,
    restore: RestoreSources,
    metrics: Arc<Metrics>,
    stamps: Stamps,
    checked: Checked,
    /// Whether `log_segment_missing`, `log_window_held` and `log_window_failing` are raised; each unknown at a start,
    /// since the alert router keeps an episode open across a restart, so the first pass that finds it over resolves it.
    raised: Raised,
    /// Since when the passes have failed, if the last did.
    failing: Option<Instant>,
}

/// Whether each of the window's alerts is raised, none when not known.
#[derive(Default)]
struct Raised {
    missing: Option<bool>,
    held: Option<bool>,
    failing: Option<bool>,
}

/// What a pass found of the segments on the host.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Pass {
    /// The segments it removed, and those it put back, by file.
    pub removed: Vec<String>,
    pub restored: Vec<String>,
    /// What holds the oldest segment left, if one is listed.
    pub held: Option<Held>,
}

impl Window {
    /// The window `cfg`'s `[archive] local_window` sets, beside `uploader`, following the writers `sources` hands it;
    /// none without the window, or with `[snapshots]`, whose history index rebuilds from the whole log.
    /// Error cases:
    /// - `[daily_snapshots]` is absent, or the bucket's URL or client does not build
    pub(crate) fn for_config(
        cfg: &NodeConfig,
        uploader: &Uploader,
        sources: &Sources,
        metrics: &Arc<Metrics>,
    ) -> anyhow::Result<Option<Self>> {
        let Some(window) = cfg.archive.as_ref().and_then(|archive| archive.local_window) else { return Ok(None) };
        if cfg.snapshots.is_some() {
            warn!(
                "archive.local_window does not apply with [snapshots], whose history index rebuilds from the whole \
                 log: the host keeps every segment"
            );
            return Ok(None);
        }
        let snapshots = cfg.daily_snapshots.as_ref().context("archive.local_window needs [daily_snapshots]")?;
        let restore = RestoreSources::for_config(cfg)?.context("archive.local_window needs [archive]")?;
        let history = HistorySection { dir: cfg.history.dir.clone(), retention: cfg.history.retention, marks: None };
        let window = Self::new(cfg.node.log_dir.clone(), millis(window), snapshots.dir.clone(), history, restore)
            .following(sources.subscribe(), uploader.archived(), metrics.clone());
        Ok(Some(window))
    }

    /// A window of `window` milliseconds over the log in `dir`, bounded by the daily snapshots in `snapshots` and
    /// the index `history` names, restoring from `restore`; it follows no writer until `following`.
    pub(crate) fn new(
        dir: PathBuf,
        window: Millis,
        snapshots: PathBuf,
        history: HistorySection,
        restore: RestoreSources,
    ) -> Self {
        Self {
            dir,
            window,
            snapshots,
            history,
            sources: watch::channel(None).1,
            archived: watch::channel(None).1,
            restore,
            metrics: Arc::new(Metrics::new()),
            stamps: Stamps::default(),
            checked: Checked::default(),
            raised: Raised::default(),
            failing: None,
        }
    }

    /// This window, following the writers `sources` hands it, below the count of ranges `archived` gives once it is
    /// known.
    pub(crate) fn following(
        self,
        sources: watch::Receiver<Option<watch::Receiver<Manifest>>>,
        archived: watch::Receiver<Option<u64>>,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self { sources, archived, metrics, ..self }
    }

    /// Passes over each writer's segments in turn, every `PASS_EVERY`, until its sources close.
    /// Error cases:
    /// - every sender of its sources is gone
    pub(crate) async fn run(mut self) -> anyhow::Result<()> {
        let mut sources = std::mem::replace(&mut self.sources, watch::channel(None).1);
        loop {
            let source = sources.borrow_and_update().clone();
            if let Some(source) = source {
                // A writer that stops, as at a handover, leaves its manifest closed until the next one arrives.
                tokio::select! {
                    () = self.follow(source) => {}
                    changed = sources.changed() => {
                        changed.context("the local window's sources closed")?;
                        continue;
                    }
                }
            }
            sources.changed().await.context("the local window's sources closed")?;
        }
    }

    /// Passes over `manifest`'s writer's segments every `PASS_EVERY`, until the writer is gone.
    async fn follow(&mut self, manifest: watch::Receiver<Manifest>) {
        let mut manifest = manifest;
        if manifest.wait_for(|view| view.authority.is_some()).await.is_err() {
            return;
        }
        let dir = self.dir.clone();
        match spawn_blocking(move || restore::remove_staged(&dir)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("the local window could not remove what a crash left of a restore: {e:#}"),
            Err(e) => warn!("the local window's removal of what a crash left of a restore stopped: {e}"),
        }
        loop {
            let view = manifest.borrow_and_update().clone();
            match self.pass(&view).await {
                Ok(pass) => {
                    debug!(
                        removed = pass.removed.len(),
                        restored = pass.restored.len(),
                        held = ?pass.held,
                        "the local window passed over the log's segments"
                    );
                    self.failing = None;
                    if self.raised.failing.replace(false) != Some(false) {
                        info!(alert_resolved = "log_window_failing", "the local window's passes succeed");
                    }
                }
                Err(e) => self.failed(&e),
            }
            self.metrics.archive.window.passes.inc();
            // Not woken by the writer's flushes, which come with every batch.
            sleep(PASS_EVERY).await;
            if manifest.has_changed().is_err() {
                return;
            }
        }
    }

    /// One pass over the segments of `view`'s writer: puts back each it must keep and lacks, then removes, oldest
    /// first, each that may leave, and measures what stays.
    /// Error cases:
    /// - `view` is not a writer's, or the log directory, a listing or a kept segment's first command cannot be read
    /// - the directory does not hold the newest segment `view` names: one set aside, or rebuilt in its place, is not
    ///   the writer's, and the window touches nothing of it, nor of one that lost that segment
    pub(crate) async fn pass(&mut self, view: &Manifest) -> anyhow::Result<Pass> {
        view.ensure_verified()?;
        if !self.describes(view).await? {
            bail!(
                "the log's directory does not hold the newest segment its writer names, as long as it names it: a copy \
                 set aside or rebuilt in its place, or a directory that lost it"
            );
        }
        // Until the uploader's resume has found what the bucket holds, every segment would look unarchived.
        let Some(archived) = *self.archived.borrow() else {
            debug!("the window waits for the uploader to find how many ranges the bucket holds");
            return Ok(Pass::default());
        };
        let archived = archived.saturating_mul(RANGE_SEQS);
        let dir = self.dir.clone();
        let mut local: BTreeSet<u64> =
            spawn_blocking(move || list_segments(&dir)).await??.into_iter().map(|(seq, _)| seq).collect();
        let mut floors = None;
        let kept = self.kept_from(view, archived, &mut floors).await?;
        let restored = self.restore_lacking(view, &mut local, (archived, kept)).await?;
        let mut pass = Pass { restored, ..Pass::default() };
        let (removed, held) = self.remove_aged(view, &mut local, archived, &mut floors).await?;
        (pass.removed, pass.held) = (removed, held);
        self.measure(view, &local)?;
        Ok(pass)
    }

    /// Removes, oldest first and at most `MAX_REMOVALS`, each segment on the host that may leave, and returns those
    /// it removed and what holds the first it kept, raising or resolving `log_window_held` by how long that one is
    /// past the window. The newest completed segment always stays: nothing after it stamps the window for it.
    /// Error cases:
    /// - a listing, or a kept segment's first command, cannot be read
    async fn remove_aged(
        &mut self,
        view: &Manifest,
        local: &mut BTreeSet<u64>,
        archived: u64,
        floors: &mut Option<Floors>,
    ) -> anyhow::Result<(Vec<String>, Option<Held>)> {
        let mut removed = Vec::new();
        let (Some(&first), Some(newest)) = (local.first(), view.completed_len().checked_sub(1)) else {
            return Ok((removed, None));
        };
        let Some((mut ordinal, _)) = view.containing(first)? else { return Ok((removed, None)) };
        let newest = view.completed_at(newest)?;
        let now = self.stamp(&newest).await?;
        while ordinal + 1 < view.completed_len() {
            let (segment, next) = (view.completed_at(ordinal)?, view.completed_at(ordinal + 1)?);
            if !local.contains(&segment.first_seq) || !local.contains(&next.first_seq) {
                break;
            }
            let past = now.saturating_sub(self.stamp(&next).await?.saturating_add(self.window));
            let held = match self.holding(view, &segment, (archived, past), floors, now).await? {
                Some(held) => Some(held),
                None if removed.len() >= MAX_REMOVALS => return Ok((removed, None)),
                None => self.remove(view, &segment).await?,
            };
            if let Some(held) = held {
                self.report_held(&segment, held, past);
                return Ok((removed, Some(held)));
            }
            local.remove(&segment.first_seq);
            self.stamps.forget_before(next.first_seq);
            removed.push(segment.file);
            ordinal += 1;
        }
        self.report_held(&newest, Held::Window, 0);
        Ok((removed, Some(Held::Window)))
    }

    /// What holds `segment` on the host, `past` sequenced milliseconds past the window by the segment after it, the
    /// bucket holding the ranges before seq `archived`, short of a reader that takes a hold after this: the archive,
    /// the window, the floors, read once a pass and only when a segment reaches them, a reader, then the bucket's
    /// read-back.
    /// Error cases:
    /// - the floors cannot be read off the runtime
    async fn holding(
        &mut self,
        view: &Manifest,
        segment: &SegmentInfo,
        (archived, past): (u64, Millis),
        floors: &mut Option<Floors>,
        now: Millis,
    ) -> anyhow::Result<Option<Held>> {
        if segment.last_seq >= archived {
            return Ok(Some(Held::Archive));
        }
        if past < 0 {
            return Ok(Some(Held::Window));
        }
        if let Some(held) = self.floors(archived, now, floors).await?.holding(segment.last_seq) {
            return Ok(Some(held));
        }
        if view.holds.oldest().is_some_and(|oldest| oldest <= segment.last_seq) {
            return Ok(Some(Held::Reader));
        }
        match self.restore.read_back(segment).await {
            Ok(_) => Ok(None),
            Err(e) => {
                warn!(segment = %segment.file, "the bucket did not give back this segment, which stays: {e:#}");
                Ok(Some(Held::ReadBack))
            }
        }
    }

    /// The floors, as `floors` holds them once read this pass, or read now off the runtime, the bucket holding the
    /// ranges before seq `archived` and the log's newest completed segment starting at `now`.
    /// Error cases:
    /// - the floors cannot be read off the runtime
    pub(super) async fn floors(
        &mut self,
        archived: u64,
        now: Millis,
        floors: &mut Option<Floors>,
    ) -> anyhow::Result<Floors> {
        if let Some(read) = *floors {
            return Ok(read);
        }
        let (snapshots, history) = (self.snapshots.clone(), self.history_section());
        let mut checked = std::mem::take(&mut self.checked);
        let (checked, read) = spawn_blocking(move || {
            let read = Floors::read(&snapshots, &history, archived, now, &mut checked);
            (checked, read)
        })
        .await?;
        self.checked = checked;
        *floors = Some(read);
        Ok(read)
    }

    /// Removes `segment` unless a reader now holds it, under the holds' lock, and syncs the directory; what holds it
    /// otherwise.
    /// Error cases:
    /// - the file cannot be removed, or the directory synced
    async fn remove(&self, view: &Manifest, segment: &SegmentInfo) -> anyhow::Result<Option<Held>> {
        let newest = newest_named(view)?;
        let (holds, dir, file, last) = (view.holds.clone(), self.dir.clone(), segment.file.clone(), segment.last_seq);
        let removed = spawn_blocking(move || -> anyhow::Result<bool> {
            // The directory is checked under the lock the unlink holds, so the unlink follows the check at once.
            let removed = holds.unless_held(last, || {
                ensure!(
                    holds_newest(&dir, newest.as_ref()),
                    "the log's directory is no longer the one its writer lists"
                );
                Ok(std::fs::remove_file(dir.join(&file))?)
            });
            let Some(removed) = removed else { return Ok(false) };
            removed?;
            std::fs::File::open(&dir)?.sync_all()?;
            Ok(true)
        });
        match removed.await?.with_context(|| format!("removing {}", segment.file))? {
            true => {
                self.metrics.archive.window.removed.inc();
                info!(segment = %segment.file, last_seq = segment.last_seq, "the segment left the host for the archive");
                Ok(None)
            }
            false => Ok(Some(Held::Reader)),
        }
    }

    /// Whether the log's directory holds the newest segment `view` names, the one its writer appends to or else the
    /// newest it lists, at least as long as `view` knows it: a copy set aside, or one rebuilt in its place, is not the
    /// directory `view` lists, and the window touches nothing of it.
    /// Error cases:
    /// - a listing cannot be read
    async fn describes(&self, view: &Manifest) -> anyhow::Result<bool> {
        let (dir, newest) = (self.dir.clone(), newest_named(view)?);
        Ok(spawn_blocking(move || holds_newest(&dir, newest.as_ref())).await?)
    }

    /// The stamp of `listed`'s first command, read off the runtime once.
    /// Error cases:
    /// - see `Stamps::of`
    async fn stamp(&mut self, listed: &SegmentInfo) -> anyhow::Result<Millis> {
        let (dir, read, mut stamps) = (self.dir.clone(), listed.clone(), std::mem::take(&mut self.stamps));
        let (stamps, ts) = spawn_blocking(move || {
            let ts = stamps.of(&dir, &read);
            (stamps, ts)
        })
        .await?;
        self.stamps = stamps;
        ts
    }

    /// The history section the floors read.
    fn history_section(&self) -> HistorySection {
        HistorySection { dir: self.history.dir.clone(), retention: self.history.retention, marks: None }
    }
}

/// The newest segment `view` names: the one its writer appends to, or else the newest it lists.
/// Error cases:
/// - a listing cannot be read
fn newest_named(view: &Manifest) -> anyhow::Result<Option<SegmentInfo>> {
    match &view.active {
        Some(active) => Ok(Some(active.info.clone())),
        None => view.completed_len().checked_sub(1).map(|newest| view.completed_at(newest)).transpose(),
    }
}

/// Whether `dir` holds `newest`, a segment's listing, at least as long as it says; with no segment named, it does.
fn holds_newest(dir: &std::path::Path, newest: Option<&SegmentInfo>) -> bool {
    newest.is_none_or(|newest| std::fs::metadata(dir.join(&newest.file)).is_ok_and(|file| file.len() >= newest.bytes))
}

#[cfg(test)]
mod scenario;
#[cfg(test)]
mod tests;
