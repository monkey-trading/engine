//! The node's writer of daily snapshots. The sequencer hands it each one once its checkpoint is durable,
//! with the engine's encoding the stats worker gives, and a thread of its own publishes it: the file under
//! its name, then its line in the manifest, each synced before the next, so a crash leaves a listed
//! snapshot whole, or an unlisted file the next start removes. A failed write pages Warning
//! `daily_snapshot_failed` and loses only that day's snapshot, which merges two of an audit's ranges; the
//! log stays the authority. The node keeps every day's snapshot.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};

use anyhow::{Context, anyhow};
use engine_core::types::Millis;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::daily_snapshots::{Listed, MANIFEST, SCHEMA, Written, day, offset_named, read_manifest};
use crate::log::{Checkpoint, Offset};
use crate::snapshot::sealing::Encoding;

/// A daily snapshot for the thread to publish.
pub(crate) struct Job {
    pub offset: Offset,
    pub checkpoint: Checkpoint,
    /// The checkpoint's sequenced time.
    pub ts: Millis,
    pub engine: Encoding,
}

/// The sequencer's end of the writer.
pub(crate) struct Writer {
    /// Closed when the writer is dropped, which ends the thread once it has published what it holds.
    jobs: Option<SyncSender<Job>>,
    /// The sequenced time of the newest snapshot listed or handed over.
    last_ts: Option<Millis>,
    thread: Option<JoinHandle<()>>,
}

/// Feeds bytes to a file and to their SHA-256 hash, counting them.
struct Hashed<W> {
    inner: W,
    hash: Sha256,
    bytes: u64,
}

impl Drop for Writer {
    /// Closes the queue and joins the thread, which publishes the snapshot it holds first.
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            // A thread that panicked has paged nothing more to report here; the log stays the authority.
            let _ = thread.join();
        }
    }
}

impl<W: Write> Write for Hashed<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hash.update(&buf[..written]);
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Writer {
    /// Starts the writer for `dir`, putting it in order after the last start first: staging files go, so
    /// do snapshot files the manifest does not list, and listings past `last_seq`, the log's last command,
    /// which a log cut back no longer holds.
    /// Error cases:
    /// - `dir` cannot be created, listed or cleaned, its manifest cannot be read or rewritten, or the
    ///   thread cannot start
    pub(crate) fn start(dir: &Path, last_seq: Option<u64>) -> anyhow::Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut listed = read_manifest(dir)?;
        let held = listed.len();
        listed.retain(|listing| last_seq.is_some_and(|last| listing.seq <= last));
        if listed.len() < held {
            warn!(dropped = held - listed.len(), "daily snapshots past the log's last command are dropped");
            write_manifest(dir, &listed)?;
        }
        for entry in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
            let entry = entry.with_context(|| format!("listing {}", dir.display()))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let unlisted = offset_named(&name).is_some() && !listed.iter().any(|listing| listing.file == name);
            if name.ends_with(".tmp") || unlisted {
                fs::remove_file(entry.path()).with_context(|| format!("removing {}", entry.path().display()))?;
            }
        }
        sync_dir(dir)?;
        let last_ts = listed.last().map(|listing| listing.ts);
        let (jobs, taken) = sync_channel(1);
        let owned = dir.to_owned();
        let thread = thread::Builder::new()
            .name("daily-snapshots".into())
            .spawn(move || run(&owned, listed, taken))
            .context("starting the daily snapshot writer")?;
        Ok(Self { jobs: Some(jobs), last_ts, thread: Some(thread) })
    }

    /// Whether a snapshot is due at a checkpoint stamped `ts`: none is taken yet, or the newest is on an
    /// earlier UTC day.
    pub(crate) fn due(&self, ts: Millis) -> bool {
        self.last_ts.is_none_or(|last| day(ts) > day(last))
    }

    /// Hands `job` to the thread, which takes its day. A thread still writing the last one, or stopped,
    /// takes nothing, which pages Warning `daily_snapshot_failed`, and the day's next checkpoint tries again.
    pub(crate) fn take(&mut self, job: Job) {
        let (seq, ts) = (job.offset.seq, job.ts);
        let refused = match self.jobs.as_ref().map(|jobs| jobs.try_send(job)) {
            Some(Ok(())) => {
                self.last_ts = Some(ts);
                return;
            }
            Some(Err(TrySendError::Full(_))) => "the writer is still writing the last one",
            Some(Err(TrySendError::Disconnected(_))) | None => "the writer has stopped",
        };
        warn!(alert = "daily_snapshot_failed", seq, "no daily snapshot at this checkpoint: {refused}");
    }
}

