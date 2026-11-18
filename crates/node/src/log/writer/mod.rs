//! Writer publication: canonical completed descriptors and actual flushed-byte commitments.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Context;
use tokio::sync::watch;
use tracing::{info, warn};

mod append;
pub(super) mod output;
pub(super) mod seal;

use crate::log::catalog::{CheckpointLocator, LazyCatalogView};
use crate::log::segments::{Segment, segment_name};
use crate::log::writer::output::Output;
use crate::log::writer::seal::SealWork;
use crate::log::{ActivePrefix, CatalogBoundary, LogWriter, Manifest, Offset};

/// Catalog garbage collection runs once per checkpoint on the sequencer thread; each turn consumes
/// whole receipts, so a few turns drain what one checkpoint's folds retire.
const MAX_CATALOG_GC_TURNS: usize = 8;

/// A batch's entries, still to be written and to reach storage, with the segment seals rotation
/// deferred to this job. `write` and `sync` can run on any thread, the jobs in the order they were
/// taken; `LogWriter::finish_sync` publishes the batch and the seals once it has.
pub(crate) struct SyncJob {
    file: Option<Arc<fs::File>>,
    /// The entries the batch appended, taken by the first `write`.
    bytes: Mutex<Vec<u8>>,
    /// Whether the job carried entries or a seal for the sync thread to write.
    carried: bool,
    dir: Option<fs::File>,
    epoch: u64,
    /// Bytes of the entries appended since the job before this one was taken.
    appended_bytes: u64,
    segment: Option<u64>,
    last: Option<Offset>,
    active: Option<ActivePrefix>,
    seals: Vec<SealWork>,
}

impl SyncJob {
    /// Bytes of the entries the batch appended.
    pub(crate) fn appended_bytes(&self) -> u64 {
        self.appended_bytes
    }

    /// Whether the job has nothing to sync: no bytes were appended since the last sync and no
    /// segment seal waits on it.
    pub(crate) fn is_empty(&self) -> bool {
        self.file.is_none() && self.dir.is_none() && self.seals.is_empty()
    }

    /// Whether syncing `next`, queued after this job, makes syncing this one unnecessary: `next`
    /// syncs the data of the same segment, which holds this job's entries, and this one has no
    /// directory or seal of its own.
    pub(crate) fn covered_by(&self, next: &SyncJob) -> bool {
        self.seals.is_empty() && self.dir.is_none() && next.file.is_some() && self.segment == next.segment
    }

    /// Writes the batch's entries to its segment, once, with every earlier job's already written.
    /// A job covered by the next one is written and left to that job's sync.
    /// Error cases:
    /// - the write fails
    pub(crate) fn write(&self) -> anyhow::Result<()> {
        let bytes = std::mem::take(&mut *self.bytes.lock().unwrap_or_else(PoisonError::into_inner));
        if let Some(file) = self.file.as_deref().filter(|_| !bytes.is_empty()) {
            (&*file).write_all(&bytes).context("writing log")?;
        }
        Ok(())
    }

    /// Runs each deferred segment seal, which writes and syncs the segment rotation completed, then
    /// writes this batch's entries and syncs the segment's data and, if asked for, the directory.
    /// Error cases:
    /// - a seal, the write, or either sync fails
    pub(crate) fn sync(&self) -> anyhow::Result<()> {
        for seal in &self.seals {
            seal.run()?;
        }
        self.write()?;
        if let Some(file) = &self.file {
            file.sync_data().context("syncing log")?;
        }
        if let Some(dir) = &self.dir {
            dir.sync_all().context("syncing log directory")?;
        }
        Ok(())
    }
}

impl LogWriter {
    /// Collects retired catalog versions under the same catalog owner, in at most
    /// `MAX_CATALOG_GC_TURNS` bounded turns, stopping once a turn makes no progress. A live catalog
    /// left full by a failed fold is folded here first, so the retry runs once per checkpoint.
    /// Error cases:
    /// - the retried fold, retained pins, official update proofs, publication or live-view rebasing fails
    pub(crate) fn maintain_catalog(&mut self) -> anyhow::Result<()> {
        if self.catalog_owner.is_none() {
            return Ok(());
        }
        if self.catalog_failing && self.manifest.borrow().catalog.as_ref().is_some_and(|c| c.live_full()) {
            self.fold_catalog(None)?;
            self.catalog_failing = false;
            info!(alert_resolved = "log_catalog_unavailable", "the log catalog takes completed segments again");
        }
        self.catalog_maintenance(|store, start| {
            let mut root = start.clone();
            for _ in 0..MAX_CATALOG_GC_TURNS {
                let (next, report) = match store.gc(&root, crate::authenticated::COMPLETE_RECEIPT_BUDGET) {
                    Ok(turn) => turn,
                    // Earlier turns committed; the owner must adopt their root.
                    Err(error) if root != *start => {
                        warn!(alert = "log_catalog_unavailable", "catalog collection stopped early: {error:#}");
                        break;
                    }
                    Err(error) => return Err(error),
                };
                let progressed = next != root;
                root = next;
                if report.idle || !progressed {
                    break;
                }
            }
            Ok(root)
        })
    }

