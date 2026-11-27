//! What makes the soak's node one with the storage features on, as the launch configuration runs them: no
//! `[snapshots]`, and `[daily_snapshots]` and `[archive]` with a `local_window`. Its log's ranges and the tails of its
//! checkpoints, and each daily snapshot, go to a loopback stand-in for the bucket (`archive::stand_in`) below a
//! stand-in vault that finalizes each checkpoint a little after the node reports it final; the log's local window
//! removes the segments the bucket holds and the node's newest snapshot does not need, the snapshot uploader removes
//! the host's older snapshots, and a restart goes on from the newest snapshot and the log after it.
//!
//! The history index is the release configuration's, unpaired and in `history.dir` (`indexed`), which the window's
//! floors read. The node's `[snapshots]` and this are exclusive (`archive::Window::for_config` leaves the window off
//! beside a paired history index, which rebuilds from the whole log), so the soak runs one shape or the other
//! (`SOAK_STORAGE`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::AbortHandle;

use crate::archive::stand_in::{StandIn, Vault};
use crate::archive::window::{RestoreSources, Window, millis};
use crate::archive::{Bucket, Uploader, log_prefix};
use crate::common::{CHAIN_ID, VAULT};
use crate::config::{ArchiveSection, DailySnapshotsSection, HistorySection, NodeConfig};
use crate::daily_snapshots::{SnapshotUploader, snapshots_prefix};
use crate::load_scenario::durable::Durable;
use crate::log::Manifest;
use crate::metrics::Metrics;

/// How long after the node reports a checkpoint final the stand-in vault finalizes it: the span in which the
/// checkpoint's tail is the bucket's only copy of what follows the vault's finalized one.
const FINALITY_LAG: Duration = Duration::from_secs(20);
/// How long a whole range, or a snapshot, may wait for the bucket before `archive_upload_lagging`.
const LAG_ALERT: Duration = Duration::from_secs(10 * 60);

/// The bucket and vault the run's nodes upload to, and the windows they keep, which follow from the day the
/// snapshots are counted by.
pub(super) struct Archived {
    pub bucket: StandIn,
    pub vault: Vault,
    /// Forgets the tails the vault's finality has passed, as a bucket's lifecycle rule would (`forget_tails`).
    forgetting: AbortHandle,
    /// The day a daily snapshot is taken at the first checkpoint of: a UTC day, or the shorter one a run that
    /// cannot cross enough midnights sets (`SOAK_DAY_MINUTES`).
    pub day: Duration,
}

/// A task of the node's storage, by what it is, which a node that stops aborts.
pub(super) type StorageTask = (&'static str, AbortHandle);

/// What the bucket and the host hold of the log and the snapshots.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Counts {
    /// The ranges of the log in the bucket, and the bytes they take compressed.
    pub ranges: u64,
    pub range_bytes: u64,
    /// The tails of checkpoints in the bucket, and their bytes.
    pub tails: u64,
    pub tail_bytes: u64,
    /// The snapshots in the bucket, and their bytes.
    pub snapshots: u64,
    pub snapshot_bytes: u64,
}

impl Counts {
    /// Every byte the bucket holds, which the soak's own process keeps in memory.
    pub(super) fn bytes(&self) -> u64 {
        self.range_bytes + self.tail_bytes + self.snapshot_bytes
    }
}

impl Archived {
    /// The stand-in bucket and vault, for snapshots counted by days of `day`.
    pub(super) async fn start(day: Duration) -> Arc<Self> {
        let (bucket, vault) = (StandIn::start().await, Vault::default());
        let forgetting = tokio::spawn(forget_tails(bucket.handle(), vault.clone())).abort_handle();
        Arc::new(Self { bucket, vault, forgetting, day })
    }

    /// How long the host keeps the log's segments the bucket holds: a day.
    pub(super) fn log_window(&self) -> Duration {
        self.day
    }

