//! `[alerts.storage]`: when the node pages about a data volume filling.

use std::time::Duration;

use anyhow::ensure;
use serde::Deserialize;

/// The most `rate_window` may be, so a projection never rests on samples a host-clock jump could have made up.
const MAX_RATE_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);

/// The thresholds of the node's disk alerts. A volume is Warning or Critical when its free share, or its projected
/// time to fill at the growth the last `rate_window` shows, passes the level's threshold. The figures come from the
/// host's disk and clock, so they are operator alerts and no protocol input.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct StorageSection {
    /// How often store sizes and volume free space are read; one read is a walk of each store's directory.
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// Warning while a volume's free space is below this share of it (15% of a 1.92 TB drive is about 290 GB, days
    /// of the history index at ten times launch flow).
    pub warn_free_percent: u8,
    /// Critical while a volume's free space is below this share of it (5% is about 96 GB, a day or more at ten times
    /// launch flow, and what a full disk's outbox and log syncs still need to commit).
    pub critical_free_percent: u8,
    /// Warning while the volume is projected to fill within this long (a fortnight is time to order a drive or move
    /// a store).
    #[serde(with = "humantime_serde")]
    pub warn_fill_within: Duration,
    /// Critical while the volume is projected to fill within this long (three days outlasts a weekend).
    #[serde(with = "humantime_serde")]
    pub critical_fill_within: Duration,
    /// The span of growth the projection reads, made once the whole span is sampled, so a burst in its first hours,
    /// such as an index catching up after a start, is not read as a steady rate (six hours smooths that and a period
    /// close, and shows ten times launch flow within the window).
    #[serde(with = "humantime_serde")]
    pub rate_window: Duration,
}

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            warn_free_percent: 15,
            critical_free_percent: 5,
            warn_fill_within: Duration::from_secs(14 * 24 * 3600),
            critical_fill_within: Duration::from_secs(3 * 24 * 3600),
            rate_window: Duration::from_secs(6 * 3600),
        }
    }
}

impl StorageSection {
    /// Checks the critical level is the tighter at each measure and the windows are usable.
    /// Error cases:
    /// - a free share is zero, or the critical share is not below the warning share, or the warning share is over 100
    /// - the critical fill time is not below the warning one
    /// - `interval` is under a second, or `rate_window` is shorter than six intervals or over a week
    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            0 < self.critical_free_percent
                && self.critical_free_percent < self.warn_free_percent
                && self.warn_free_percent <= 100,
            "alerts.storage needs 0 < critical_free_percent < warn_free_percent <= 100"
        );
        ensure!(
            !self.critical_fill_within.is_zero() && self.critical_fill_within < self.warn_fill_within,
            "alerts.storage needs 0 < critical_fill_within < warn_fill_within"
        );
        ensure!(self.interval >= Duration::from_secs(1), "alerts.storage.interval must be at least 1s");
        ensure!(
            self.rate_window >= self.interval * 6 && self.rate_window <= MAX_RATE_WINDOW,
            "alerts.storage.rate_window must be at least six intervals and at most 7d"
        );
        Ok(())
    }
}
