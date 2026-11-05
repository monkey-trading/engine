//! Dashboard statistics: the series, position maps and MONKEY activity the House and MONKEY pages
//! read, derived from the log and held in memory. The sequencer feeds them every command it applies,
//! and recovery feeds them every command its replay re-applies through the same `observe`, so a
//! restarted node serves the buckets it served before. Nothing is written to disk, so a host has no
//! file to roll back; a recovery from a checkpoint snapshot sees only the commands after it, and
//! every answer says where its data begins.
//!
//! The scan of every account (`snapshot`) is due at most once per `api.stats_interval` of sequenced
//! time, with a light one (totals only) every `api.stats_light_interval` between, so the same log gives the same snapshots whenever it is replayed. A running node takes it
//! on a worker thread that applies the same commands to a copy of the engine (`background`), so no
//! scan holds the sequencer; a replay, and a node whose worker stopped, scan on the replaying or
//! sequencing thread. Reads run on the sequencer's thread, between batches, and so see only commands
//! the log already holds durably.

mod activity;
pub(crate) mod background;
mod closeout;
mod distributions;
mod turns;
pub(crate) use turns::TurnsPage;
mod monkey;
mod record;
mod series;
mod snapshot;
mod view;
pub(crate) use closeout::{PositionCloseout, account_closeouts};
pub(crate) use distributions::DistributionsPage;
pub(crate) use monkey::MonkeyView;
pub(crate) use view::{ActivityPage, HouseFigures, PositionsView, SeriesPage, SeriesQuery};

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::Context;
use engine_core::command::Sequenced;
use engine_core::types::Millis;
use engine_core::{Engine, Fixed};

use crate::config::{NodeConfig, StatsSection};
use crate::stats::activity::Activity;
use crate::stats::distributions::Distributions;
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
    /// Between light scans: `api.stats_light_interval`, or `interval_ms` if that is shorter.
    light_interval_ms: Millis,
    activity_limit: usize,
    distribution_limit: usize,
    layout: Layout,
}

/// Everything recorded, behind the lock `Stats` takes.
pub(crate) struct Inner {
    series: SeriesStore,
    activity: Activity,
    /// Every distribution epoch closed since the record began, as far as `[stats]` keeps them.
    distributions: Distributions,
    /// The daily turns the log assigned, as far as the ring keeps them.
    turns: turns::TurnHistory,
    /// The latest scan of either kind: the totals every scan takes.
    snapshot: Option<Snapshot>,
    /// The latest full scan: margin, the position maps and the distribution excess.
    full: Option<Snapshot>,
    begin: Option<Begin>,
    next_scan: Option<Millis>,
    next_light: Option<Millis>,
    queue_total: Fixed,
    staked: u128,
    stakers: u64,
    distribution: Option<Distribution>,
    /// The worker that takes the scans, once the sequencer has started one.
    background: Option<background::Background>,
    /// What the worker is to do next, in order, until the batch that produced it is durable.
    staged: Vec<background::Job>,
    /// Set while a replay feeds the record, whose scans are light: only the last one is served.
    recovering: bool,
}

/// The dashboard statistics of one node. Clones share one record: the sequencer writes it as it
/// applies each command, and a view reads it from the stats worker's thread, so the lock is held
/// briefly on both sides. The record can be a batch ahead of the log and of the worker's copy, so its
/// views run on the sequencer, with the engine at the same seq, and wait for the log (`query_durable`).
#[derive(Clone)]
pub struct Stats {
    shared: Arc<Mutex<Inner>>,
    settings: Arc<Settings>,
}

impl Settings {
    /// The settings `cfg` gives.
    /// Error cases:
    /// - a retention, `api.stats_interval` or `api.stats_light_interval` is too long to count in
    ///   milliseconds
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
            light_interval_ms: StatsSection::millis(cfg.api.stats_light_interval.min(cfg.api.stats_interval))
                .context("api.stats_light_interval")?,
            activity_limit: section.activity_limit,
            distribution_limit: section.distribution_limit,
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
            distributions: Distributions::new(settings.distribution_limit),
            turns: turns::TurnHistory::default(),
            snapshot: None,
            full: None,
            begin: None,
            next_scan: None,
            next_light: None,
            queue_total: Fixed::ZERO,
            staked: 0,
            stakers: 0,
            distribution: None,
            background: None,
            staged: Vec::new(),
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