    /// Syncs actual bytes while leaving backfill's published physical frontier unchanged.
    /// Error cases:
    /// - a sync job holds entries not yet written, or the write, file sync or directory sync fails
    pub(crate) fn sync_unpublished(&mut self) -> anyhow::Result<()> {
        self.write_pending()?;
        if let Some((_, output)) = self.open.as_mut().filter(|_| self.unsynced) {
            output.file().sync_data().context("syncing unpublished checkpoint")?;
            self.unsynced = false;
            #[cfg(test)]
            {
                self.syncs += 1;
            }
        }
        self.dir_handle.sync_all().context("syncing unpublished checkpoint directory")
    }

    /// Pins exact C before any feed can observe it, without waiting for the history subscriber.
    /// Error cases:
    /// - current physical framing, exact catalog commit or durable catalog-head publication fails
    pub(crate) fn seal_catalog_boundary_unpublished(
        &mut self,
    ) -> anyhow::Result<(CatalogBoundary, Arc<LazyCatalogView>)> {
        let pending = self.capture_catalog_boundary_unpublished()?;
        let owner = self.catalog_owner.as_mut().context("catalog owner missing")?;
        let (boundary, catalog) = pending.commit(owner)?;
        self.manifest.send_modify(|manifest| manifest.catalog = Some(catalog.clone()));
        Ok((boundary, catalog))
    }

    /// Returns the exact flushed checkpoint boundary and a root lease to hold through publication.
    /// The parent calls this only after authoritative log sync and startup mutation authorization.
    /// Error cases:
    /// - the frontier is not a checkpoint, the catalog is absent, or its durable fold fails
    pub fn seal_catalog_boundary(&mut self) -> anyhow::Result<(CatalogBoundary, Arc<LazyCatalogView>)> {
        let pending = self.capture_catalog_boundary()?;
        let owner = self.catalog_owner.as_mut().context("catalog owner missing")?;
        let (boundary, catalog) = pending.commit(owner)?;
        self.manifest.send_modify(|m| m.catalog = Some(catalog.clone()));
        Ok((boundary, catalog))
    }

    /// Folds completed metadata independently of paired snapshot/history acknowledgments.
    /// Error cases:
    /// - exact staging, commit, or durable signed tip publication fails
    pub fn flush_catalog(&mut self) -> anyhow::Result<()> {
        self.fold_catalog(None).map(|_| ())
    }

    /// Applies official Store maintenance and refreshes the compatible watch to its signed root.
    /// Error cases:
    /// - maintenance, metadata verification, head publication, or live-prefix rebase fails
    pub fn catalog_maintenance(
        &mut self,
        operation: impl FnOnce(
            &crate::authenticated::Store,
            &crate::authenticated::Root,
        ) -> anyhow::Result<crate::authenticated::Root>,
    ) -> anyhow::Result<()> {
        let view = self.manifest.borrow().catalog.clone().context("catalog missing")?;
        let owner = self.catalog_owner.as_mut().context("catalog owner missing")?;
        owner.maintenance(operation)?;
        let next = Arc::new(owner.align(&view)?);
        self.manifest.send_modify(|m| m.catalog = Some(next));
        Ok(())
    }

    /// Keeps every Store version advance and its signed head under one serialized owner.
    /// Error cases:
    /// - exact fold or durable head publication fails
    fn fold_catalog(&mut self, locator: Option<CheckpointLocator>) -> anyhow::Result<Arc<LazyCatalogView>> {
        let view = self.manifest.borrow().catalog.clone().context("catalog missing")?;
        let owner = self.catalog_owner.as_mut().context("catalog owner missing")?;
        let next = Arc::new(view.fold(owner, locator)?);
        self.manifest.send_modify(|m| m.catalog = Some(next.clone()));
        Ok(next)
    }

