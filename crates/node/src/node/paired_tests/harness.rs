//! Exercises the same read-only recovery, independent preflight and authorized activation as Node.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::types::AccountId;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::api::{Feed, MAX_BACKFILLS};
use crate::checkpoints::{CheckpointChain, VaultState, View, Work};
use crate::clock::{Clock, SystemClock};
use crate::config::{NodeConfig, SnapshotSection};
use crate::history::Reader;
use crate::history::runtime::TestState;
use crate::log::{CheckpointDomain, CheckpointParent, LogEntry, Manifest};
use crate::node::{preflight, read_log_and_history_observe};
use crate::sequencer::{Request, Sequencer, StreamMsg, replay_path_with_trust};
use crate::tee::EnclaveKey;
use crate::test_support::{deposit, dev_config, log_entries, sample_around, scratch_dir, sequencer_config};

/// The longest one step of a paired node may take before its test fails as hung: one command, the
/// history owner's current and next turn, or one side of a stop. A wait spanning many steps fails
/// only when none of them completes within it, so a loaded machine slows a test down without
/// failing it.
pub(crate) const HANG: Duration = Duration::from_secs(300);

pub(crate) struct Node {
    pub cfg: NodeConfig,
    pub tx: mpsc::Sender<Request>,
    pub reader: Reader,
    pub manifest: watch::Receiver<Manifest>,
    pub stream: broadcast::Sender<Arc<StreamMsg>>,
    pub snapshot_seq: Option<u64>,
    pub sealing: crate::snapshot::sealing::Endpoint,
    clock: Arc<dyn Clock>,
    sequencer: JoinHandle<Result<()>>,
    history: JoinHandle<Result<()>>,
    custody: mpsc::UnboundedReceiver<crate::sequencer::SignedWithdrawal>,
}

struct NoChain;
impl CheckpointChain for NoChain {
    async fn state(&self, _: View) -> Result<VaultState> {
        unreachable!("mock custody has no checkpoint chain")
    }
    async fn post(&self, _: &crate::log::Checkpoint) -> Result<()> {
        unreachable!()
    }
}

/// The state root a strict audit replay of the whole log reaches, trusting only the test key.
pub(crate) fn audit_root(cfg: &NodeConfig) -> [u8; 32] {
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() };
    let signer = AccountId(EnclaveKey::test_key().address());
    replay_path_with_trust(&cfg.node.log_dir, Some(domain), Some(signer)).unwrap().engine.state_root()
}

/// Uses independent signer pins supplied by the mock attestation test key, never cache metadata.
pub(crate) fn configuration(label: &str) -> NodeConfig {
    let mut cfg = dev_config();
    let root = scratch_dir(label);
    cfg.node.log_dir = root.join("log");
    cfg.history.dir = root.join("legacy-history");
    cfg.node.segment_commands = 4;
    cfg.node.checkpoint_every = 3;
    cfg.snapshots = Some(SnapshotSection {
        dir: root.join("snapshots"),
        retain: 4.try_into().unwrap(),
        max_bytes: (16 * 1024 * 1024).try_into().unwrap(),
        trusted_signers: vec![AccountId(EnclaveKey::test_key().address())],
    });
    cfg
}

impl Node {
    /// Starts real log, proof stores, sequencer and supervised history with production startup ordering.
    pub(crate) async fn start(cfg: NodeConfig) -> Result<Self> {
        Self::start_at(cfg, Arc::new(SystemClock)).await
    }

    /// Controls only the existing clock input; all storage and cryptographic paths remain real.
    pub(crate) async fn start_at(cfg: NodeConfig, clock: Arc<dyn Clock>) -> Result<Self> {
        Self::start_with(cfg, clock, None, None).await
    }

    /// Resumes a log whose genesis is `genesis`, the commands a fixture log ran, rather than the dev
    /// config's, as a node configured with that genesis would.
    pub(crate) async fn start_resuming(cfg: NodeConfig, clock: Arc<dyn Clock>, genesis: Vec<Command>) -> Result<Self> {
        Self::start_with(cfg, clock, None, Some(genesis)).await
    }

    /// Starts with checkpoint posting gated on `parent`, the chain's finalized checkpoint, sending
    /// each checkpoint's work to `work` as the poster would receive it.
    pub(crate) async fn start_gated(
        cfg: NodeConfig,
        parent: CheckpointParent,
        work: mpsc::Sender<Work>,
    ) -> Result<Self> {
        Self::start_with(cfg, Arc::new(SystemClock), Some((parent, work)), None).await
    }

