//! What the soak reports and judges of the node's storage when it runs the launch configuration
//! (`load_scenario::archived`): what its archive's bucket holds of the log, its checkpoints' tails and its daily
//! snapshots, what the log's local window and the snapshots' window have removed from the host, and the bytes each
//! store holds. An interval fails when a storage task has ended or the uploads fall behind; the run fails when it
//! was long enough to have uploaded and pruned and did not, or when none of its late restarts came back with part of
//! the log off the host. An alert of the archive or the snapshots that no event of the run explains fails its
//! interval as any other does (`alerts`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::archive::window::{Checked, Floors};
use crate::config::HistorySection;
use crate::daily_snapshots::read_manifest;
use crate::load_scenario::archived::{Archived, Counts};
use crate::load_scenario::node::Node;
use crate::log::{Manifest, list_segments};

/// How many whole ranges the bucket may lack at an interval's end, those waiting for the vault's finality among
/// them: the uploads keep up with the log.
const PENDING_RANGES: i64 = 6;
/// How many days of the run pass before it must have uploaded, and before it must have pruned: the log's window is a
/// day, and the newest snapshot a segment ends before comes at the first day's end.
const UPLOADED_AFTER_DAYS: u32 = 1;
const PRUNED_AFTER_DAYS: u32 = 3;

/// The storage the run holds on its disk and in its bucket at one moment.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Figures {
    pub bucket: Counts,
    /// The segments the log's manifest lists, and how many the host no longer holds.
    pub segments: u64,
    pub off_host: u64,
    /// The first seq of the oldest segment on the host.
    pub first_seq: u64,
    pub log_bytes: u64,
    /// The snapshots the manifest lists, and how many the host no longer holds.
    pub snapshots: u64,
    pub snapshots_off_host: u64,
    pub daily_bytes: u64,
    /// The newest snapshot's bytes, which the state's size sets.
    pub newest_snapshot_bytes: u64,
}

/// The run's storage: where it is, and what its restarts found.
pub(super) struct Stored {
    pub archived: Arc<Archived>,
    log: PathBuf,
    daily: PathBuf,
    /// The history section the window reads its floors from.
    history: HistorySection,
    /// For each restart, how long into the run it was and how many segments the host lacked then.
    restarts: Vec<(Duration, u64)>,
}

impl Stored {
    pub(super) fn new(archived: Arc<Archived>, log: PathBuf, daily: PathBuf, history: HistorySection) -> Self {
        Self { archived, log, daily, history, restarts: Vec::new() }
    }

    /// What holds the oldest segment on the host, as the window asks it: the floors the daily snapshots and the
    /// history set, and whether the bucket gives the segment back.
    pub(super) async fn holding(&self) -> String {
        let local = list_segments(&self.log).unwrap_or_default();
        let listed = Manifest::load(&self.log).map(|manifest| manifest.segments).unwrap_or_default();
        let Some(oldest) = local.first().and_then(|(seq, _)| listed.iter().find(|s| s.first_seq == *seq)) else {
            return "no listed segment on the host".to_owned();
        };
        let daily = self.daily.clone();
        let history = HistorySection { dir: self.history.dir.clone(), retention: self.history.retention, marks: None };
        let now = crate::clock::now_ms();
        let floors =
            tokio::task::spawn_blocking(move || Floors::read(&daily, &history, u64::MAX, now, &mut Checked::default()))
                .await;
        let back = self.archived.restore_sources().read_back(oldest).await.map(|bytes| bytes.len());
        format!(
            "oldest segment {}..{}: floors {floors:?}, read back from the bucket {back:?}",
            oldest.first_seq, oldest.last_seq
        )
    }