    /// The record, with the background scan installed if it has finished.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        let mut inner = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        inner.collect();
        inner
    }

    /// Moves the scans to a worker over a copy of `engine`, which `follow` then keeps in step. Call
    /// it once, after recovery, before the sequencer applies its first command. A copy that cannot
    /// be made leaves the scans on the sequencer's thread, with an alert.
    pub(crate) fn scan_in_background(&self, engine: &Engine) {
        match background::Background::start(engine) {
            Ok(worker) => self.lock().background = Some(worker),
            Err(error) => background::stopped(&format!("{error:#}")),
        }
    }

    /// Stages a command the sequencer applied for the worker, if there is one, before `observe` sees
    /// it. It goes to the worker with `deliver`, once the command is durable.
    pub(crate) fn follow(&self, sequenced: &Sequenced) {
        let mut inner = self.lock();
        if inner.background.is_some() {
            inner.staged.push(background::Job::Apply(Box::new(sequenced.clone())));
        }
    }

    /// Takes what `follow` and the due scans staged: the jobs of a batch to deliver when it is durable.
    pub(crate) fn stage(&self) -> Vec<background::Job> {
        std::mem::take(&mut self.lock().staged)
    }

    /// Hands the worker the jobs of a durable batch, in order. A worker that has stopped leaves the
    /// scans on the sequencer's thread, with an alert.
    pub(crate) fn deliver(&self, jobs: Vec<background::Job>) {
        if jobs.is_empty() {
            return;
        }
        // Not under the lock: a full queue makes this wait for the worker, and a read it is running
        // needs the lock.
        let Some(worker) = self.reader() else { return };
        if !jobs.into_iter().all(|job| worker.send(job)) {
            self.lock().background = None;
            background::stopped("the worker's queue closed");
        }
    }

    /// Commands the worker has been handed and not applied, for the benchmark.
    #[cfg(test)]
    pub(crate) fn behind(&self) -> u64 {
        self.lock().background.as_ref().map_or(0, background::Background::behind)
    }

    /// A handle that runs reads on the worker's copy of the engine, which holds the commands
    /// delivered so far; `None` when no worker runs, and the read goes to the sequencer.
    pub(crate) fn reader(&self) -> Option<background::Reader> {
        self.lock().background.as_ref().map(background::Background::reader)
    }

    /// Waits for the scans in flight and installs them. For tests.
    #[cfg(test)]
    pub(crate) fn settle(&self) {
        self.deliver(self.stage());
        let mut inner = self.lock();
        while let Some(worker) = inner.background.as_mut().filter(|worker| worker.in_flight() > 0) {
            match worker.wait() {
                Ok(snapshot) => inner.install(*snapshot),
                Err(reason) => panic!("the background scan failed: {reason}"),
            }
        }
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
        if inner.full.is_none()
            && let Some(seq) = engine.last_seq()
        {
            let snapshot = snapshot::scan(engine, seq, engine.now(), self.settings.layout, true);
            inner.serve(snapshot);
        }
    }
}

impl Stats {
    /// Every series bucket and the activity ids held, for comparing two records.
    #[cfg(test)]
    pub(crate) fn dump(&self) -> String {
        let inner = self.lock();
        let (activity, _) = inner.activity.page(None, usize::MAX);
        format!(
            "{:?}\n{:?}\n{:?}\n{:?}",
            inner.series.rows(),
            activity,
            inner.distributions.page(None, usize::MAX),
            inner.begin
        )
    }
}

#[cfg(test)]
mod bench;
#[cfg(test)]
pub(crate) mod tests;
