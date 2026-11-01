//! The derived marks cache: each market's index-mark history, kept as per-resolution OHLC buckets
//! in SQLite beside the account history index and served signed at `/v1/markets/{id}/marks`. The
//! cache is rebuildable state, never sequenced: recovery seeds it from the log's replay (the same
//! derivation as the live index), live commands keep it current, and an optional provider backfill
//! seeds the depth a fresh log cannot have. Every bucket says whether it is log-derived or
//! provider-seeded, so a client can weigh seeded history against what the public log proves. The
//! buckets are chained: each row names the bucket below it under its authentication tag, and
//! per-pair chain heads committed in memory at recovery re-derivation anchor the index, so a host
//! that deletes, reorders, or rolls the cache back mid-run is refused at serve time — the read
//! over the broken span fails before anything is signed.

mod backfill;
pub(crate) mod run;
pub(crate) mod store;

pub(crate) use backfill::{Source, sources};
pub use store::Reader;
pub(crate) use store::Store;

use anyhow::Context;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use engine_core::Fixed;
use engine_core::types::{MarketId, Millis};
use serde::Serialize;
use tracing::warn;

use crate::config::{MarksSection, default_marks_retention_1h, default_marks_retention_1m, default_marks_retention_1s};

/// One mark a command left: the market, the sequenced time, and the index mark it held once the
/// command applied.
pub(crate) type MarkSample = (MarketId, Millis, Fixed);
/// The sink recovery's replay hands each changed mark to, to seed the cache.
pub(crate) type MarkSink = Box<dyn FnMut(MarketId, Millis, Fixed)>;

/// A bucket width the cache keeps, each with its own retention. The storage tag is the column
/// value, so new resolutions are only ever appended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Resolution {
    S1 = 0,
    M1 = 1,
    H1 = 2,
    D1 = 3,
}

impl Resolution {
    pub const ALL: [Resolution; 4] = [Resolution::S1, Resolution::M1, Resolution::H1, Resolution::D1];
    /// The bucket width in milliseconds.
    pub const fn ms(self) -> Millis {
        match self {
            Resolution::S1 => 1_000,
            Resolution::M1 => 60_000,
            Resolution::H1 => 3_600_000,
            Resolution::D1 => 86_400_000,
        }
    }
    /// The bucket `ts` falls in: the window start, which orders buckets in time.
    pub fn bucket(self, ts: Millis) -> Millis {
        ts - ts.rem_euclid(self.ms())
    }
    /// The wire name the endpoint accepts and serves.
    pub const fn as_str(self) -> &'static str {
        match self {
            Resolution::S1 => "1s",
            Resolution::M1 => "1m",
            Resolution::H1 => "1h",
            Resolution::D1 => "1d",
        }
    }
    /// The storage tag's resolution.
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            0 => Some(Resolution::S1),
            1 => Some(Resolution::M1),
            2 => Some(Resolution::H1),
            3 => Some(Resolution::D1),
            _ => None,
        }
    }

    /// The wire name's resolution.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "1s" => Some(Resolution::S1),
            "1m" => Some(Resolution::M1),
            "1h" => Some(Resolution::H1),
            "1d" => Some(Resolution::D1),
            _ => None,
        }
    }
}

/// Where a bucket's prices come from: derived from the sequenced log, or seeded from a provider's
/// historical candles. A bucket a log mark has touched is log-derived, whatever seeded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    LogDerived,
    SeededFromProvider,
}

impl Provenance {
    /// The value the store keeps.
    pub const fn as_str(self) -> &'static str {
        match self {
            Provenance::LogDerived => "log_derived",
            Provenance::SeededFromProvider => "seeded_from_provider",
        }
    }
    /// The stored value's provenance.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "log_derived" => Some(Provenance::LogDerived),
            "seeded_from_provider" => Some(Provenance::SeededFromProvider),
            _ => None,
        }
    }
}

/// One bucket of a market's marks, as the endpoint serves it: the window start and its OHLC.
#[derive(Debug, PartialEq, Serialize)]
pub struct MarkEntry {
    /// The bucket's window start, in milliseconds since the epoch.
    pub ts: Millis,
    pub open: Fixed,
    pub high: Fixed,
    pub low: Fixed,
    pub close: Fixed,
    /// Whether this bucket is log-derived or provider-seeded.
    pub provenance: Provenance,
}

/// One page of a market's marks for one resolution, oldest first.
#[derive(Debug, PartialEq, Serialize)]
pub struct MarksPage {
    pub market: MarketId,
    pub resolution: &'static str,
    pub entries: Vec<MarkEntry>,
    /// The `from_ms` for the next, newer page; `None` when the window is exhausted.
    pub next: Option<Millis>,
    /// The most entries one response ever carries.
    pub max_limit: usize,
}

/// How long each resolution's buckets stay, counted back from the newest mark written; `None`
/// keeps a resolution unbounded.
#[derive(Debug, Clone, Copy)]
pub struct Retentions {
    pub s1: Millis,
    pub m1: Millis,
    pub h1: Millis,
    pub d1: Option<Millis>,
}

