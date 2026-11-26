//! What the window reports of its passes: the alerts a pass raises or resolves, and the gauges of the log on the host.

use std::collections::BTreeSet;

use engine_core::types::Millis;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::archive::window::{FAILING_ALERT, HELD_ALERT, Held, Window, millis};
use crate::log::{Manifest, SegmentInfo};

impl Window {
    /// Counts a failed pass, and raises `log_window_failing` once passes have failed for `FAILING_ALERT`.
    pub(super) fn failed(&mut self, e: &anyhow::Error) {
        self.metrics.archive.window.failures.inc();
        let since = *self.failing.get_or_insert_with(Instant::now);
        if self.raised.failing != Some(true) && since.elapsed() >= FAILING_ALERT {
            self.raised.failing = Some(true);
            warn!(
                alert = "log_window_failing",
                "the local window's passes over the log's segments have failed for {:?}, so no segment leaves the \
                 host: {e:#}",
                since.elapsed()
            );
        } else {
            warn!("the local window's pass over the log's segments failed, and runs again: {e:#}");
        }
    }

    /// Raises `log_window_held` once a reader or the bucket's read-back holds a segment `past` the window by
    /// `HELD_ALERT`, and resolves it once none does.
    pub(super) fn report_held(&mut self, segment: &SegmentInfo, held: Held, past: Millis) {
        let past = past.max(0);
        self.metrics.archive.window.held_seconds.set(past as f64 / 1000.0);
        let over = past >= millis(HELD_ALERT) && matches!(held, Held::Reader | Held::ReadBack);
        match (over, self.raised.held) {
            (true, Some(false) | None) => {
                self.raised.held = Some(true);
                warn!(
                    alert = "log_window_held",
                    "segment {} has been past archive.local_window for {}s of sequenced time, and stays on the host \
                     because {}",
                    segment.file,
                    past / 1000,
                    held.reason()
                );
            }
            (false, Some(true) | None) => {
                self.raised.held = Some(false);
                info!(alert_resolved = "log_window_held", "no reader or read-back holds the log past the window");
            }
            _ => {}
        }
    }

    /// Sets the bytes and the first seq of the segments on the host, `local`, as `view` lists them.
    /// Error cases:
    /// - a listing cannot be read
    pub(super) fn measure(&self, view: &Manifest, local: &BTreeSet<u64>) -> anyhow::Result<()> {
        let mut bytes = view.active.as_ref().map_or(0, |active| active.info.bytes);
        if let Some((first, _)) = local.first().map(|first| view.containing(*first)).transpose()?.flatten() {
            for ordinal in first..view.completed_len() {
                let listed = view.completed_at(ordinal)?;
                if local.contains(&listed.first_seq) {
                    bytes = bytes.saturating_add(listed.bytes);
                }
            }
        }
        let window = &self.metrics.archive.window;
        window.local_bytes.set(i64::try_from(bytes).unwrap_or(i64::MAX));
        window.local_first_seq.set(local.first().map_or(0, |seq| i64::try_from(*seq).unwrap_or(i64::MAX)));
        Ok(())
    }
}
