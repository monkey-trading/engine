//! Judging a volume's space: its free share, and the time it has to fill at its recent growth.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

use tracing::{error, info, warn};

use crate::config::StorageSection;
use crate::metrics::{Metrics, VolumeLabel};
use crate::storage::layout::Store;
use crate::storage::reading::{Reading, Volume};

/// A level holds until the measure clears its threshold by a tenth, so a volume at the edge does not page and
/// resolve in turn.
const CLEARANCE_TENTHS: u128 = 11;
const GIB: f64 = (1u64 << 30) as f64;
const DAY_SECS: f64 = 86_400.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    #[default]
    Clear,
    Warning,
    Critical,
}

#[derive(Default)]
struct VolumeState {
    /// The level last announced.
    level: Level,
    /// Whether the alerts the volume is not at have been resolved, which waits for a projection: a run before may
    /// have left a fill-time episode open, and a watch with no growth history cannot tell it is over.
    reconciled: bool,
    /// Used bytes at each sample within the rate window.
    samples: VecDeque<(Instant, u64)>,
}

/// The state of the disk alerts across samples.
pub struct Watch {
    cfg: StorageSection,
    volumes: BTreeMap<String, VolumeState>,
    /// The stores, or `all`, with an open `storage_unreadable`.
    unreadable: BTreeSet<String>,
    /// The stores, or `all`, judged readable or not at least once.
    judged: BTreeSet<String>,
}

impl Watch {
    /// A watch with no history: it raises what a volume is at when first read, and resolves the alerts a volume is not
    /// at once its growth over a window is known, and for each store when first read, since a run before it may
    /// have left an episode open.
    pub fn new(cfg: StorageSection) -> Self {
        Self { cfg, volumes: BTreeMap::new(), unreadable: BTreeSet::new(), judged: BTreeSet::new() }
    }

    /// Judges each volume of `reading` at `now` and sets its growth and projected fill as metrics. Raises each
    /// level's alert as a volume reaches it and resolves it as the volume leaves, and does the same for each store
    /// that could or could not be read. The first time it judges a store, and a volume once its growth over a window
    /// is known, it also resolves each alert the store or volume is not at, which pages nothing unless an earlier run
    /// left that episode open.
    pub fn observe(&mut self, now: Instant, reading: &Reading, metrics: &Metrics) {
        for volume in &reading.volumes {
            self.judge(now, volume, reading, metrics);
        }
        let mut failed = BTreeMap::new();
        for (store, error) in &reading.failures {
            failed.entry(store.name().to_owned()).or_insert(error.as_str());
        }
        let known = reading.stores.iter().map(|(store, _)| store.name().to_owned()).chain(["all".to_owned()]).collect();
        self.settle(&failed, known);
    }

    /// Records that no read finished, as `storage_unreadable` for `all`, which the next read that finishes resolves.
    /// The stores' own episodes stay as they are, since nothing was read of them.
    pub fn unread(&mut self, why: &str) {
        self.settle(&BTreeMap::from([("all".to_owned(), why)]), vec!["all".to_owned()]);
    }

    /// Records `volume`'s used bytes at `now`, sets its growth and projected fill as metrics, and announces its level
    /// when it changed.
    fn judge(&mut self, now: Instant, volume: &Volume, reading: &Reading, metrics: &Metrics) {
        let label = VolumeLabel { volume: volume.name.clone() };
        let state = self.volumes.entry(volume.name.clone()).or_default();
        let used = volume.total_bytes.saturating_sub(volume.free_bytes);
        state.samples.push_back((now, used));
        // The oldest sample kept is the newest one at least a window old, so jitter in when reads finish never
        // leaves the window short.
        while state.samples.get(1).is_some_and(|(at, _)| now.duration_since(*at) >= self.cfg.rate_window) {
            state.samples.pop_front();
        }
        let growth = growth(state, now, &self.cfg);
        match growth {
            Some(rate) => {
                metrics.storage.volume_growth_bytes_per_second.get_or_create(&label).set(rate);
            }
            None => {
                metrics.storage.volume_growth_bytes_per_second.remove(&label);
            }
        }
        let fill_secs = growth.filter(|rate| *rate > 0.0).map(|rate| volume.free_bytes as f64 / rate);
        match fill_secs {
            Some(secs) => {
                metrics.storage.volume_fill_seconds.get_or_create(&label).set(secs);
            }
            None => {
                metrics.storage.volume_fill_seconds.remove(&label);
            }
        }
        let level = level(&self.cfg, volume, fill_secs, state.level);
        let reconcile = !state.reconciled && growth.is_some();
        if level != state.level || reconcile {
            let message = describe(volume, growth, fill_secs, reading);
            announce(&volume.name, state.level, level, &message);
            if reconcile {
                resolve_above(&volume.name, level, &message);
                state.reconciled = true;
            }
            state.level = level;
        }
    }