    /// Writes buffered entries to the open segment and syncs it to the disk, unless the segment is
    /// already synced, then publishes the last of them as flushed. Called after each command batch:
    /// a command must be durable before its effects are acknowledged to clients or read from disk
    /// by the stream, and in a confidential VM the guest page cache is lost whenever the VM stops.
    /// Error cases:
    /// - a sync job holds entries not yet written, or the write to the file, or the sync, fails
    pub fn flush(&mut self) -> anyhow::Result<()> {
        self.write_pending()?;
        if let Some((_, out)) = self.open.as_mut().filter(|_| self.unsynced) {
            out.file().sync_data().context("syncing log")?;
            self.unsynced = false;
            #[cfg(test)]
            {
                self.syncs += 1;
            }
        }
        let last = self.last;
        // A seal not yet published means the view still holds the previous segment as the active
        // one; publishing the new segment's prefix here would contradict it.
        let active = self
            .open
            .as_ref()
            .filter(|(segment, _)| segment.bytes > 0)
            .filter(|_| self.pending_seals.is_empty() && self.taken_seals == 0)
            .and_then(|(segment, _)| last.map(|last| ActivePrefix { info: segment.info(), last }));
        self.manifest.send_modify(|published| {
            published.flushed = last;
            published.active = active;
        });
        Ok(())
    }

    /// Syncs the open segment's data, then forces the directory entry naming it to storage, so its
    /// entries survive a host kill or a power loss as well as a process crash, and only then
    /// publishes them as flushed: a signature the stream can read is already durable.
    /// Error cases:
    /// - the write or either sync fails
    pub fn sync(&mut self) -> anyhow::Result<()> {
        self.sync_unpublished()?;
        self.flush()
    }

    /// Hands every entry appended since the last job to a new one, which writes them and makes them
    /// durable, for a thread other than the writer's to run in the order the jobs were taken:
    /// nothing is published until `finish_sync`, and the writer still counts the entries unsynced
    /// until then. An inline `sync` or `flush` waits for every such job first (`write_pending`).
    /// With `directory`, the job also syncs the directory entries.
    /// Error cases:
    /// - the directory handle cannot be duplicated
    pub(crate) fn begin_sync(&mut self, directory: bool) -> anyhow::Result<SyncJob> {
        let (mut file, mut bytes) = (None, Vec::new());
        // Entries a job already took are that job's to sync, so a batch that appended nothing
        // since carries none. The entries ride the job, so this thread makes no call on the file.
        let new = self.begun != Some(self.appended);
        if let Some((_, out)) = self.open.as_mut().filter(|_| self.unsynced && new) {
            bytes = out.take();
            file = Some(out.file().clone());
        }
        let dir = directory
            .then(|| self.dir_handle.try_clone().context("duplicating the log directory handle"))
            .transpose()?;
        self.begun = Some(self.appended);
        let last = self.last;
        let segment = self.open.as_ref().map(|(segment, _)| segment);
        let active = segment
            .filter(|segment| segment.bytes > 0)
            .and_then(|segment| last.map(|last| ActivePrefix { info: segment.info(), last }));
        let seals = std::mem::take(&mut self.pending_seals);
        self.taken_seals += seals.len() as u64;
        let appended_bytes = self.appended_bytes - std::mem::replace(&mut self.begun_bytes, self.appended_bytes);
        let carried = !bytes.is_empty() || !seals.is_empty();
        self.handed += u64::from(carried);
        let (bytes, segment) = (Mutex::new(bytes), segment.map(|s| s.first_seq));
        Ok(SyncJob { file, bytes, carried, dir, epoch: self.appended, appended_bytes, segment, last, active, seals })
    }

    /// Takes `job`, which has run, as done: its entries are durable, so the writer no longer counts
    /// them unsynced and the manifest publishes them as flushed. The job's deferred seals publish
    /// first, each listing its segment as completed and clearing the active prefix, so the manifest
    /// never carries a new segment's active prefix ahead of its predecessor's completion. A job
    /// older than what an inline flush already published changes nothing, and one whose segment has
    /// since been completed leaves the manifest's active prefix as the completion set it.
    pub(crate) fn finish_sync(&mut self, job: &SyncJob) {
        if job.epoch == self.appended {
            self.unsynced = false;
        }
        #[cfg(test)]
        if job.file.is_some() {
            self.syncs += 1;
        }
        for seal in &job.seals {
            self.list_deferred(seal);
        }
        self.taken_seals -= job.seals.len() as u64;
        self.handed -= u64::from(job.carried);
        let current = self.open.as_ref().map(|(segment, _)| segment.first_seq);
        self.manifest.send_if_modified(|published| {
            if published.flushed > job.last {
                return false;
            }
            published.flushed = job.last;
            if current == job.segment {
                published.active = job.active.clone();
            }
            true
        });
    }

    /// The offset of the last entry appended, or `None` before the first: a follower resumes the
    /// stream right after it.
    pub fn last_offset(&self) -> Option<Offset> {
        self.last
    }

