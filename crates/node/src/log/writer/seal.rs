//! Sealing a completed segment: its fsync, its durable listing in the manifest, and the staged next
//! segment's exclusive link. `seal_open_segment` runs this inline; with a log-sync pipeline
//! (`LogWriter::defer_seals`) rotation defers it as a `SealWork` riding the next `SyncJob`, so a
//! segment boundary costs the sequencer thread no fsync. The on-disk order is the same either way:
//! the old segment is listed before the new one takes its name, and the directory entry reaches
//! storage last, so a crash mid-seal leaves what a crash mid-rotation leaves.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, ensure};
use tracing::{info, warn};

use crate::log::catalog::CheckpointLocator;
use crate::log::segments::listing_line;
use crate::log::{LogWriter, SegmentInfo};

/// One deferred segment seal: what the sync thread runs to make the seal durable, and what
/// `finish_sync` publishes once it has.
pub(crate) struct SealWork {
    file: File,
    dir: File,
    manifest: File,
    base_len: u64,
    line: Vec<u8>,
    staged: PathBuf,
    path: PathBuf,
    listed: SegmentInfo,
    checkpoints: Vec<CheckpointLocator>,
}

impl SealWork {
    /// Seals the segment and links the staged next one, in the order and with the durability an
    /// inline seal gives: the segment's bytes, then the directory, then the manifest listing; the
    /// new segment's name only after its predecessor is listed, and the directory again at the end.
    /// Error cases:
    /// - a sync, the manifest length check, the listing write, the exclusive link, or the staging
    ///   removal fails
    pub(crate) fn run(&self) -> anyhow::Result<()> {
        self.file.sync_all().context("syncing log segment")?;
        self.dir.sync_all().context("syncing log directory")?;
        ensure!(self.manifest.metadata()?.len() == self.base_len, "manifest length changed before listing");
        let mut manifest = &self.manifest;
        manifest.write_all(&self.line).context("writing manifest")?;
        self.manifest.sync_all().context("syncing manifest")?;
        // hard_link fails if an untrusted host races an existing destination into place.
        ensure!(!self.path.exists(), "{} already exists", self.path.display());
        fs::hard_link(&self.staged, &self.path)
            .with_context(|| format!("exclusively publishing {}", self.path.display()))?;
        fs::remove_file(&self.staged).with_context(|| format!("removing {}", self.staged.display()))?;
        self.dir.sync_all().context("syncing log directory")?;
        Ok(())
    }
}

impl LogWriter {
    /// Moves rotation's seal to the log-sync thread; the sequencer calls it where it starts that
    /// thread (`pipeline_log_syncs`).
    pub(crate) fn defer_seals(&mut self) {
        self.seals_deferred = true;
    }

    /// Defers the seal the current rotation owes the open segment: its flush, listing, and the
    /// staged file's link become a `SealWork` riding the next `SyncJob`, and the open segment swaps
    /// immediately, so appends go on while the seal reaches storage. With no open segment — the
    /// first segment of a boot, or one recovery found listed — there is nothing to seal, and the
    /// link and its directory sync stay here, as they do without deferral.
    /// Error cases:
    /// - the open segment cannot be flushed, a handle cannot be duplicated, or the inline link of a
    ///   first segment fails
    pub(crate) fn defer_segment_seal(&mut self, staged: &Path, path: &Path) -> anyhow::Result<()> {
        if self.open.is_none() {
            let published = self.complete_and_link(staged, path);
            let removed = fs::remove_file(staged).with_context(|| format!("removing {}", staged.display()));
            published?;
            removed?;
            self.dir_handle.sync_all().with_context(|| format!("syncing {}", self.dir.display()))?;
            return Ok(());
        }
        let (segment, out) = self.open.as_mut().expect("checked above");
        out.flush().context("flushing log segment")?;
        let listed = segment.info();
        let line = listing_line(&listed)?;
        // An earlier rotation's seal may still ride a queued job: its listing lands ahead of this
        // one, so this one appends after every pending seal's line, not at the writer's length.
        let base_len = self
            .manifest_len
            .checked_add(self.pending_seals.iter().map(|seal| seal.line.len() as u64).sum::<u64>())
            .context("manifest length overflow")?;
        let seal = SealWork {
            file: out.get_ref().try_clone().context("duplicating the log file handle")?,
            dir: self.dir_handle.try_clone().context("duplicating the log directory handle")?,
            manifest: self.manifest_file.try_clone().context("duplicating the manifest handle")?,
            base_len,
            staged: staged.to_owned(),
            path: path.to_owned(),
            listed,
            checkpoints: std::mem::take(&mut segment.checkpoints),
            line,
        };
        let (_, out) = self.open.take().expect("the open segment is flushed");
        drop(out);
        self.pending_seals.push(seal);
        Ok(())
    }