    /// Raises `storage_unreadable` for each of `failed` not yet open, and resolves each of `known` that is not
    /// failed and was open, or is judged for the first time.
    fn settle(&mut self, failed: &BTreeMap<String, &str>, known: Vec<String>) {
        for (store, error) in failed {
            self.judged.insert(store.clone());
            if self.unreadable.insert(store.clone()) {
                warn!(alert = "storage_unreadable", store = %store, "cannot read {store}'s size: {error}");
            }
        }
        for store in known.into_iter().filter(|store| !failed.contains_key(store)) {
            let was_open = self.unreadable.remove(&store);
            if self.judged.insert(store.clone()) || was_open {
                info!(alert_resolved = "storage_unreadable", store = %store, "{store}'s size reads again");
            }
        }
    }
}

/// Growth of used bytes a second over the samples in the window, once the window is full: a burst written in a
/// window's first hours, such as an index catching up after a start, is not read as a steady rate.
fn growth(state: &VolumeState, now: Instant, cfg: &StorageSection) -> Option<f64> {
    let (first_at, first) = *state.samples.front()?;
    let (_, last) = *state.samples.back()?;
    let span = now.duration_since(first_at);
    (span >= cfg.rate_window).then(|| (last as f64 - first as f64) / span.as_secs_f64())
}

/// The level a volume is at, given the level it held: a held level needs its measure past the threshold by a tenth
/// to clear.
fn level(cfg: &StorageSection, volume: &Volume, fill_secs: Option<f64>, held: Level) -> Level {
    let tenths = |level| if held >= level { CLEARANCE_TENTHS } else { 10 };
    let low = |percent: u8, level| {
        u128::from(volume.free_bytes) * 1000
            < u128::from(volume.total_bytes) * u128::from(percent) * 10 * tenths(level) / 10
    };
    let soon = |within: std::time::Duration, level| {
        fill_secs.is_some_and(|secs| secs < within.as_secs_f64() * tenths(level) as f64 / 10.0)
    };
    if low(cfg.critical_free_percent, Level::Critical) || soon(cfg.critical_fill_within, Level::Critical) {
        Level::Critical
    } else if low(cfg.warn_free_percent, Level::Warning) || soon(cfg.warn_fill_within, Level::Warning) {
        Level::Warning
    } else {
        Level::Clear
    }
}

/// The alert line's text: where the volume stands, what it is growing at, and what is on it.
fn describe(volume: &Volume, growth: Option<f64>, fill_secs: Option<f64>, reading: &Reading) -> String {
    let share = 100.0 * volume.free_bytes as f64 / volume.total_bytes as f64;
    let trend = match (growth, fill_secs) {
        (_, Some(secs)) => format!(
            "growing {:.1} GiB a day, full in {:.1} days",
            growth.unwrap_or_default() * DAY_SECS / GIB,
            secs / DAY_SECS
        ),
        (Some(_), None) => "not growing over the sampled window".to_owned(),
        (None, _) => "growth not yet sampled".to_owned(),
    };
    let stores = reading
        .stores
        .iter()
        .filter(|(store, _)| volume.stores.contains(store))
        .map(|(store, bytes): &(Store, u64)| format!("{} {:.1} GiB", store.name(), *bytes as f64 / GIB))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "volume {} has {:.1} GiB free of {:.1} GiB ({share:.1}%), {trend}; holds {stores}",
        volume.name,
        volume.free_bytes as f64 / GIB,
        volume.total_bytes as f64 / GIB,
    )
}

/// Raises the alert of each level `to` reaches and resolves that of each it leaves; the Warning stays open while the
/// volume is Critical.
fn announce(volume: &str, from: Level, to: Level, message: &str) {
    match (from >= Level::Warning, to >= Level::Warning) {
        (false, true) => warn!(alert = "disk_space_low", volume = %volume, "{message}"),
        (true, false) => info!(alert_resolved = "disk_space_low", volume = %volume, "{message}"),
        _ => {}
    }
    match (from >= Level::Critical, to >= Level::Critical) {
        (false, true) => error!(alert = "disk_space_critical", volume = %volume, "{message}"),
        (true, false) => info!(alert_resolved = "disk_space_critical", volume = %volume, "{message}"),
        _ => {}
    }
}

/// Resolves the alert of each level above `level`, which pages nothing unless an earlier run left it open.
fn resolve_above(volume: &str, level: Level, message: &str) {
    if level < Level::Warning {
        info!(alert_resolved = "disk_space_low", volume = %volume, "{message}");
    }
    if level < Level::Critical {
        info!(alert_resolved = "disk_space_critical", volume = %volume, "{message}");
    }
}