    /// Publishes the manifest on `manifest` from now on, starting with the one published so far: a
    /// follower's writer, which published to no reader, becomes the sequencer's, or a member link's.
    pub(crate) fn publish_on(&mut self, manifest: watch::Sender<Manifest>) {
        manifest.send_replace(self.published());
        self.manifest = manifest;
    }

    /// The manifest this writer has published.
    pub(crate) fn published(&self) -> Manifest {
        self.manifest.borrow().clone()
    }

    /// How many times the open segment's bytes reached storage; tests bound sync budgets with it.
    #[cfg(test)]
    pub(crate) fn sync_count(&self) -> usize {
        self.syncs
    }

    /// Starts the segment for `first_seq`, first syncing and listing the current one, if any. The
    /// new file takes its segment name only once the old one is listed, which `LogReader` relies
    /// on. With deferred seals (`pipeline_log_syncs`) the listing, the link, and their fsyncs ride
    /// the next `SyncJob` instead of running here; the on-disk order is unchanged. Creating it is
    /// the only step that needs a new file descriptor, so it runs first: if it fails while a
    /// segment is open, that segment keeps taking appends and the next command retries.
    /// Error cases:
    /// - the new segment cannot be created and no segment is open
    /// - a segment by the new name already exists
    /// - the current segment cannot be flushed or synced, or its listing cannot be stored
    /// - the new segment cannot be exclusively linked into place
    fn start_segment(&mut self, first_seq: u64) -> anyhow::Result<()> {
        // Only the catalog: holding the published segment list here would make listing below copy it.
        let catalog = self.manifest.borrow().catalog.clone();
        if let Some(catalog) = &catalog {
            let full = catalog.live_full();
            if full && self.catalog_failing && self.open.is_some() {
                // A full live catalog retries in the next checkpoint's maintenance, not per command.
                return Ok(());
            }
            let next = if full {
                self.fold_catalog(None)
            } else {
                let owner = self.catalog_owner.as_mut().context("catalog owner missing")?;
                owner.align(catalog).map(Arc::new)
            };
            match next {
                Ok(next) => {
                    self.manifest.send_modify(|m| m.catalog = Some(next));
                    if std::mem::take(&mut self.catalog_failing) {
                        info!(
                            alert_resolved = "log_catalog_unavailable",
                            "the log catalog takes completed segments again"
                        );
                    }
                }
                // Below capacity, the live view holds the completed segment until a later fold.
                Err(e) if !full || self.open.is_some() => {
                    if !self.catalog_failing {
                        warn!(
                            alert = "log_catalog_unavailable",
                            "catalog publication failed; segments wait in memory, then the active one grows: {e:#}"
                        );
                        self.catalog_failing = true;
                    }
                    if full {
                        return Ok(());
                    }
                }
                Err(e) => return Err(e).context("publishing log catalog"),
            }
        }
        let path = self.dir.join(segment_name(first_seq));
        let staged = staging_path(&self.dir, first_seq);
        // A crash during an earlier attempt at this rotation leaves the same name.
        let file = match remove_if_present(&staged)
            .and_then(|()| OpenOptions::new().write(true).create_new(true).open(&staged))
        {
            Ok(file) => file,
            Err(e) if self.open.is_some() => {
                if !self.rotation_failing {
                    warn!(alert = "log_rotation_failed", "cannot start a log segment, so the current one grows: {e}");
                    self.rotation_failing = true;
                }
                return Ok(());
            }
            Err(e) => return Err(e).with_context(|| format!("creating {}", staged.display())),
        };
        if self.seals_deferred {
            self.defer_segment_seal(&staged, &path)?;
            self.open = Some((Segment::empty(first_seq), Output::new(file)));
        } else {
            let published = self.complete_and_link(&staged, &path);
            // The staging name only held the new file until its link to `path`.
            let removed = fs::remove_file(&staged).with_context(|| format!("removing {}", staged.display()));
            published?;
            removed?;
            self.open = Some((Segment::empty(first_seq), Output::new(file)));
            // A flush syncs only the segment's data, so its name is made durable here.
            self.dir_handle.sync_all().with_context(|| format!("syncing {}", self.dir.display()))?;
        }
        if std::mem::take(&mut self.rotation_failing) {
            info!(alert_resolved = "log_rotation_failed", "log segments rotate again");
        }
        Ok(())
    }
}

/// The one staging name for the segment starting at `first_seq`.
pub(super) fn staging_path(dir: &Path, first_seq: u64) -> std::path::PathBuf {
    dir.join(format!("{}.tmp", segment_name(first_seq)))
}

/// Removes `path` if it exists.
/// Error cases:
/// - the file exists but cannot be removed
pub(super) fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}
