//! The node's writer of daily snapshots. The sequencer hands it each one with the engine's encoding, which
//! the stats worker gives only once the checkpoint's batch is durable, or encoded on the sequencer's thread
//! after a sync of the log, with the enclave key that signed its checkpoint, and a thread of its own signs it
//! over the encoding's SHA-256 (`Signing`) and publishes it: the file under its name, then its line in the manifest, each synced before
//! the next, so a crash leaves a listed snapshot whole, or an unlisted file
//! the next start removes. A day whose snapshot the thread cannot take, or fails to write, pages Warning
//! `daily_snapshot_failed` and has none, which merges two of an audit's ranges; the log stays the authority.
//! The node keeps every day's snapshot.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, channel, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Context;
use engine_core::types::Millis;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::daily_snapshots::{Listed, MANIFEST, SCHEMA, Signing, Written, day, offset_named, read_manifest};
use crate::log::{Checkpoint, Offset};
use crate::snapshot::sealing::{ENCODE_WAIT, Encoding};

/// A daily snapshot for the thread to publish.
pub(crate) struct Job {
    pub offset: Offset,
    pub checkpoint: Checkpoint,
    /// The checkpoint's sequenced time.
    pub ts: Millis,
    pub engine: Encoding,
    /// The key that signed the checkpoint, which signs the snapshot.
    pub signing: Signing,
}

/// The longest a writer's drop waits for its thread to finish the snapshot it holds, so a slow write never
/// holds the sequencer's stop, a handover's included; a snapshot left to finish after it is an unlisted file
/// the next start removes if the process ends first.
const DROP_WAIT: Duration = Duration::from_secs(2);

/// The sequencer's end of the writer.
pub(crate) struct Writer {
    /// Closed when the writer is dropped, which ends the thread once it has published what it holds.
    jobs: Option<SyncSender<(Job, bool)>>,
    /// The sequenced time of the newest snapshot listed, handed over, or of a day skipped.
    last_ts: Option<Millis>,
    /// Whether the thread waits for a snapshot: cleared as it takes one, set once that is written or fails.
    idle: Arc<AtomicBool>,
    /// Disconnected once the thread has ended.
    ended: Receiver<()>,
    thread: Option<JoinHandle<()>>,
    /// Whether `daily_snapshot_failed` is raised for a day this end skipped or could not encode, which the next
    /// snapshot handed over resolves.
    failing: bool,
}

/// Feeds bytes to `inner`, a file or nowhere, and to their SHA-256 hash, counting them.
pub(super) struct Hashed<W> {
    pub(super) inner: W,
    pub(super) hash: Sha256,
    bytes: u64,
}