    /// How long the host keeps a snapshot the bucket holds: a day and a half, so the window's edge falls between days.
    pub(super) fn snapshot_window(&self) -> Duration {
        self.day * 3 / 2
    }

    /// The history index's retention, half the log's window, which the window must cover.
    pub(super) fn retention(&self) -> Duration {
        self.day / 2
    }

    /// Sets `cfg` as the launch configuration is: no `[snapshots]`, daily snapshots in `daily`, the archive naming the
    /// stand-in with the log's window, and the unpaired history index (`indexed`) and the alert outbox in
    /// directories of their own for the disk watcher to read.
    pub(super) fn configure(&self, cfg: &mut NodeConfig, run: &std::path::Path) {
        cfg.snapshots = None;
        cfg.daily_snapshots =
            Some(DailySnapshotsSection { dir: run.join("daily-snapshots"), local_window: self.snapshot_window() });
        cfg.archive = Some(ArchiveSection { local_window: Some(self.log_window()), ..self.bucket.section(LAG_ALERT) });
        // The disk watcher judges a volume by how full it is, not by the run's own growth over a short window.
        cfg.alerts.storage.warn_fill_within = Duration::from_secs(120);
        cfg.alerts.storage.critical_fill_within = Duration::from_secs(60);
        cfg.history.dir = run.join("history");
        cfg.history.retention = self.retention();
        cfg.alerts.outbox.dir = run.join("alert-outbox");
        for dir in [&cfg.history.dir, &cfg.alerts.outbox.dir] {
            std::fs::create_dir_all(dir).expect("a store's directory");
        }
    }

    /// The directory the node's daily snapshots are in.
    pub(super) fn snapshots_dir(cfg: &NodeConfig) -> PathBuf {
        cfg.daily_snapshots.as_ref().expect("[daily_snapshots]").dir.clone()
    }

    /// Has `cfg` read the archive over the stand-in's plain `http`, as a reader that trusts only the public roots
    /// reads it, which `archive::restore_whole` does.
    pub(super) fn read_plain(&self, cfg: &mut NodeConfig) {
        cfg.archive.as_mut().expect("[archive]").public_url = self.bucket.plain.clone();
    }

    /// Runs the node's storage beside its sequencer, whose log is the one `manifest` publishes, as `node::run` runs
    /// them: the log's archive uploader and its local window, the daily snapshots' uploader, and the disk watcher.
    /// Each ends with a line naming why, and the node reports a task that has.
    pub(super) fn run_beside(
        &self,
        cfg: &NodeConfig,
        manifest: watch::Receiver<Manifest>,
        metrics: &Arc<Metrics>,
    ) -> Vec<StorageTask> {
        let prefix = log_prefix(CHAIN_ID, VAULT);
        let (log, snapshots) = (cfg.node.log_dir.clone(), Self::snapshots_dir(cfg));
        let (uploader, sources) = Uploader::new(
            self.bucket.store(&prefix),
            log.clone(),
            LAG_ALERT,
            metrics.clone(),
            Some(self.vault.clone()),
        );
        let history = HistorySection { dir: cfg.history.dir.clone(), retention: cfg.history.retention, marks: None };
        let restore = self.restore_sources();
        let window = Window::new(log, millis(self.log_window()), snapshots.clone(), history, restore).following(
            sources.subscribe(),
            uploader.archived(),
            metrics.clone(),
        );
        sources.send_replace(Some(manifest));
        let snapshot_store = self.bucket.store(&snapshots_prefix(CHAIN_ID, VAULT));
        let timing = (self.snapshot_window(), LAG_ALERT);
        let snapshots = SnapshotUploader::new(snapshot_store, snapshots, timing, metrics.clone());
        let watcher = crate::storage::run(crate::storage::Layout::of(cfg), cfg.alerts.storage.clone(), metrics.clone());
        vec![
            spawn_named("the log archive's uploader", async move {
                let _sources = sources;
                uploader.run().await
            }),
            spawn_named("the log's local window", window.run()),
            spawn_named("the daily snapshots' uploader", snapshots.run()),
            spawn_named("the disk watcher", watcher),
        ]
    }

