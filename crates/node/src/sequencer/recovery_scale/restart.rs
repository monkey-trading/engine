//! Restarting on the generated log by replaying the whole of it: `engine replay`'s replay, and a node's
//! recovery to its first command, each from the book the log starts from (`Base`). A whole log's replay
//! starts from an empty engine, and this one from the book, so each is the node's own read with its
//! replay started from the book (`Replay::seeded`). The restart from a cache is `cache`'s.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::ensure;
use engine_core::types::AccountId;
use tokio::sync::{Semaphore, broadcast};

use crate::api::{Feed, MAX_BACKFILLS};
use crate::common::{CHAIN_ID, VAULT};
use crate::config::HistorySection;
use crate::history::derived::{Deriver, RowsStore};
use crate::history::marks::{Retentions, Store};
use crate::history::{History, HistoryCheck, PageQuery};
use crate::log::{CheckpointDomain, LogEntry, Manifest, Offset, ReplayWorkers};
use crate::node::marks::recovery_sink;
use crate::sequencer::Sequencer;
use crate::sequencer::recovery_scale::Base;
use crate::sequencer::recovery_scale::flow::Node;
use crate::sequencer::replay::replay_path_from;
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, stats};

/// The log's deployment.
pub(super) const DOMAIN: CheckpointDomain = CheckpointDomain { chain_id: CHAIN_ID, vault: VAULT };

/// A replay's commands and checkpoints, and its wall time.
pub(super) struct Replayed {
    pub commands: u64,
    pub checkpoints: u64,
    pub took: Duration,
}

impl Replayed {
    /// Commands a second.
    pub(super) fn rate(&self) -> f64 {
        self.commands as f64 / self.took.as_secs_f64()
    }
}

/// A recovery by full replay: the replay, seeding the marks cache and the dashboard record, then the
/// sequencer's start on the replayed log to its first command, and the whole of it.
pub(super) struct Recovered {
    pub node: Node,
    pub replayed: Replayed,
    pub start: Duration,
    pub first_command: Duration,
    /// The history index, checked against the log as the replay read it, to catch up after the start.
    pub history: History,
}

/// `engine replay`'s replay of the log in `dir` from the book: every command's events, every
/// checkpoint's root and signature, and the command seals, under `signer`, checked ahead by `workers`.
pub(super) fn replay(base: &Base, dir: &Path, signer: AccountId, workers: ReplayWorkers) -> Replayed {
    let engine = base.engine();
    let started = Instant::now();
    let replayed = replay_path_from(engine, dir, signer, workers).expect("the log replays to its roots from the book");
    Replayed { commands: replayed.commands, checkpoints: replayed.checkpoints, took: started.elapsed() }
}

/// A node's recovery of the log in `dir` by full replay, as `node::recovery` reads a log with no cache:
/// the history index checked against each entry, each mark seeding the marks cache in `run`, each
/// command recorded into the dashboard record, the command seals checked ahead by `workers`, then the
/// sequencer opened on the log, signing with `key`, and its first command.
/// Error cases:
/// - the log does not replay, the marks cache cannot be opened or seeded, or the sequencer refuses the
///   log or its first command
pub(super) fn recover(
    base: &Base,
    dir: &Path,
    run: &Path,
    key: Arc<EnclaveKey>,
    workers: ReplayWorkers,
) -> anyhow::Result<Recovered> {
    let history = HistorySection { dir: run.join("history"), ..dev_config().history };
    let marks = Rc::new(RefCell::new(Store::open_in(&run.join("marks"), Retentions::defaults(), key.marks_key())?));
    let (sink, failed, _) = recovery_sink(&marks);
    let mut check = HistoryCheck::open(&history)?;
    let engine = base.engine();
    let started = Instant::now();
    let signer = AccountId(key.address());
    let observe = |entry: &LogEntry| check.observe(entry);
    let log = Sequencer::read_log_from(engine, dir, (DOMAIN, signer), Some(sink), Some(stats()), observe, workers)?;
    ensure!(!failed.get(), "seeding the marks cache from the log failed");
    let replayed =
        Replayed { commands: log.replayed.commands, checkpoints: log.replayed.checkpoints, took: started.elapsed() };
    let history = check.finish()?;
    let starting = Instant::now();
    let mut node = Node::recovered(log, dir, key, None)?;
    node.first_command()?;
    Ok(Recovered { node, replayed, start: starting.elapsed(), first_command: started.elapsed(), history })
}

/// The history index's catch-up over the log in `dir` from where `history` resumes to `node`'s last
/// command: each command read back from the log through `node`'s published view and indexed, each
/// rollover's rows taken from `rows`, as an index that resumes behind its log takes them.
/// Error cases:
/// - the log cannot be read, or the index's writer fails
pub(super) async fn catch_up_history(
    mut history: History,
    node: &Node,
    dir: &Path,
    rows: Arc<RowsStore>,
) -> anyhow::Result<Duration> {
    // No sequencer runs while it catches up: the live stream is closed, so the feed ends at the log's
    // end.
    let (_, live) = broadcast::channel(1);
    let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
    // An empty index resumes at the first seq a log can hold, and this log starts where the book ends.
    let first = Manifest::load(dir)?.segments.first().map_or(0, |listed| listed.first_seq);
    let start = history.resume_at().max(Offset::start_of(first));
    let feed = Feed::new(live, dir.to_owned(), node.manifest.clone(), backfills, Some(start));
    let deriver = Deriver::new(node.seq.derived_rows(), Some((dir.to_owned(), node.manifest.clone())))
        .storing(rows, Default::default());
    history.catch_up_to(node.seq.last_seq());
    let reader = history.reader();
    let started = Instant::now();
    history.run(feed, deriver).await?;
    let took = started.elapsed();
    // Readers are let in once the index takes the last command.
    let query = PageQuery { account: AccountId([0; 20]), kind: None, before: None, after: None, limit: 1 };
    ensure!(reader.page(query).await?.is_some(), "the history index did not take the log's last command");
    Ok(took)
}

/// The size of the file at `path`.
pub(super) fn file_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |found| found.len())
}