    /// What the bucket and the host's stores hold now.
    pub(super) fn figures(&self) -> Figures {
        let listed = Manifest::load(&self.log).map(|manifest| manifest.segments).unwrap_or_default();
        let local = list_segments(&self.log).unwrap_or_default();
        let first_seq = local.first().map_or(0, |(seq, _)| *seq);
        let held = |file: &str| local.iter().any(|(_, path)| path.file_name().is_some_and(|name| name == file));
        let off_host = listed.iter().filter(|segment| !held(&segment.file)).count() as u64;
        let snapshots = read_manifest(&self.daily).unwrap_or_default();
        let snapshots_off_host = snapshots.iter().filter(|s| !self.daily.join(&s.file).exists()).count() as u64;
        Figures {
            bucket: self.archived.counts(),
            segments: listed.len() as u64,
            off_host,
            first_seq,
            log_bytes: super::hours::dir_bytes(&self.log),
            snapshots: snapshots.len() as u64,
            snapshots_off_host,
            daily_bytes: super::hours::dir_bytes(&self.daily),
            newest_snapshot_bytes: snapshots.last().map_or(0, |listing| listing.bytes),
        }
    }

    /// Waits until the daily snapshot writer of the node that stopped has ended its work, which the node's drop waits
    /// two seconds for: a process that is gone leaves no thread to write a snapshot or to lose the files a new
    /// writer cleans from the directory, so no restart starts before the staged files are published or removed.
    pub(super) async fn writer_settled(&self) {
        let staged = || {
            std::fs::read_dir(&self.daily)
                .map(|listing| {
                    listing.filter_map(Result::ok).any(|item| item.file_name().to_string_lossy().contains(".tmp"))
                })
                .unwrap_or(false)
        };
        let (mut quiet, waited) = (0, std::time::Instant::now());
        while quiet < 3 && waited.elapsed() < Duration::from_secs(120) {
            quiet = if staged() { 0 } else { quiet + 1 };
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Notes that a restart `at` into the run found the host lacking `off_host` segments of the log.
    pub(super) fn restarted(&mut self, at: Duration, off_host: u64) {
        self.restarts.push((at, off_host));
    }

    /// The interval's lines: the bucket's contents, the node's uploads and what the host has let go.
    pub(super) fn print(&self, leg: &str, figures: &Figures, node: &Node) {
        let (bucket, archive) = (&figures.bucket, &node.metrics.archive);
        println!(
            "SOAK {leg} archive ranges={} tails={} snapshots={} bucket_bytes={} (ranges={} tails={} snapshots={}) \
             node_confirmed_ranges={} pending_ranges={} snapshots_pending={}",
            bucket.ranges,
            bucket.tails,
            bucket.snapshots,
            bucket.bytes(),
            bucket.range_bytes,
            bucket.tail_bytes,
            bucket.snapshot_bytes,
            archive.confirmed_ranges.get(),
            archive.pending_ranges.get(),
            node.metrics.snapshot_archive.pending.get(),
        );
        println!(
            "SOAK {leg} pruned log_segments_off_host={}/{} first_seq_on_host={} snapshots_off_host={}/{} \
             window_passes={} window_failures={} window_removed_since_start={} window_oldest_held_s={:.0}",
            figures.off_host,
            figures.segments,
            figures.first_seq,
            figures.snapshots_off_host,
            figures.snapshots,
            archive.window.passes.get(),
            archive.window.failures.get(),
            archive.window.removed.get(),
            archive.window.held_seconds.get(),
        );
        println!(
            "SOAK {leg} disk_bytes_storage log_on_host={} daily_snapshots={} bucket_held_in_this_process={}",
            figures.log_bytes,
            figures.daily_bytes,
            bucket.bytes()
        );
    }

    /// What fails an interval of the node's storage: a task of it ended, the bucket is behind the log, or a
    /// snapshot waits for it.
    pub(super) fn failures(&self, node: &Node) -> Vec<String> {
        let mut failed: Vec<String> = node.storage_ended().iter().map(|name| format!("{name} stopped")).collect();
        let behind = node.metrics.archive.pending_ranges.get();
        if behind > PENDING_RANGES {
            failed.push(format!("{behind} whole ranges of the log wait for the bucket, over {PENDING_RANGES}"));
        }
        let waiting = node.metrics.snapshot_archive.pending.get();
        if waiting > 1 {
            failed.push(format!("{waiting} daily snapshots wait for the bucket"));
        }
        failed
    }

    /// What fails the run once it has ended with `figures`, `elapsed` after it began, days of `day`: it was long
    /// enough to upload and did not, long enough to prune the log and the snapshots from the host and did not, or
    /// none of its restarts from three days on came back with part of the log off the host.
    pub(super) fn conclusion(&self, figures: &Figures, elapsed: Duration, day: Duration) -> Vec<String> {
        let mut failed = Vec::new();
        if elapsed >= day * UPLOADED_AFTER_DAYS {
            if figures.bucket.ranges == 0 {
                failed.push("no range of the log reached the bucket".to_owned());
            }
            if figures.bucket.snapshots == 0 {
                failed.push("no daily snapshot reached the bucket".to_owned());
            }
        }
        if elapsed >= day * PRUNED_AFTER_DAYS {
            if figures.segments > 3 && figures.off_host == 0 {
                failed.push("no segment of the log left the host".to_owned());
            }
            if figures.snapshots > 3 && figures.snapshots_off_host == 0 {
                failed.push("no daily snapshot left the host".to_owned());
            }
        }
        let late: Vec<_> = self.restarts.iter().filter(|(at, _)| *at >= day * PRUNED_AFTER_DAYS).collect();
        if !late.is_empty() && late.iter().all(|(_, off_host)| *off_host == 0) && figures.off_host > 0 {
            failed.push("no restart from three days on came back with part of the log off the host".to_owned());
        }
        failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn stored() -> Stored {
        let archived = Archived::start(Duration::from_secs(120)).await;
        let history = HistorySection { dir: PathBuf::from("/none/history"), retention: DAY / 2, marks: None };
        Stored::new(archived, PathBuf::from("/none/log"), PathBuf::from("/none/daily"), history)
    }

    const DAY: Duration = Duration::from_secs(120);

    #[tokio::test]
    async fn a_run_too_short_to_upload_or_prune_is_not_judged_on_either() {
        let stored = stored().await;
        assert!(stored.conclusion(&Figures::default(), DAY / 2, DAY).is_empty());
        let uploaded = stored.conclusion(&Figures::default(), DAY, DAY);
        assert_eq!(uploaded.len(), 2, "after a day, a range and a snapshot are in the bucket: {uploaded:?}");
        let long = Figures { segments: 8, snapshots: 5, ..Figures::default() };
        let pruned = stored.conclusion(&long, DAY * 3, DAY);
        assert!(pruned.iter().any(|why| why.contains("segment of the log left")), "{pruned:?}");
        assert!(pruned.iter().any(|why| why.contains("snapshot left the host")), "{pruned:?}");
    }

    #[tokio::test]
    async fn a_run_that_pruned_passes_and_a_late_restart_must_have_found_the_log_pruned() {
        let mut stored = stored().await;
        let bucket = Counts { ranges: 3, snapshots: 2, ..Counts::default() };
        let pruned =
            Figures { bucket, segments: 8, off_host: 3, snapshots: 5, snapshots_off_host: 2, ..Figures::default() };
        assert!(stored.conclusion(&pruned, DAY * 4, DAY).is_empty());
        stored.restarted(DAY * 2, 0);
        assert!(stored.conclusion(&pruned, DAY * 4, DAY).is_empty(), "a restart before pruning is not held to it");
        stored.restarted(DAY * 3, 0);
        let failed = stored.conclusion(&pruned, DAY * 4, DAY);
        assert_eq!(failed.len(), 1, "{failed:?}");
        stored.restarted(DAY * 3, 2);
        assert!(stored.conclusion(&pruned, DAY * 4, DAY).is_empty(), "one that found it pruned is enough");
    }
}