    /// Where a segment the host lacks is read from: the stand-in's public origin.
    pub(super) fn restore_sources(&self) -> RestoreSources {
        let bucket = self.bucket_of(&log_prefix(CHAIN_ID, VAULT));
        RestoreSources { http: self.bucket.client.clone(), bucket, nodes: Vec::new() }
    }

    /// The bucket's log under `prefix`, as a reader of its public origin names it.
    fn bucket_of(&self, prefix: &str) -> Bucket {
        Bucket::new(&self.bucket.url, prefix).expect("the stand-in's bucket")
    }

    /// Has the stand-in vault finalize the checkpoint at `seq`, rooted at `root`, `FINALITY_LAG` from now.
    pub(super) fn finalize_later(self: &Arc<Self>, seq: u64, root: String) {
        let archived = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FINALITY_LAG).await;
            archived.vault.finalize(seq, &root);
        });
    }

    /// What the bucket holds now, by what each object is.
    pub(super) fn counts(&self) -> Counts {
        let snapshots = snapshots_prefix(CHAIN_ID, VAULT);
        self.bucket.with(|bucket| {
            let mut counts = Counts::default();
            for (key, object) in &bucket.objects {
                let bytes = object.len() as u64;
                let (number, size) = if key.starts_with(&snapshots) {
                    (&mut counts.snapshots, &mut counts.snapshot_bytes)
                } else if key.contains("/tail/") {
                    (&mut counts.tails, &mut counts.tail_bytes)
                } else {
                    (&mut counts.ranges, &mut counts.range_bytes)
                };
                *number += 1;
                *size += bytes;
            }
            counts
        })
    }
}

/// Runs `task` on its own task, which says why it ended, as no storage task of the node ends while it runs.
fn spawn_named(
    name: &'static str,
    task: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
) -> StorageTask {
    let task = tokio::spawn(async move {
        let ended = task.await;
        println!("SOAK {name} stopped: {ended:?}");
    });
    (name, task.abort_handle())
}

impl Durable {
    /// This run's storage, when the node runs the launch configuration.
    pub(super) fn archived(&self) -> Option<&Arc<Archived>> {
        self.archived.as_ref()
    }
}

impl Drop for Archived {
    fn drop(&mut self) {
        self.forgetting.abort();
    }
}

/// How often the bucket drops the tails the vault has finalized past.
const FORGET_EVERY: Duration = Duration::from_secs(10);

/// Drops, every `FORGET_EVERY`, each checkpoint's tail below the vault's finalized checkpoint, which no reader takes
/// once the ranges hold what it did, and keeps the finalized checkpoint's own, whose range is not whole until a later
/// checkpoint is finalized: a bucket's lifecycle rule for them. Without it a run holds every tail in
/// this process's memory, which at ten times launch flow is more than the log itself.
async fn forget_tails(bucket: crate::archive::stand_in::Handle, vault: Vault) {
    use crate::archive::Finality;
    loop {
        tokio::time::sleep(FORGET_EVERY).await;
        if let Ok(Some((finalized, _))) = vault.finalized().await {
            bucket.with(|held| held.objects.retain(|key, _| tail_seq(key).is_none_or(|seq| seq >= finalized)));
        }
    }
}

/// The checkpoint seq a tail's object key names, `log/<chain>/<vault>/tail/<seq>-<root>/<range>`; none for any other.
fn tail_seq(key: &str) -> Option<u64> {
    key.split_once("/tail/")?.1.split_once('-')?.0.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tails_key_names_its_checkpoint_and_no_other_key_does() {
        assert_eq!(tail_seq("log/31337/0x00/tail/1200-0xabc/00000000000000000000.jsonl.zst"), Some(1_200));
        assert_eq!(tail_seq("log/31337/0x00/00000000000000010000.jsonl.zst"), None);
        assert_eq!(tail_seq("snapshots/31337/0x00/5-1.json"), None);
    }
}