    async fn start_with(
        cfg: NodeConfig,
        clock: Arc<dyn Clock>,
        gate: Option<(CheckpointParent, mpsc::Sender<Work>)>,
        genesis: Option<Vec<Command>>,
    ) -> Result<Self> {
        let key = Arc::new(EnclaveKey::test_key());
        let parent = gate.as_ref().map(|(parent, _)| *parent);
        let marks = std::rc::Rc::new(std::cell::RefCell::new(crate::history::marks::Store::open_in(
            &crate::test_support::scratch_dir("paired-harness-marks"),
            crate::history::marks::Retentions::defaults(),
        )?));
        let (log, plan) = read_log_and_history_observe(&cfg, Some(AccountId(key.address())), parent, &marks, |_| {})?;
        let snapshot_seq = log.snapshot_sequence();
        let (stream, _) = broadcast::channel(2);
        let (withdrawals, custody) = mpsc::unbounded_channel();
        let (manifest_tx, manifest) = watch::channel(Manifest::default());
        let mut seq_cfg = sequencer_config(&cfg.node.log_dir, manifest_tx);
        seq_cfg.segment_commands = cfg.node.segment_commands;
        seq_cfg.checkpoint_every = cfg.node.checkpoint_every;
        seq_cfg.clock = clock.clone();
        if let Some(genesis) = genesis {
            seq_cfg.genesis = genesis;
        }
        if let Some((parent, work)) = gate {
            seq_cfg.checkpoint_parent = parent;
            seq_cfg.checkpoints = Some(work);
        }
        preflight::authorize::<NoChain>(None, Some((&log, &seq_cfg, AccountId(key.address())))).await?;
        let (log, mut history) = plan.finish_with_log(log, key.clone())?;
        let sealing = history.sealing().expect("authenticated history has a seal endpoint");
        seq_cfg.history_sealing = Some(sealing.clone());
        let mut seq = Sequencer::recover_from(log, seq_cfg, key, stream.clone(), withdrawals)?;
        seq.carry_toxicity_notional(cfg.genesis.params.toxicity.notional.unwrap())?;
        // The `V20` rules, before access screening: these fixtures credit receipts unscreened.
        seq.activate_legacy_protocol()?;
        if seq.needs_genesis() {
            seq.genesis(&cfg.genesis)?;
        }
        seq.trust(sample_around(clock.as_ref()))?;
        history.catch_up_to(seq.last_seq());
        let reader = history.reader();
        let feed = Feed::new(
            stream.subscribe(),
            cfg.node.log_dir.clone(),
            manifest.clone(),
            Arc::new(Semaphore::new(MAX_BACKFILLS)),
            Some(history.resume_at()),
        );
        let (tx, received) = mpsc::channel(16);
        let sequencer = tokio::spawn(seq.run(received));
        let history = tokio::spawn(history.run(feed));
        Ok(Self { cfg, tx, reader, manifest, stream, snapshot_seq, sealing, clock, sequencer, history, custody })
    }

    /// Waits until the history owner has indexed every entry the sequencer has flushed, observed
    /// through the owner's own queue rather than by sleeping, and returns the owner's state there.
    /// It fails only when the owner takes no further entry for `HANG`, however long the whole
    /// catch-up takes. Entries taken bump a counter outside the owner's queue, so progress stays
    /// observable while one inspect waits out a slow turn; an inspect that outlasts `HANG` is
    /// replaced by a fresh one.
    pub(crate) async fn history_caught_up(&self) -> TestState {
        let target = self.manifest.borrow().flushed;
        // Start at the counter's current value: entries already taken owe no progress, so an
        // owner stuck from this call on fails inside the first `HANG` window.
        let (mut taken, mut since) = (self.sealing.taken.load(Ordering::Acquire), Instant::now());
        loop {
            let state = match timeout(HANG, self.sealing.inspect()).await {
                Ok(Ok(state)) => Some(state),
                Ok(Err(error)) => panic!("expected the history owner's inspected state, got an error: {error:#}"),
                Err(_) => None,
            };
            let seen = self.sealing.taken.load(Ordering::Acquire);
            if seen > taken {
                (taken, since) = (seen, Instant::now());
            }
            if let Some(state) = state
                && state.last_entry >= target
            {
                return state;
            }
            assert!(
                since.elapsed() < HANG,
                "the history owner takes no further entry for {HANG:?}, at {taken} entries taken short of the flushed frontier {target:?}"
            );
            tokio::task::yield_now().await;
        }
    }

    /// Stops the history owner as a failed task would, returning once its input queue is closed.
    pub(crate) async fn stop_history(&self) {
        self.history.abort();
        timeout(HANG, async {
            while !self.sealing.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the history owner closes its queue");
    }

    /// Hands the sequencer a fresh trusted time sample, as the node's sampler would; a run longer
    /// than the no-sample grace otherwise refuses every stamp.
    pub(crate) async fn refresh_time(&self) {
        self.tx.send(Request::TimeSample(sample_around(self.clock.as_ref()))).await.unwrap();
    }

    /// Sequences `cmd` under a fresh trusted time sample, so no run is refused for its length, and
    /// returns its events.
    pub(crate) async fn submit(&self, cmd: Command) -> Vec<Event> {
        self.refresh_time().await;
        let (reply, done) = oneshot::channel();
        self.tx.send(Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
        timeout(HANG, done).await.unwrap().unwrap()
    }

    /// Sequences one deposit and stops, so the drain signs a checkpoint over the state this node
    /// recovered plus that deposit; a strict audit replay of the whole log must reach its root.
    pub(crate) async fn stop_after_signing(self, label: &str) -> (NodeConfig, Reader) {
        let recovered = self.manifest.borrow().flushed;
        self.submit(deposit(1, label)).await;
        let (cfg, reader) = self.stop().await;
        let signed = log_entries(&cfg.node.log_dir)
            .into_iter()
            .rev()
            .find_map(|entry| match entry {
                LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
                _ => None,
            })
            .expect("the log holds a checkpoint");
        assert!(recovered.is_none_or(|at| signed.seq > at.seq), "the restarted node signed a checkpoint");
        assert_eq!(signed.root, format!("0x{}", hex::encode(audit_root(&cfg))), "the audit reaches the signed root");
        (cfg, reader)
    }

    /// Removes every broadcast owner so the real Feed EOF must drain its queued seals before exit.
    pub(crate) async fn stop(self) -> (NodeConfig, Reader) {
        let Self { cfg, tx, reader, manifest, stream, sequencer, history, custody, .. } = self;
        drop(tx);
        timeout(HANG, sequencer).await.unwrap().unwrap().unwrap();
        drop(stream);
        drop(manifest);
        timeout(HANG, history).await.unwrap().unwrap().unwrap();
        drop(custody);
        (cfg, reader)
    }
}
