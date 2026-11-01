//! The `[stats]` section: how long the dashboard series stay in memory and how the position
//! aggregates are bucketed, with the validation that keeps a setting from failing silently.

use std::time::Duration;

use anyhow::{Context, ensure};
use engine_core::types::Millis;
use serde::Deserialize;

/// Dashboard statistics a node derives from the log and serves at `/v1/stats/series`,
/// `/v1/house/positions` and `/v1/monkey/activity`. Everything is held in memory and rebuilt at
/// recovery; nothing is written to disk.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatsSection {
    /// How long 1m buckets stay, counted back from the newest command. 48 hours by default.
    #[serde(default = "default_retention_1m", with = "humantime_serde")]
    pub retention_1m: Duration,
    /// How long 1h buckets stay. 90 days by default.
    #[serde(default = "default_retention_1h", with = "humantime_serde")]
    pub retention_1h: Duration,
    /// How long 1d buckets stay; absent keeps them unbounded.
    #[serde(default, with = "humantime_serde::option")]
    pub retention_1d: Option<Duration>,
    /// Most MONKEY events (allocations, stakes, unstakes) the activity feed keeps, newest first.
    #[serde(default = "default_activity_limit")]
    pub activity_limit: usize,
    /// How far from a market's mark the close-out map reaches, in basis points each way.
    #[serde(default = "default_closeout_range_bps")]
    pub closeout_range_bps: u32,
    /// The width of a close-out price bucket, in basis points of the mark.
    #[serde(default = "default_closeout_bucket_bps")]
    pub closeout_bucket_bps: u32,
    /// How far the profit buckets reach from a position's entry, in basis points each way; a
    /// position beyond falls in the open bucket at that end.
    #[serde(default = "default_profit_range_bps")]
    pub profit_range_bps: u32,
    /// The width of a profit bucket, in basis points of the entry price.
    #[serde(default = "default_profit_bucket_bps")]
    pub profit_bucket_bps: u32,
}

/// One second, the least `api.stats_interval`.
pub(super) const MIN_STATS_INTERVAL: Duration = Duration::from_secs(1);
/// The most buckets either position map may have, so a setting cannot make a snapshot unbounded.
pub(crate) const MAX_BUCKETS: u32 = 400;
/// The hours of hourly volume buckets `GET /v1/house` sums.
pub(crate) const VOLUME_WINDOW_HOURS: u64 = 24;
/// The farthest, in basis points, a position map's buckets may reach from its centre.
pub(crate) const MAX_COVERED_BPS: u64 = 1_000_000;
/// The most activity entries the feed may keep.
pub(crate) const MAX_ACTIVITY: usize = 100_000;

impl Default for StatsSection {
    fn default() -> Self {
        Self {
            retention_1m: default_retention_1m(),
            retention_1h: default_retention_1h(),
            retention_1d: None,
            activity_limit: default_activity_limit(),
            closeout_range_bps: default_closeout_range_bps(),
            closeout_bucket_bps: default_closeout_bucket_bps(),
            profit_range_bps: default_profit_range_bps(),
            profit_bucket_bps: default_profit_bucket_bps(),
        }
    }
}

impl StatsSection {
    /// A retention in milliseconds.
    /// Error cases:
    /// - it is too long to count in milliseconds
    pub fn millis(retention: Duration) -> anyhow::Result<Millis> {
        Millis::try_from(retention.as_millis()).context("too long to count in milliseconds")
    }

    /// Checks the retentions are positive and countable, and the bucket settings give a bounded,
    /// non-empty map.
    /// Error cases:
    /// - a retention is zero or too long to count in milliseconds
    /// - `activity_limit` is zero or over 100000
    /// - a range or a bucket width is zero, a bucket is wider than its range, or a range holds more
    ///   than 400 buckets each way
    pub fn validate(&self) -> anyhow::Result<()> {
        let retentions = [("retention_1m", Some(self.retention_1m)), ("retention_1h", Some(self.retention_1h))];
        for (name, retention) in retentions.into_iter().chain([("retention_1d", self.retention_1d)]) {
            if let Some(retention) = retention {
                let ms = Self::millis(retention).with_context(|| format!("stats.{name}"))?;
                ensure!(ms > 0, "stats.{name} must be at least one millisecond");
            }
        }
        ensure!(
            self.retention_1h >= Duration::from_secs(VOLUME_WINDOW_HOURS * 3_600),
            "stats.retention_1h must cover the {VOLUME_WINDOW_HOURS} hours of volume `GET /v1/house` sums"
        );
        ensure!((1..=MAX_ACTIVITY).contains(&self.activity_limit), "stats.activity_limit must be 1 to {MAX_ACTIVITY}");
        for (name, range, bucket) in [
            ("closeout", self.closeout_range_bps, self.closeout_bucket_bps),
            ("profit", self.profit_range_bps, self.profit_bucket_bps),
        ] {
            ensure!(range > 0 && bucket > 0, "stats.{name}_range_bps and stats.{name}_bucket_bps must be positive");
            ensure!(bucket <= range, "stats.{name}_bucket_bps must not exceed stats.{name}_range_bps");
            ensure!(
                range.div_ceil(bucket) <= MAX_BUCKETS,
                "stats.{name}_range_bps holds more than {MAX_BUCKETS} buckets of stats.{name}_bucket_bps"
            );
            // The buckets cover a whole number of widths, which may reach past the range asked for.
            let covered = u64::from(range.div_ceil(bucket)) * u64::from(bucket);
            ensure!(
                covered <= MAX_COVERED_BPS,
                "stats.{name}_bucket_bps rounds stats.{name}_range_bps up to {covered}, past {MAX_COVERED_BPS}"
            );
        }
        let covered =
            u64::from(self.closeout_range_bps.div_ceil(self.closeout_bucket_bps)) * u64::from(self.closeout_bucket_bps);
        ensure!(covered < 10_000, "stats.closeout_range_bps rounds up to {covered} with its bucket, a fall to zero");
        Ok(())
    }
}

/// Two days of 1m buckets.
fn default_retention_1m() -> Duration {
    Duration::from_secs(48 * 60 * 60)
}

/// Ninety days of 1h buckets.
fn default_retention_1h() -> Duration {
    Duration::from_secs(90 * 24 * 60 * 60)
}

fn default_activity_limit() -> usize {
    5_000
}

/// Twenty percent each way.
fn default_closeout_range_bps() -> u32 {
    2_000
}

/// One percent.
fn default_closeout_bucket_bps() -> u32 {
    100
}

/// Fifty percent each way.
fn default_profit_range_bps() -> u32 {
    5_000
}

/// Five percent.
fn default_profit_bucket_bps() -> u32 {
    500
}

/// Sixty seconds, the default `api.stats_interval`.
pub(super) fn default_stats_interval() -> Duration {
    Duration::from_secs(60)
}
