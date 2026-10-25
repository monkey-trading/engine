//! Writer publication: canonical completed descriptors and actual flushed-byte commitments.

use std::fs::{self, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, ensure};
use tracing::{info, warn};

use crate::log::catalog::{CheckpointLocator, LazyCatalogView};
use crate::log::segments::{Segment, command_seq, listing_line, segment_name};
use crate::log::{ActivePrefix, CatalogBoundary, LogEntry, LogWriter, Offset, SegmentInfo, offset_of};

/// Catalog garbage collection runs once per checkpoint on the sequencer thread; each turn consumes
/// whole receipts, so a few turns drain what one checkpoint's folds retire.
const MAX_CATALOG_GC_TURNS: usize = 8;

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
    /// - buffered write, file sync or directory sync fails
    pub(crate) fn sync_unpublished(&mut self) -> anyhow::Result<()> {
        if let Some((_, output)) = self.open.as_mut().filter(|_| self.unsynced) {
            output.flush().context("flushing unpublished checkpoint")?;
            output.get_ref().sync_data().context("syncing unpublished checkpoint")?;
            self.unsynced = false;
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

    /// Buffers one entry as a JSON line, and returns its offset. A command first completes the
    /// current segment if it is full.
    /// Error cases:
    /// - completing the current segment or starting a new one fails
    /// - a withdrawal or checkpoint comes before any command in this segment
    /// - the write to the buffer or file fails
    pub fn append(&mut self, entry: &LogEntry) -> anyhow::Result<Offset> {
        let offset = offset_of(self.last, entry)?;
        if let Some(seq) = command_seq(entry)
            && self.open.as_ref().is_none_or(|(segment, _)| segment.commands >= self.segment_commands)
        {
            self.start_segment(seq)?;
        }
        let (segment, out) = self.open.as_mut().context("log entry before the first command of its segment")?;
        let mut line = serde_json::to_vec(entry).context("encoding log entry")?;
        line.push(b'\n');
        // Set first: the buffer may reach the file before `flush`, and those bytes need syncing too.
        self.unsynced = true;
        out.write_all(&line).context("writing log entry")?;
        if let LogEntry::Checkpoint(checkpoint) = entry {
            segment.checkpoints.push(CheckpointLocator {
                segment_first_seq: segment.first_seq,
                offset,
                start: segment.bytes,
                end: segment.bytes + line.len() as u64,
                checkpoint: checkpoint.clone(),
            });
        }
        segment.record(&line, Some(entry));
        self.last = Some(offset);
        Ok(offset)
    }

    /// Writes buffered entries to the open segment and syncs it to the disk, unless the segment is
    /// already synced, then publishes the last of them as flushed. Called after each command batch:
    /// a command must be durable before its effects are acknowledged to clients or read from disk
    /// by the stream, and in a confidential VM the guest page cache is lost whenever the VM stops.
    /// Error cases:
    /// - the write to the file, or the sync, fails
    pub fn flush(&mut self) -> anyhow::Result<()> {
        if let Some((_, out)) = self.open.as_mut().filter(|_| self.unsynced) {
            out.flush().context("flushing log")?;
            out.get_ref().sync_data().context("syncing log")?;
            self.unsynced = false;
        }
        let last = self.last;
        let active = self
            .open
            .as_ref()
            .filter(|(segment, _)| segment.bytes > 0)
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

    /// Starts the segment for `first_seq`, first syncing and listing the current one, if any. The
    /// new file takes its segment name only once the old one is listed, which `LogReader` relies on.
    /// Creating it is the only step that needs a new file descriptor, so it runs first: if it fails
    /// while a segment is open, that segment keeps taking appends and the next command retries.
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
        let published = self.complete_and_link(&staged, &path);
        // The staging name only held the new file until its link to `path`.
        let removed = fs::remove_file(&staged).with_context(|| format!("removing {}", staged.display()));
        published?;
        removed?;
        self.open = Some((Segment::empty(first_seq), BufWriter::new(file)));
        // A flush syncs only the segment's data, so its name is made durable here.
        self.dir_handle.sync_all().with_context(|| format!("syncing {}", self.dir.display()))?;
        if std::mem::take(&mut self.rotation_failing) {
            info!(alert_resolved = "log_rotation_failed", "log segments rotate again");
        }
        Ok(())
    }

    /// Lists the open segment, if any, then links the staged file as the next segment at `path`.
    /// Error cases:
    /// - a segment by that name already exists, the open segment cannot be synced or listed, or the
    ///   link fails
    fn complete_and_link(&mut self, staged: &Path, path: &Path) -> anyhow::Result<()> {
        ensure!(!path.exists(), "{} already exists", path.display());
        if let Some((segment, out)) = self.open.take() {
            let done = out.into_inner().map_err(|e| e.into_error()).context("flushing log segment")?;
            done.sync_all().context("syncing log segment")?;
            let listed = segment.info();
            self.list(&listed)?;
            info!(
                file = %listed.file,
                first_seq = listed.first_seq,
                last_seq = listed.last_seq,
                bytes = listed.bytes,
                sha256 = %listed.sha256,
                "log segment completed"
            );
            let catalog = self.manifest.borrow().catalog.clone();
            let next = catalog.map(|c| c.append(listed.clone(), segment.checkpoints.into())).transpose()?;
            self.manifest.send_modify(|m| {
                match next {
                    Some(next) => m.catalog = Some(Arc::new(next)),
                    None => Arc::make_mut(&mut m.segments).push(listed),
                }
                m.active = None;
            });
        }
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
