//! Dashboard statistics: the series, position maps and MONKEY activity the House and MONKEY pages
//! read, derived from the log and held in memory. The sequencer feeds them every command it applies,
//! and recovery feeds them every command its replay re-applies through the same `observe`, so a
//! restarted node serves the buckets it served before. Nothing is written to disk, so a host has no
//! file to roll back; a recovery from a checkpoint snapshot sees only the commands after it, and
//! every answer says where its data begins.
//!
//! The scan of every account (`snapshot`) is due at most once per `api.stats_interval` of sequenced
//! time, with a light one (totals only) every `api.stats_light_interval` between, so the same log
//! gives the same snapshots whenever it is replayed. A running node takes it on a worker thread
//! that applies the same commands to a copy of the engine (`background`), so no scan holds the
//! sequencer, as does a member's replica once built, so its start copies nothing; a replay, and a
//! node whose worker stopped, scan on the replaying or sequencing thread. Reads run on the
//! sequencer's thread, between batches, and may see a batch the log has not yet synced; each answer
//! waits until the log holds what it read (`ApiState::query_durable`).

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
pub(crate) use closeout::{AccountStanding, PositionCloseout, account_closeouts};
pub(crate) use distributions::DistributionsPage;
pub(crate) use monkey::MonkeyView;
pub(crate) use view::{ActivityPage, HouseFigures, PositionsView, SeriesPage, SeriesQuery};

use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::Context;
use engine_core::command::Sequenced;
use engine_core::eip712::B256;
use engine_core::types::Millis;
use engine_core::{Engine, Fixed};

use crate::config::{NodeConfig, StatsSection};
use crate::log::{CommandEntry, LogEntry};
use crate::sequencer::replay::Fold;
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
    /// Encode jobs a test holds back, their replies unanswered, while set (`hold_encodes`).
    #[cfg(test)]
    held_encodes: Option<Vec<background::Job>>,
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
            #[cfg(test)]
            held_encodes: None,
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
    /// it once, after recovery, before the sequencer applies its first command, or once a member's
    /// replica is built, before it applies another. A copy that cannot be made leaves the scans on the
    /// caller's thread, with an alert. A worker already running, one a member's replica kept in step
    /// with `engine` as it followed, is kept: no copy is made.
    pub(crate) fn scan_in_background(&self, engine: &Engine) {
        if self.lock().background.is_some() {
            return;
        }
        match background::Background::start(engine) {
            Ok(worker) => self.lock().background = Some(worker),
            Err(error) => background::stopped(&format!("{error:#}")),
        }
    }

    /// Stages a command the sequencer applied for the worker, if there is one, before `observe` sees
    /// it. It goes to the worker with `deliver`, once the command is durable.
    pub(crate) fn follow(&self, sequenced: &Sequenced) {
        self.stage_job(background::Job::Apply(Box::new(sequenced.clone()), None));
    }

    /// `follow`, for a command the sequencer logged as `entry` with the cumulative command prefix `link`,
    /// which the worker's fold takes (`seed_fold`).
    pub(crate) fn follow_logged(&self, entry: &CommandEntry, link: B256) {
        let logged = (link, entry.seal.as_ref().map(|seal| seal.signer));
        self.stage_job(background::Job::Apply(Box::new(entry.sequenced.clone()), Some(logged)));
    }

    /// Stages a withdrawal or checkpoint entry the sequencer logged for the worker's fold.
    pub(crate) fn follow_entry(&self, entry: &LogEntry) {
        if !matches!(entry, LogEntry::Command(_)) {
            self.stage_job(background::Job::Entry(Box::new(entry.clone())));
        }
    }

    /// Has the worker fold the replay state from `fold`, the state after every command and entry staged so far,
    /// so a daily snapshot can carry it (`carry_job`).
    pub(crate) fn seed_fold(&self, fold: Fold) {
        self.stage_job(background::Job::Seed(Box::new(fold)));
    }

    /// A job that encodes the replay state the worker folds at the checkpoint after the command at `seq`, the
    /// last entry staged, and the end its encoding arrives on; `None` when no worker runs.
    pub(crate) fn carry_job(&self, seq: u64) -> Option<(background::Job, Receiver<background::Encoded>)> {
        self.lock().background.as_ref()?;
        let (reply, encoded) = sync_channel(1);
        Some((background::Job::Carry { seq, reply }, encoded))
    }

    /// A job that encodes the worker's copy at the command at `seq`, stamped `ts`, the last one
    /// `follow` staged, for a recovery cache at the checkpoint after it, and the end its encoding
    /// arrives on. `stage_job` stages it; `None` when no worker runs, and the caller encodes the
    /// engine itself.
    pub(crate) fn encode_job(&self, seq: u64, ts: Millis) -> Option<(background::Job, Receiver<background::Encoded>)> {
        self.lock().background.as_ref()?;
        // One slot: the worker never waits on the reader, which may already have given up.
        let (reply, encoded) = sync_channel(1);
        Some((background::Job::Encode { seq, ts, reply }, encoded))
    }

    /// Stages `job` behind what `follow` staged, for the worker with the batch once durable. With no
    /// worker it is dropped, and so is any reply it carries.
    pub(crate) fn stage_job(&self, job: background::Job) {
        let mut inner = self.lock();
        #[cfg(test)]
        if matches!(job, background::Job::Encode { .. })
            && let Some(held) = &mut inner.held_encodes
        {
            held.push(job);
            return;
        }
        if inner.background.is_some() {
            inner.staged.push(job);
        }
    }

    /// Holds back every encode job staged from now on, its reply unanswered, as a worker that never
    /// reaches it would.
    #[cfg(test)]
    pub(crate) fn hold_encodes(&self) {
        self.lock().held_encodes = Some(Vec::new());
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
                Ok((snapshot, sampled)) => inner.take(*snapshot, sampled),
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
    /// restart's series stay those the log gives. With a worker running, as a member's replica keeps
    /// one, the scan is the worker's, delivered with the first durable batch, so the start does not
    /// wait for it.
    pub(crate) fn finish_recovery(&self, engine: &Engine) {
        let mut inner = self.lock();
        inner.recovering = false;
        if inner.begin.is_none() {
            inner.refresh_queue(engine);
            inner.refresh_stakers(engine);
        }
        let Some(seq) = engine.last_seq().filter(|_| inner.full.is_none()) else { return };
        match inner.background.as_mut() {
            Some(worker) => {
                let job = worker.scan_job(seq, engine.now(), self.settings.layout, (true, false));
                inner.staged.push(job);
            }
            None => {
                let snapshot = snapshot::scan(engine, seq, engine.now(), self.settings.layout, true);
                inner.serve(snapshot);
            }
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
mod launch_bench;
#[cfg(test)]
pub(crate) mod tests;