    /// Publishes one deferred seal whose `SealWork` has run on the sync thread: the manifest length
    /// it wrote is now the writer's, and the segment lists as completed with no active prefix, as
    /// `seal_open_segment` does inline.
    pub(crate) fn list_deferred(&mut self, seal: &SealWork) {
        self.manifest_len = self.manifest_len.checked_add(seal.line.len() as u64).expect("manifest length overflow");
        info!(
            file = %seal.listed.file,
            first_seq = seal.listed.first_seq,
            last_seq = seal.listed.last_seq,
            bytes = seal.listed.bytes,
            sha256 = %seal.listed.sha256,
            "log segment completed"
        );
        let catalog = self.manifest.borrow().catalog.clone();
        match catalog.map(|c| c.append(seal.listed.clone(), Arc::from(seal.checkpoints.clone()))).transpose() {
            Ok(next) => {
                self.manifest.send_modify(|m| {
                    match next {
                        Some(next) => m.catalog = Some(Arc::new(next)),
                        None => Arc::make_mut(&mut m.segments).push(seal.listed.clone()),
                    }
                    m.active = None;
                });
            }
            Err(e) => {
                warn!(alert = "log_catalog_unavailable", "the sealed segment publishes without the catalog: {e:#}");
                // The archive serves the catalog when one is present: drop it so the plain manifest
                // lists the sealed segment until the catalog recovers.
                self.manifest.send_modify(|m| {
                    m.catalog = None;
                    Arc::make_mut(&mut m.segments).push(seal.listed.clone());
                    m.active = None;
                });
            }
        }
    }

    /// Lists the open segment, if any, as completed: flushes and syncs it, appends its listing to
    /// the manifest, and folds it into the catalog. A handover seals the log's tail so the archive
    /// and the successor it hands the keys to hold the segment the final checkpoint is in. A full
    /// live catalog is folded first, as rotation does; a catalog that still cannot take the
    /// segment degrades to the plain manifest, which remains authoritative.
    /// Error cases:
    /// - the segment cannot be flushed, synced, or listed, or the manifest update fails
    pub(crate) fn seal_open_segment(&mut self) -> anyhow::Result<Option<SegmentInfo>> {
        if self.catalog_owner.is_some()
            && self.manifest.borrow().catalog.as_ref().is_some_and(|catalog| catalog.live_full())
        {
            // A failed fold is not fatal: the append below degrades to the plain manifest, which
            // remains authoritative.
            if let Err(e) = self.fold_catalog(None) {
                warn!(alert = "log_catalog_unavailable", "folding the full catalog before the seal failed: {e:#}");
            }
        }
        let Some((segment, out)) = self.open.as_mut() else { return Ok(None) };
        out.flush().context("flushing log segment")?;
        out.get_ref().sync_all().context("syncing log segment")?;
        let listed = segment.info();
        // List before taking the open segment: a failure leaves the segment open, so sequencing
        // goes on with a log that still opens.
        self.list(&listed)?;
        let (segment, _) = self.open.take().expect("the open segment is listed");
        info!(
            file = %listed.file,
            first_seq = listed.first_seq,
            last_seq = listed.last_seq,
            bytes = listed.bytes,
            sha256 = %listed.sha256,
            "log segment completed"
        );
        let catalog = self.manifest.borrow().catalog.clone();
        match catalog.map(|c| c.append(listed.clone(), segment.checkpoints.into())).transpose() {
            Ok(next) => {
                self.manifest.send_modify(|m| {
                    match next {
                        Some(next) => m.catalog = Some(Arc::new(next)),
                        None => Arc::make_mut(&mut m.segments).push(listed.clone()),
                    }
                    m.active = None;
                });
            }
            Err(e) => {
                warn!(alert = "log_catalog_unavailable", "the sealed segment publishes without the catalog: {e:#}");
                // The archive serves the catalog when one is present: drop it so the plain manifest
                // lists the sealed segment until the catalog recovers.
                self.manifest.send_modify(|m| {
                    m.catalog = None;
                    Arc::make_mut(&mut m.segments).push(listed.clone());
                    m.active = None;
                });
            }
        }
        Ok(Some(listed))
    }

    /// Lists the open segment, if any, then links the staged file as the next segment at `path`.
    /// Error cases:
    /// - a segment by that name already exists, the open segment cannot be synced or listed, or the
    ///   link fails
    pub(crate) fn complete_and_link(&mut self, staged: &Path, path: &Path) -> anyhow::Result<()> {
        ensure!(!path.exists(), "{} already exists", path.display());
        self.seal_open_segment()?;
        // hard_link fails if an untrusted host races an existing destination into place.
        fs::hard_link(staged, path).with_context(|| format!("exclusively publishing {}", path.display()))
    }

    /// Appends one listing to the manifest, durably, after the listed segment's directory entry.
    /// Error cases:
    /// - the directory or the manifest cannot be synced, or the manifest cannot be written
    fn list(&mut self, listed: &SegmentInfo) -> anyhow::Result<()> {
        self.dir_handle.sync_all().with_context(|| format!("syncing {}", self.dir.display()))?;
        ensure!(self.manifest_file.metadata()?.len() == self.manifest_len, "manifest length changed before listing");
        let line = listing_line(listed)?;
        self.manifest_file.write_all(&line).context("writing manifest")?;
        self.manifest_file.sync_all().context("syncing manifest")?;
        self.manifest_len = self.manifest_len.checked_add(line.len() as u64).context("manifest length overflow")?;
        Ok(())
    }
}
