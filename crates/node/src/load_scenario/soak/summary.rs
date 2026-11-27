//! The end of each interval and of the run: the interval's figures and verdict, the node's stop, and the run's
//! conclusion.

use std::path::PathBuf;

use engine_core::types::AccountId;

use super::Soak;
use super::hours::{Disk, Standing, baseline, dir_bytes, failures, median, memory_failure};
use super::stored::index_failures;
use super::support::{memory_mib, newest_cache};
use super::timeline::stop_alerts;

const MIB: u64 = 1 << 20;

impl Soak {
    /// Judges and prints the interval that has ended.
    pub(super) async fn report(&mut self, leg: &str, commands: f64) {
        self.alerts.drain();
        let mut interval = std::mem::take(&mut self.interval);
        let (unexplained, counts) = self.alerts.report(&self.timeline);
        interval.unexpected_alerts = unexplained;
        let mut failed = failures(&interval, !self.config.short);
        let stored = self.stored.as_ref().map(|stored| stored.figures());
        let disk = Disk {
            log: dir_bytes(&self.durable.cfg.node.log_dir),
            snapshots: self.durable.snapshots().map_or(0, dir_bytes),
            index: self.stored.as_ref().map_or(0, |_| dir_bytes(&self.durable.cfg.history.dir)),
        };
        let rss: Vec<u64> = self.samples.iter().map(|(rss, _)| *rss).collect();
        let standing = Standing {
            rss_mib: if rss.is_empty() { memory_mib().0 } else { median(&rss) },
            state_bytes: stored.map_or_else(|| newest_cache(&self.durable), |figures| figures.newest_snapshot_bytes),
        };
        let held = stored.map_or(0, |figures| figures.bucket.bytes());
        if self.first.is_none() {
            self.held_at_first = held;
        }
        self.first = baseline(self.first, standing);
        let first = self.first.unwrap_or(standing);
        if !self.config.short && self.first.is_some() {
            // The stand-in bucket is held in this process's memory, which the node's is not.
            let bucket_mib = held.saturating_sub(self.held_at_first) / MIB;
            failed.extend(memory_failure(first, standing, bucket_mib));
        }
        let (runs, behind_index) = (self.node().history_runs(), self.node().history_behind().await);
        if self.stored.is_none() && !runs {
            failed.push("the history owner stopped, so the node takes no more recovery caches".to_owned());
        }
        if let Some(stored) = &self.stored {
            failed.extend(stored.failures(self.node()));
            failed.extend(index_failures(runs, behind_index));
        }
        let (excused, judged) =
            interval.windows.iter().fold((0, 0), |(excused, judged), window| match window.excused {
                Some(_) => (excused + 1, judged),
                None => (excused, judged + 1),
            });
        let behind = self.samples.iter().map(|(_, behind)| *behind).max().unwrap_or(0);
        let grown = disk.log.saturating_sub(self.disk.log);
        println!("SOAK {leg} windows={} judged={judged} excused={excused}", interval.windows.len());
        println!(
            "SOAK {leg} memory {} median={} max={} first_interval_median={} newest_cache_bytes={}",
            memory_mib().1,
            standing.rss_mib,
            self.samples.iter().map(|(rss, _)| *rss).max().unwrap_or(0),
            first.rss_mib,
            standing.state_bytes
        );
        println!(
            "SOAK {leg} disk_bytes log={} caches_and_history_index={} history_index={} marks_cache=not run \
             alert_outbox=not run; log_growth_bytes={grown} commands={commands:.0} log_bytes_per_command={:.0}",
            disk.log,
            disk.snapshots,
            disk.index,
            grown as f64 / commands.max(1.0)
        );
        println!("SOAK {leg} stats_worker_behind_max_commands={behind}");
        match (&self.stored, stored) {
            (Some(stored), Some(figures)) => {
                stored.print(leg, &figures, self.node());
                println!(
                    "SOAK {leg} history_index runs={runs} behind_commands={behind_index:?} bytes={} retention_s={}",
                    disk.index,
                    self.durable.cfg.history.retention.as_secs()
                );
                println!("SOAK {leg} window_probe {}", stored.holding().await);
            }
            _ => println!(
                "SOAK {leg} history_owner takes_captures={} index_behind_commands={:?}",
                self.node().history_runs(),
                self.node().history_behind().await
            ),
        }
        println!("SOAK {leg} alerts raised: {counts:?}");
        let verdict = if failed.is_empty() { "PASS".to_owned() } else { format!("FAIL {failed:?}") };
        println!("SOAK {leg} verdict {verdict}");
        self.disk = disk;
        self.samples.clear();
        self.failed.push((leg.to_owned(), failed));
    }

    /// Stops the node, and returns its signer and log, to replay.
    pub(super) async fn finish(&mut self) -> (AccountId, PathBuf) {
        let place = self.timeline.begin("stop", &stop_alerts());
        let node = self.node.take().expect("a node");
        let (signer, log_dir) = (node.signer, node.log_dir.clone());
        node.stop(false).await.expect("the node drains");
        self.alerts.drain();
        self.timeline.end(place);
        // What ended after the last report: a fault, a restart or an alert no interval has judged.
        let mut tail = std::mem::take(&mut self.interval);
        (tail.unexpected_alerts, _) = self.alerts.report(&self.timeline);
        self.failed.push(("after the last interval".to_owned(), failures(&tail, false)));
        if let Some(stored) = &self.stored {
            let figures = stored.figures();
            println!(
                "SOAK storage at the end: bucket ranges={} tails={} snapshots={}; the host lacks {} of {} segments of the \
                 log and {} of {} snapshots",
                figures.bucket.ranges,
                figures.bucket.tails,
                figures.bucket.snapshots,
                figures.off_host,
                figures.segments,
                figures.snapshots_off_host,
                figures.snapshots
            );
            let why = stored.conclusion(&figures, self.driven.elapsed(), self.config.day);
            self.failed.push(("storage at the end".to_owned(), why));
        }
        (signer, log_dir)
    }

    /// Fails the run if any interval failed.
    pub(super) fn conclude(&self) {
        let failed: Vec<_> = self.failed.iter().filter(|(_, why)| !why.is_empty()).collect();
        println!("SOAK {} of {} intervals failed", failed.len(), self.failed.len());
        assert!(failed.is_empty(), "the soak failed: {failed:?}");
    }
}
