//! The archive's backlog: the whole ranges not yet confirmed, when each was first seen whole, and the
//! `archive_upload_lagging` episode their wait opens.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::time::Instant;
use tracing::{info, warn};

use crate::archive::whole_ranges;
use crate::log::Manifest;
use crate::metrics::Metrics;

/// The ranges waiting for the archive.
pub(super) struct Backlog {
    lag_alert: Duration,
    /// The first range not yet confirmed, or found in the bucket at a resume.
    pub(super) next: u64,
    /// The whole ranges counted so far.
    pub(super) whole: u64,
    /// Each count of whole ranges when it was first seen, oldest first: a range below an entry's count and at
    /// or above the one before it waits from that entry's time.
    seen: VecDeque<(u64, Instant)>,
    /// When the resume under way began: until it ends, the bucket's state is unknown.
    resuming: Option<Instant>,
    /// When the resume that just ended began, which the ranges whole at its end wait from.
    resumed_from: Option<Instant>,
    /// Whether `archive_upload_lagging` is raised.
    raised: bool,
}

impl Backlog {
    /// An empty backlog, warning once a range has waited `lag_alert`.
    pub(super) fn new(lag_alert: Duration) -> Self {
        Self { lag_alert, next: 0, whole: 0, seen: VecDeque::new(), resuming: None, resumed_from: None, raised: false }
    }

    /// Starts the clock of a resume, unless one already runs: a resume that has not reached the bucket for
    /// `lag_alert` raises the alert as a waiting range would.
    pub(super) fn resuming(&mut self) {
        self.resuming.get_or_insert_with(Instant::now);
    }

    /// Ends the resume at `next`, the first range the bucket lacks, keeping when each later range was first seen
    /// whole, so a new source carries an episode on rather than ending it; the ranges first seen whole next wait
    /// from the resume's start.
    pub(super) fn restart(&mut self, next: u64) {
        self.resumed_from = self.resuming.take();
        (self.next, self.whole) = (next, self.whole.max(next));
        self.seen.retain(|(count, _)| *count > next);
    }

    /// Counts the ranges `view` has made whole since the last call as waiting from now, or, at the first call after
    /// a resume, from the resume's start, and returns how many ranges `view` holds whole.
    /// Error cases:
    /// - the view's last listing cannot be read
    pub(super) fn observe(&mut self, view: &Manifest) -> anyhow::Result<u64> {
        let whole = whole_ranges(view)?;
        let since = self.resumed_from.take();
        if whole > self.whole {
            self.whole = whole;
            self.seen.push_back((whole, since.unwrap_or_else(Instant::now)));
        }
        Ok(whole)
    }

    /// Moves past the range at `next`, confirmed.
    pub(super) fn passed(&mut self) {
        self.next += 1;
    }

    /// Sets the backlog's metrics, and raises `archive_upload_lagging` once the oldest waiting range, or a resume,
    /// has waited `lag_alert`, or resolves it once none has.
    pub(super) fn check(&mut self, metrics: &Metrics) {
        while self.seen.front().is_some_and(|(count, _)| *count <= self.next) {
            self.seen.pop_front();
        }
        let pending = self.whole.saturating_sub(self.next);
        let oldest = self.seen.front().map(|(_, since)| *since).filter(|_| pending > 0);
        let waited = [self.resuming, oldest].into_iter().flatten().min().map(|since| since.elapsed());
        let archive = &metrics.archive;
        archive.confirmed_ranges.set(i64::try_from(self.next).unwrap_or(i64::MAX));
        archive.pending_ranges.set(i64::try_from(pending).unwrap_or(i64::MAX));
        archive.oldest_pending_seconds.set(waited.map_or(0.0, |waited| waited.as_secs_f64()));
        match waited {
            Some(waited) if waited >= self.lag_alert && !self.raised => {
                self.raised = true;
                let what = match pending {
                    0 => "the uploader has not reached the bucket to resume".to_owned(),
                    _ => format!("{pending} whole ranges of the log wait for the archive, the oldest"),
                };
                warn!(
                    alert = "archive_upload_lagging",
                    "{what} for {waited:?}: the host keeps the log and sequencing goes on; check the bucket, its keys, \
                     the node's route to it and the vault's finality"
                );
            }
            Some(waited) if waited >= self.lag_alert => {}
            _ if self.raised => {
                self.raised = false;
                info!(alert_resolved = "archive_upload_lagging", "the archive holds every range that waited");
            }
            _ => {}
        }
    }
}
