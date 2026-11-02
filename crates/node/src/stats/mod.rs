//! Dashboard statistics: the series, position maps and MONKEY activity the House and MONKEY pages
//! read, derived from the log and held in memory. The sequencer feeds them every command it applies,
//! and recovery feeds them every command its replay re-applies through the same `observe`, so a
//! restarted node serves the buckets it served before. Nothing is written to disk, so a host has no
//! file to roll back; a recovery from a checkpoint snapshot sees only the commands after it, and
//! every answer says where its data begins.
//!
//! The scan of every account (`snapshot`) runs on the sequencer's thread, at most once per
//! `api.stats_interval` of sequenced time, so the same log gives the same snapshots whenever it is
//! replayed. Reads run on that thread too, between batches, and so see only commands the log
//! already holds durably.

mod activity;
mod closeout;
mod monkey;
mod record;
mod series;
mod snapshot;
mod view;
pub(crate) use monkey::MonkeyView;
pub(crate) use view::{ActivityPage, HouseFigures, PositionsView, SeriesPage, SeriesQuery};

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::Context;
use engine_core::types::Millis;
use engine_core::{Engine, Fixed};

use crate::config::{NodeConfig, StatsSection};
use crate::stats::activity::Activity;
use crate::stats::series::SeriesStore;
use crate::stats::snapshot::{Grid, Layout, Snapshot};

/// The seq and time of the first command recorded.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Begin {
    pub ts: Millis,
    pub seq: u64,
}

/// The last distribution epoch to close, as its event said.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Distribution {
    pub epoch: u64,
    /// When the epoch ended.
    pub closed_ms: Millis,
    pub nav: Fixed,
    pub floor: Fixed,
    /// Whole USDC base units above the floor.
    pub excess: u128,
    pub allocated: u128,
}

/// What `[stats]` and `api.stats_interval` set.
pub(crate) struct Settings {
    retention: [Option<Millis>; 3],
    interval_ms: Millis,
    activity_limit: usize,
    layout: Layout,
}

/// Everything recorded, behind the lock `Stats` takes.
pub(crate) struct Inner {
    series: SeriesStore,
    activity: Activity,
    snapshot: Option<Snapshot>,
    begin: Option<Begin>,
    next_scan: Option<Millis>,
    queue_total: Fixed,
    staked: u128,
    stakers: u64,
    distribution: Option<Distribution>,
    /// Set while a replay feeds the record, whose scans are light: only the last one is served.
    recovering: bool,
}

/// The dashboard statistics of one node. Clones share one record: the sequencer writes it, and a
/// read runs on the sequencer's thread, so the lock is never contended; it stands for the thread
/// boundary a recovery's replay and the sequencer hand the record across.
#[derive(Clone)]
pub struct Stats {
    shared: Arc<Mutex<Inner>>,
    settings: Arc<Settings>,
}

impl Settings {
    /// The settings `cfg` gives.
    /// Error cases:
    /// - a retention or `api.stats_interval` is too long to count in milliseconds
    fn new(cfg: &NodeConfig) -> anyhow::Result<Self> {
        let section: &StatsSection = &cfg.stats;
        let retention_1d = section.retention_1d.map(StatsSection::millis).transpose().context("stats.retention_1d")?;
        Ok(Self {
            retention: [
                Some(StatsSection::millis(section.retention_1m).context("stats.retention_1m")?),
                Some(StatsSection::millis(section.retention_1h).context("stats.retention_1h")?),
                retention_1d,
            ],
            interval_ms: StatsSection::millis(cfg.api.stats_interval).context("api.stats_interval")?,
            activity_limit: section.activity_limit,
            layout: Layout {
                closeout: Grid::new(section.closeout_range_bps, section.closeout_bucket_bps),
                profit: Grid::new(section.profit_range_bps, section.profit_bucket_bps),
            },
        })
    }
}

impl Inner {
    fn new(settings: &Settings) -> Self {
        Self {
            series: SeriesStore::new(settings.retention),
            activity: Activity::new(settings.activity_limit),
            snapshot: None,
            begin: None,
            next_scan: None,
            queue_total: Fixed::ZERO,
            staked: 0,
            stakers: 0,
            distribution: None,
            recovering: false,
        }
    }
}

impl Stats {
    /// An empty record under `cfg`'s settings.
    /// Error cases:
    /// - a retention or `api.stats_interval` is too long to count in milliseconds
    pub fn new(cfg: &NodeConfig) -> anyhow::Result<Self> {
        let settings = Settings::new(cfg)?;
        Ok(Self { shared: Arc::new(Mutex::new(Inner::new(&settings))), settings: Arc::new(settings) })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets everything recorded: a replay starts over, so a recovery attempt that failed part
    /// way leaves nothing behind.
    pub(crate) fn restart(&self) {
        let mut fresh = Inner::new(&self.settings);
        fresh.recovering = true;
        *self.lock() = fresh;
    }

    /// Ends a recovery: takes the totals the first command would have, when no command was replayed,
    /// and a full scan for the answers that read one, without recording it in a series, so a
    /// restart's series stay those the log gives.
    pub(crate) fn finish_recovery(&self, engine: &Engine) {
        let mut inner = self.lock();
        inner.recovering = false;
        if inner.begin.is_none() {
            inner.refresh_queue(engine);
            inner.refresh_stakers(engine);
        }
        if inner.snapshot.as_ref().is_none_or(|snapshot| !snapshot.full)
            && let Some(seq) = engine.last_seq()
        {
            inner.snapshot = Some(snapshot::scan(engine, seq, engine.now(), self.settings.layout, true));
        }
    }
}

impl Stats {
    /// Every series bucket and the activity ids held, for comparing two records.
    #[cfg(test)]
    pub(crate) fn dump(&self) -> String {
        let inner = self.lock();
        let (activity, _) = inner.activity.page(None, usize::MAX);
        format!("{:?}\n{:?}\n{:?}", inner.series.rows(), activity, inner.begin)
    }
}

#[cfg(test)]
mod bench;
#[cfg(test)]
pub(crate) mod tests;
