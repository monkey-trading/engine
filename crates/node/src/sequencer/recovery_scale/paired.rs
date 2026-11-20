//! The log's node with paired recovery caches, as a node with `[snapshots]` runs: its history owner
//! indexes the log and publishes each cache a capture hands it, the stats worker's copy encoding the
//! engine, every `interval` of sequenced time. A restart reads the newest cache through the recovery
//! the node runs (`Sequencer::read_paired_log` composing it), the posted log head's check included,
//! then makes the start checks a node makes before it sequences and stamps its first command.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use engine_core::Engine;
use engine_core::types::AccountId;
use tokio::sync::{Semaphore, broadcast};
use tokio::task::JoinHandle;

use crate::api::{Feed, MAX_BACKFILLS};
use crate::config::{NodeConfig, SnapshotSection};
use crate::history::HistoryPlan;
use crate::history::derived::Deriver;
use crate::history::runtime::Stopper;
use crate::history::startup::Plan;
use crate::log::{CheckpointParent, ReplayWorkers};
use crate::log_heads::Head;
use crate::sequencer::recovery_scale::flow::{CHECKPOINT_EVERY, Node, Pairing, SEGMENT_COMMANDS};
use crate::sequencer::recovery_scale::restart::DOMAIN;
use crate::sequencer::{ReplayedLog, Sequencer};
use crate::snapshot::sealing::Endpoint;
use crate::tee::EnclaveKey;
use crate::test_support::stats;

/// Stream messages the history owner's feed may fall behind by before it reads the log instead.
const STREAM_CAPACITY: usize = 4_096;

/// A node with paired caches: the node, the endpoint its captures go through, and its history owner,
/// with the handle that stops it.
pub(super) struct Paired {
    pub node: Node,
    pub sealing: Endpoint,
    history: JoinHandle<anyhow::Result<()>>,
    stopper: Stopper,
}

/// The config of a node whose log is in `run`, with its caches there, one every `interval`, pinned to
/// `signer`, its replays' command seals checked ahead by `workers`, in the log's deployment and at
/// the template's cadences.
pub(super) fn node_config(run: &Path, interval: Duration, (signer, workers): (AccountId, ReplayWorkers)) -> NodeConfig {
    let mut cfg = crate::test_support::dev_config();
    cfg.node.replay_workers = workers;
    cfg.custody.chain_id = DOMAIN.chain_id;
    cfg.custody.vault = DOMAIN.vault.to_string();
    cfg.node.log_dir = run.join("log");
    cfg.node.segment_commands = SEGMENT_COMMANDS;
    cfg.node.checkpoint_every = CHECKPOINT_EVERY;
    // Unused with paired caches, whose history index is in the cache directory.
    cfg.history.dir = run.join("unpaired-history");
    cfg.snapshots = Some(SnapshotSection {
        dir: run.join("snapshots"),
        retain: 2.try_into().expect("non-zero"),
        interval,
        max_bytes: (16usize << 30).try_into().expect("non-zero"),
        trusted_signers: vec![signer],
    });
    cfg
}

impl Paired {
    /// A node under `cfg` that takes over `book` with an empty log, signing with `key`.
    /// Error cases:
    /// - the log or its history cannot be opened, or the sequencer refuses the book
    pub(super) fn start(book: Engine, cfg: &NodeConfig, key: Arc<EnclaveKey>) -> anyhow::Result<Self> {
        let signer = AccountId(key.address());
        // A gated log's first read, as `Sequencer::recover` reads one: from genesis, with no checkpoint.
        let genesis = Some(CheckpointParent::default());
        let (log, plan) =
            Sequencer::read_paired_log(cfg, Some(signer), genesis, Default::default(), None, None, |_| {})?;
        Self::open((log, plan), cfg, key, |log, dir, key, pairing| Node::start(book, log, dir, key, pairing))
    }

    /// A node's restart under `cfg` from its newest cache, with `posted` the head on chain, signing with
    /// `key`, to its first command: the last command of the log it recovered, and the time
    /// `read_paired_log` took and the start's after it.
    /// Error cases:
    /// - no cache serves the restart, the posted head refuses it, or the start refuses the log
    pub(super) fn restart(
        cfg: &NodeConfig,
        key: Arc<EnclaveKey>,
        posted: Head,
    ) -> anyhow::Result<(Self, Option<u64>, (Duration, Duration))> {
        let signer = AccountId(key.address());
        let started = Instant::now();
        let (log, plan) = Sequencer::read_paired_log(
            cfg,
            Some(signer),
            None,
            Default::default(),
            Some(stats()),
            Some(posted),
            |_| {},
        )?;
        ensure!(log.snapshot_sequence().is_some(), "the restart replayed the whole log, not a cache's suffix");
        let (read, recovered) = (started.elapsed(), log.last_seq());
        let starting = Instant::now();
        let mut paired =
            Self::open((log, plan), cfg, key, |log, dir, key, pairing| Node::recovered(log, dir, key, Some(pairing)))?;
        paired.node.first_command()?;
        Ok((paired, recovered, (read, starting.elapsed())))
    }

    /// Activates the history `plan` gives beside `log`, as a node does once its start checks pass,
    /// starts the node `node` builds on it, and runs the history owner on the node's stream.
    /// Error cases:
    /// - the history cannot be activated, or `node` fails
    fn open(
        (log, plan): (ReplayedLog, Plan),
        cfg: &NodeConfig,
        key: Arc<EnclaveKey>,
        node: impl FnOnce(ReplayedLog, &Path, Arc<EnclaveKey>, Pairing) -> anyhow::Result<Node>,
    ) -> anyhow::Result<Self> {
        let (log, mut history) = HistoryPlan::authenticated(plan)?.finish_with_log(log, key.clone())?;
        let sealing = history.sealing().context("authenticated history takes captures")?;
        let stopper = history.stopper().context("authenticated history stops on request")?;
        let (stream, feed) = broadcast::channel(STREAM_CAPACITY);
        let dir = &cfg.node.log_dir;
        let node = node(log, dir, key, Pairing { sealing: sealing.clone(), stream })?;
        history.catch_up_to(node.seq.last_seq());
        let backfills = Arc::new(Semaphore::new(MAX_BACKFILLS));
        let feed = Feed::new(feed, dir.clone(), node.manifest.clone(), backfills, Some(history.resume_at()));
        let deriver = Deriver::new(node.seq.derived_rows(), Some((dir.clone(), node.manifest.clone())));
        let history = tokio::spawn(history.run(feed, deriver));
        Ok(Self { node, sealing, history, stopper })
    }

    /// Stops the node as a hard stop does: no capture at its last checkpoint, and the history owner
    /// stopped at its turn's end, wherever it stands, for the next start to index the rest.
    /// Error cases:
    /// - the history owner failed
    pub(super) async fn stop(self) -> anyhow::Result<()> {
        let Self { node, history, stopper, .. } = self;
        stopper.stop();
        drop(node);
        history.await.context("the history owner's task")?
    }
}