/// The thread: publishes each snapshot it is handed in `dir`, whose manifest lists `listed`, until the
/// sequencer's end closes.
fn run(dir: &Path, mut listed: Vec<Listed>, jobs: Receiver<Job>) {
    let mut failing = false;
    for job in jobs {
        let seq = job.offset.seq;
        match publish(dir, &mut listed, job) {
            Ok(listing) => {
                info!(seq, file = %listing.file, bytes = listing.bytes, "daily snapshot published");
                if std::mem::take(&mut failing) {
                    info!(alert_resolved = "daily_snapshot_failed", "a daily snapshot is published");
                }
            }
            Err(e) => {
                failing = true;
                warn!(alert = "daily_snapshot_failed", seq, "no daily snapshot at this checkpoint: {e:#}");
            }
        }
    }
}

/// Publishes `job`'s snapshot in `dir` now, on this thread, as the writer's thread does: for a test or a
/// bench that writes snapshots at the checkpoints a log's replay passes.
/// Error cases:
/// - as `publish`, or the manifest cannot be read
#[cfg(test)]
pub(crate) fn publish_now(dir: &Path, job: Job) -> anyhow::Result<Listed> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut listed = read_manifest(dir)?;
    publish(dir, &mut listed, job)
}

/// Writes `job`'s snapshot in `dir` under its name, synced, then lists it in the manifest.
/// Error cases:
/// - the engine's encoding failed or never came, or the file or the manifest cannot be written or synced
fn publish(dir: &Path, listed: &mut Vec<Listed>, job: Job) -> anyhow::Result<Listed> {
    let engine = match job.engine {
        Encoding::Encoded(engine) => engine,
        Encoding::Pending(pending) => pending
            .recv()
            .context("the stats worker gave no encoding")?
            .map_err(|e| anyhow!("encoding the engine: {e}"))?,
    };
    let engine = RawValue::from_string(engine).context("the engine's encoding is not JSON")?;
    let file = crate::snapshot::name(job.offset);
    let staged = dir.join(format!("{file}.tmp"));
    let written = Written { schema: SCHEMA, offset: job.offset, checkpoint: &job.checkpoint, engine: &engine };
    let (hash, bytes) = write_synced(&staged, |out| serde_json::to_writer(out, &written).map_err(Into::into))?;
    fs::rename(&staged, dir.join(&file)).with_context(|| format!("publishing {file}"))?;
    sync_dir(dir)?;
    let listing = Listed {
        file,
        seq: job.offset.seq,
        index: job.offset.index,
        ts: job.ts,
        root: job.checkpoint.root.clone(),
        bytes,
        sha256: hex::encode(hash),
    };
    listed.push(listing.clone());
    write_manifest(dir, listed)?;
    Ok(listing)
}

/// Writes `listed` as the manifest in `dir`, replacing it whole.
/// Error cases:
/// - the manifest cannot be written, synced or renamed into place
fn write_manifest(dir: &Path, listed: &[Listed]) -> anyhow::Result<()> {
    let staged = dir.join(format!("{MANIFEST}.tmp"));
    write_synced(&staged, |out| {
        listed.iter().try_for_each(|listing| {
            serde_json::to_writer(&mut *out, listing)?;
            out.write_all(b"\n").map_err(Into::into)
        })
    })?;
    fs::rename(&staged, dir.join(MANIFEST)).context("publishing the daily snapshot manifest")?;
    sync_dir(dir)
}

/// Creates `path` and writes it through `write`, synced, with the SHA-256 of what it holds and its length;
/// a failed write removes it.
/// Error cases:
/// - `path` exists or cannot be created, or `write`, the flush or the sync fails
fn write_synced(
    path: &Path,
    write: impl FnOnce(&mut Hashed<BufWriter<File>>) -> anyhow::Result<()>,
) -> anyhow::Result<([u8; 32], u64)> {
    let written = (|| {
        let file = File::create_new(path).with_context(|| format!("creating {}", path.display()))?;
        let mut out = Hashed { inner: BufWriter::new(file), hash: Sha256::new(), bytes: 0 };
        write(&mut out)?;
        out.flush()?;
        out.inner.get_ref().sync_all().with_context(|| format!("syncing {}", path.display()))?;
        Ok((out.hash.finalize().into(), out.bytes))
    })();
    if written.is_err() {
        let _ = fs::remove_file(path);
    }
    written
}

/// Syncs `dir`, so the renames in it survive a crash.
/// Error cases:
/// - the directory cannot be opened or synced
fn sync_dir(dir: &Path) -> anyhow::Result<()> {
    File::open(dir).and_then(|dir| dir.sync_all()).with_context(|| format!("syncing {}", dir.display()))
}