impl Retentions {
    /// The policy the config's `[history.marks]` defaults describe: 1s for 48h, 1m for 60d, 1h for
    /// 2y, 1d unbounded.
    pub fn defaults() -> Self {
        let millis = |d: Duration| Millis::try_from(d.as_millis()).expect("default retentions fit milliseconds");
        Retentions {
            s1: millis(default_marks_retention_1s()),
            m1: millis(default_marks_retention_1m()),
            h1: millis(default_marks_retention_1h()),
            d1: None,
        }
    }

    /// The policy `section` gives, or the defaults without a section.
    /// Error cases:
    /// - a configured retention is too long to count in milliseconds
    pub fn of(section: Option<&MarksSection>) -> anyhow::Result<Self> {
        let Some(section) = section else { return Ok(Self::defaults()) };
        let millis = |d: Duration| Millis::try_from(d.as_millis()).context("too long to count in milliseconds");
        Ok(Retentions {
            s1: millis(section.retention_1s)?,
            m1: millis(section.retention_1m)?,
            h1: millis(section.retention_1h)?,
            d1: section.retention_1d_ms()?,
        })
    }

    /// This policy's retention for `resolution`.
    pub fn of_resolution(self, resolution: Resolution) -> Option<Millis> {
        match resolution {
            Resolution::S1 => Some(self.s1),
            Resolution::M1 => Some(self.m1),
            Resolution::H1 => Some(self.h1),
            Resolution::D1 => self.d1,
        }
    }
}

/// Buffers the mark samples recovery's replay takes and flushes them to the cache in batches, so
/// seeding a long log costs a transaction per thousand samples, not per sample. The first sample
/// resets the cache's log-derived content, so recovery re-derives exactly what the verified log
/// proves and nothing a host wrote into those rows while the node was stopped. A write failure
/// alerts and sets `failed`, which recovery treats as a refused boot: the cache must never serve
/// partially seeded history as if it were complete.
pub(crate) struct Seeder {
    store: Rc<RefCell<Store>>,
    buffered: Vec<MarkSample>,
    reset: bool,
    failed: Rc<Cell<bool>>,
    /// Set once the log-derived content is reset, so a recovery with zero mark samples still
    /// resets through the caller.
    reset_done: Rc<Cell<bool>>,
}

/// How many samples `Seeder` buffers before one flush.
const BATCH: usize = 1024;

impl Seeder {
    /// A seeder writing `store`, which recovery replays into, reporting failure through `failed`
    /// and reset completion through `reset_done`.
    pub fn new(store: Rc<RefCell<Store>>, failed: Rc<Cell<bool>>, reset_done: Rc<Cell<bool>>) -> Self {
        Self { store, buffered: Vec::new(), reset: false, failed, reset_done }
    }

    /// Resets the log-derived content on the first sample, then buffers it, flushing at `BATCH`.
    pub fn sample(&mut self, sample: MarkSample) {
        if !self.reset {
            self.reset = true;
            self.reset_done.set(true);
            if let Err(e) = self.store.borrow_mut().reset_log_derived() {
                warn!(alert = "marks_seed_failed", "resetting the marks cache for re-derivation: {e:#}");
                self.failed.set(true);
            }
        }
        self.buffered.push(sample);
        if self.buffered.len() >= BATCH {
            self.flush();
        }
    }

    /// Writes the buffered samples, one transaction, and clears them however it went.
    fn flush(&mut self) {
        if self.buffered.is_empty() {
            return;
        }
        if let Err(e) = self.store.borrow_mut().observe(&self.buffered) {
            warn!(alert = "marks_seed_failed", "{e:#}");
            self.failed.set(true);
        }
        self.buffered.clear();
    }
}

impl Drop for Seeder {
    fn drop(&mut self) {
        self.flush();
    }
}

#[cfg(test)]
mod task_tests;
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_their_windows_start() {
        assert_eq!(Resolution::S1.bucket(1_792_489_875_714), 1_792_489_875_000);
        assert_eq!(Resolution::M1.bucket(1_792_489_875_714), 1_792_489_860_000);
        assert_eq!(Resolution::H1.bucket(1_792_489_875_714), 1_792_486_800_000);
        assert_eq!(Resolution::D1.bucket(1_792_489_875_714), 1_792_454_400_000);
        // A whole-window timestamp is its own bucket, and negative times floor, not truncate.
        assert_eq!(Resolution::M1.bucket(1_792_489_860_000), 1_792_489_860_000);
        assert_eq!(Resolution::S1.bucket(-1), -1_000);
    }

    #[test]
    fn wire_names_round_trip() {
        for resolution in Resolution::ALL {
            assert_eq!(Resolution::parse(resolution.as_str()), Some(resolution));
        }
        assert_eq!(Resolution::parse("5m"), None);
        assert_eq!(Provenance::parse(Provenance::LogDerived.as_str()), Some(Provenance::LogDerived));
        assert_eq!(Provenance::parse(Provenance::SeededFromProvider.as_str()), Some(Provenance::SeededFromProvider));
        assert_eq!(Provenance::parse("exchange_seeded"), None);
    }

    #[test]
    fn retention_defaults_match_the_config_defaults() {
        let defaults = Retentions::defaults();
        assert_eq!(defaults.s1, 48 * 3_600_000);
        assert_eq!(defaults.m1, 60 * 86_400_000);
        assert_eq!(defaults.h1, 2 * 365 * 86_400_000);
        assert_eq!(defaults.d1, None);
    }
}