impl Drop for Writer {
    /// Closes the queue and joins the thread, which publishes the snapshot it holds first, or, after
    /// `DROP_WAIT`, leaves it to finish.
    fn drop(&mut self) {
        self.jobs = None;
        if let Err(RecvTimeoutError::Timeout) = self.ended.recv_timeout(DROP_WAIT) {
            warn!("the daily snapshot writer is still writing; it is left to finish");
            return;
        }
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

impl<W> Hashed<W> {
    /// Nothing fed yet to `inner`.
    pub(super) fn new(inner: W) -> Self {
        Self { inner, hash: Sha256::new(), bytes: 0 }
    }
}

impl Writer {
    /// Starts the writer for `dir`, putting it in order after the last start first: staging files go, so
    /// do snapshot files the manifest does not list, and listings past `last_checkpoint`, the seq of the log's
    /// last checkpoint, which a log cut back no longer holds.
    /// Error cases:
    /// - `dir` cannot be created, listed or cleaned, its manifest cannot be read or rewritten, or the
    ///   thread cannot start
    pub(crate) fn start(dir: &Path, last_checkpoint: Option<u64>) -> anyhow::Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut listed = read_manifest(dir)?;
        let held = listed.len();
        listed.retain(|listing| last_checkpoint.is_some_and(|last| listing.seq <= last));
        if listed.len() < held {
            warn!(dropped = held - listed.len(), "daily snapshots past the log's last checkpoint are dropped");
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
        let (ending, ended) = channel();
        let idle = Arc::new(AtomicBool::new(true));
        let (owned, set) = (dir.to_owned(), idle.clone());
        // The thread logs to the starter's subscriber, as the process's default does for every thread.
        let logging = tracing::dispatcher::get_default(Clone::clone);
        let thread = thread::Builder::new()
            .name("daily-snapshots".into())
            .spawn(move || {
                let _ending = ending;
                tracing::dispatcher::with_default(&logging, || run(&owned, listed, taken, &set));
            })
            .context("starting the daily snapshot writer")?;
        Ok(Self { jobs: Some(jobs), last_ts, idle, ended, thread: Some(thread), failing: false })
    }

    /// Closes the queue and waits for the thread to publish what it holds, however long that takes, for a test
    /// that reads what it wrote.
    #[cfg(test)]
    pub(crate) fn finish(mut self) {
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            thread.join().expect("the writer's thread does not panic");
        }
    }

    /// Whether a snapshot is due at a checkpoint stamped `ts`: none is taken yet, or the newest is on an
    /// earlier UTC day.
    pub(crate) fn due(&self, ts: Millis) -> bool {
        self.last_ts.is_none_or(|last| day(ts) > day(last))
    }

    /// Whether the thread can take a snapshot now: it runs, and is not writing the last one. The caller asks
    /// before it has the engine encoded, and skips the day (`skip`) when not.
    pub(crate) fn ready(&self) -> bool {
        self.jobs.is_some() && self.idle.load(Ordering::Acquire)
    }

    /// Hands `job` to the thread, which takes its day, and resolves the alert this end raised once it is
    /// published. A thread that cannot take it skips the day.
    pub(crate) fn take(&mut self, job: Job) {
        let (seq, ts) = (job.offset.seq, job.ts);
        let refused = match self.jobs.as_ref() {
            Some(jobs) => match jobs.try_send((job, self.failing)) {
                Ok(()) => {
                    self.last_ts = Some(ts);
                    self.failing = false;
                    return;
                }
                Err(TrySendError::Full(_)) => "the writer is still writing the last one",
                Err(TrySendError::Disconnected(_)) => "the writer has stopped",
            },
            None => "the writer has stopped",
        };
        self.skip(seq, ts, refused);
    }

    /// Notes that the sequencer raised `daily_snapshot_failed` for a snapshot it could not encode, so the next
    /// snapshot handed over resolves it.
    pub(crate) fn encoding_failed(&mut self) {
        self.failing = true;
    }

    /// Takes no snapshot on the UTC day of the checkpoint at `seq`, stamped `ts`, for `reason`, which pages
    /// Warning `daily_snapshot_failed` once that day; the next day's first checkpoint tries again.
    pub(crate) fn skip(&mut self, seq: u64, ts: Millis, reason: &str) {
        self.last_ts = Some(ts);
        self.failing = true;
        warn!(alert = "daily_snapshot_failed", "no daily snapshot today, at seq {seq}: {reason}");
    }
}

/// The thread: publishes each snapshot it is handed in `dir`, whose manifest lists `listed`, `idle` cleared
/// while it does, until the sequencer's end closes.
fn run(dir: &Path, mut listed: Vec<Listed>, jobs: Receiver<(Job, bool)>, idle: &AtomicBool) {
    let mut failing = false;
    for (job, resolves) in jobs {
        idle.store(false, Ordering::Release);
        let seq = job.offset.seq;
        match publish(dir, &mut listed, job) {
            Ok(listing) => {
                info!(seq, file = %listing.file, bytes = listing.bytes, "daily snapshot published");
                if std::mem::take(&mut failing) || resolves {
                    info!(alert_resolved = "daily_snapshot_failed", "a daily snapshot is published");
                }
            }
            Err(e) => {
                failing = true;
                warn!(alert = "daily_snapshot_failed", "no daily snapshot today, at seq {seq}: {e:#}");
            }
        }
        idle.store(true, Ordering::Release);
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

/// Writes `job`'s snapshot in `dir` under its name, signed with its key over the SHA-256 of the engine's
/// encoding as written, which is its state digest (`state_digest`), synced, then lists it in the manifest.
/// Error cases:
/// - the engine's encoding failed or did not come within `ENCODE_WAIT`, or the checkpoint's root does not
///   parse, or the file or the manifest cannot be written or synced
fn publish(dir: &Path, listed: &mut Vec<Listed>, job: Job) -> anyhow::Result<Listed> {
    // Bounded, so a job a failing sequencer never delivered cannot hold the writer's drop; nothing stops
    // the writer mid-job.
    let encoded = job.engine.wait(ENCODE_WAIT, || false)?;
    let engine = RawValue::from_string(encoded).context("the engine's encoding is not JSON")?;
    let state: [u8; 32] = Sha256::digest(engine.get().as_bytes()).into();
    let root = job.checkpoint.root.parse().context("the snapshot's checkpoint root")?;
    let (state, signature) = (hex::encode(state), job.signing.sign(job.offset, root, state));
    let file = crate::snapshot::name(job.offset);
    let staged = dir.join(format!("{file}.tmp"));
    let written = Written {
        schema: SCHEMA,
        offset: job.offset,
        checkpoint: &job.checkpoint,
        engine: &engine,
        state: &state,
        signature: &signature,
    };
    let (hash, bytes) = write_synced(&staged, |out| serde_json::to_writer(out, &written).map_err(Into::into))?;
    publish_staged(&staged, &dir.join(&file)).with_context(|| format!("publishing {file}"))?;
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
pub(super) fn write_manifest(dir: &Path, listed: &[Listed]) -> anyhow::Result<()> {
    let staged = dir.join(format!("{MANIFEST}.tmp"));
    write_synced(&staged, |out| {
        listed.iter().try_for_each(|listing| {
            serde_json::to_writer(&mut *out, listing)?;
            out.write_all(b"\n").map_err(Into::into)
        })
    })?;
    publish_staged(&staged, &dir.join(MANIFEST)).context("publishing the daily snapshot manifest")?;
    sync_dir(dir)
}

/// Renames `staged` to `to`, or removes it when the rename fails, so the next write can stage it again.
/// Error cases:
/// - the rename fails
pub(super) fn publish_staged(staged: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(staged, to).inspect_err(|_| {
        let _ = fs::remove_file(staged);
    })
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
        let mut out = Hashed::new(BufWriter::new(file));
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
pub(super) fn sync_dir(dir: &Path) -> anyhow::Result<()> {
    File::open(dir).and_then(|dir| dir.sync_all()).with_context(|| format!("syncing {}", dir.display()))
}
