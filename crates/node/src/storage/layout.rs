//! Which directories hold which store.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::config::NodeConfig;
use crate::history::marks::store::MARKS_FILE;
use crate::node::marks::{PRIOR_FILE, SEED_FILE};

/// The start of the file names of the authenticated history index in `snapshots.dir`.
pub(super) const AUTHENTICATED_PREFIX: &str = "authenticated-";

/// How a store picks its files from a directory: by the file's name, among the entries of the directory itself.
type Pick = fn(&str) -> bool;

/// What the node keeps on disk that grows, in the order its volumes are named.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Store {
    /// The log's segments and manifest, in `node.log_dir`.
    Log,
    /// The account history index and its derived rows: all of `history.dir` but the marks cache, and, with
    /// `[snapshots]`, the authenticated index in its directory.
    HistoryIndex,
    /// The index-mark history, in `history.dir`.
    MarksCache,
    /// The signed recovery caches: `snapshots.dir` but the authenticated index.
    RecoveryCaches,
    /// The daily snapshots, in `daily_snapshots.dir`.
    DailySnapshots,
    /// The durable alert pages, in `alerts.outbox.dir`.
    AlertOutbox,
}

/// The directories of the configured stores.
#[derive(Debug)]
pub struct Layout {
    log: PathBuf,
    history: PathBuf,
    snapshots: Option<PathBuf>,
    daily_snapshots: Option<PathBuf>,
    outbox: PathBuf,
    /// The allocated bytes of each sealed log segment, by its first seq, which `walk_log` reads once.
    pub(super) sealed: Mutex<BTreeMap<u64, u64>>,
}

impl Store {
    /// The store's label in metrics and alerts.
    pub fn name(self) -> &'static str {
        match self {
            Store::Log => "log",
            Store::HistoryIndex => "history_index",
            Store::MarksCache => "marks_cache",
            Store::RecoveryCaches => "recovery_caches",
            Store::DailySnapshots => "daily_snapshots",
            Store::AlertOutbox => "alert_outbox",
        }
    }
}

/// Every entry.
fn all(_: &str) -> bool {
    true
}

/// The marks cache's files: its database and the hidden files that hold a cache while a start seeds or hands it on.
fn marks(name: &str) -> bool {
    [MARKS_FILE, SEED_FILE, PRIOR_FILE].iter().any(|file| name.starts_with(file))
}

/// Every entry but the marks cache's.
fn not_marks(name: &str) -> bool {
    !marks(name)
}

/// The authenticated history index's files.
fn authenticated(name: &str) -> bool {
    name.starts_with(AUTHENTICATED_PREFIX)
}

/// Every entry but the authenticated history index's.
fn not_authenticated(name: &str) -> bool {
    !authenticated(name)
}

impl Layout {
    /// The directories `cfg` names.
    pub fn of(cfg: &NodeConfig) -> Self {
        Self {
            log: cfg.node.log_dir.clone(),
            history: cfg.history.dir.clone(),
            snapshots: cfg.snapshots.as_ref().map(|snapshots| snapshots.dir.clone()),
            daily_snapshots: cfg.daily_snapshots.as_ref().map(|daily| daily.dir.clone()),
            outbox: cfg.alerts.outbox.dir.clone(),
            sealed: Mutex::default(),
        }
    }

    /// Each store's directories, with which of their entries it owns.
    pub(super) fn stores(&self) -> Vec<(Store, Vec<(&Path, Pick)>)> {
        let mut history_dirs = vec![(self.history.as_path(), not_marks as Pick)];
        history_dirs.extend(self.snapshots.as_deref().map(|dir| (dir, authenticated as Pick)));
        let mut stores = vec![
            (Store::Log, vec![(self.log.as_path(), all as Pick)]),
            (Store::HistoryIndex, history_dirs),
            (Store::MarksCache, vec![(self.history.as_path(), marks as Pick)]),
        ];
        if let Some(dir) = &self.snapshots {
            stores.push((Store::RecoveryCaches, vec![(dir.as_path(), not_authenticated as Pick)]));
        }
        if let Some(dir) = &self.daily_snapshots {
            stores.push((Store::DailySnapshots, vec![(dir.as_path(), all as Pick)]));
        }
        stores.push((Store::AlertOutbox, vec![(self.outbox.as_path(), all as Pick)]));
        stores
    }

    #[cfg(test)]
    pub(super) fn new(
        log: PathBuf,
        history: PathBuf,
        snapshots: Option<PathBuf>,
        daily_snapshots: Option<PathBuf>,
        outbox: PathBuf,
    ) -> Self {
        Self { log, history, snapshots, daily_snapshots, outbox, sealed: Mutex::default() }
    }
}
