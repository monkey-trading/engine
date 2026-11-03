//! The real sequencer and checkpoint worker, in paused time, over a chain that finalizes a
//! checkpoint `FINALITY` after accepting it: the signed queue stays one deep, withdrawals get their
//! authorizations within two finality delays, and only a poster that stops raises the backlog.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::types::AccountId;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep};

use super::*;
use crate::checkpoints::{
    ApprovalId, ApprovalRegistry, ApprovalStatus, CheckpointChain, LifecycleEvidence, Observation, Poster, Registry,
    VaultState, View, Work,
};
use crate::clock::test_clock::TestClock;
use crate::log::{Checkpoint, CheckpointDomain};
use crate::sequencer::{Request, SignedWithdrawal, StreamBody, StreamMsg};
use crate::test_support::{Alert, TRADER, capture_alerts, sample_around};

/// Ethereum's nominal finality.
const FINALITY: Duration = Duration::from_secs(13 * 60);
const RETRY: Duration = Duration::from_secs(12);
/// How often the driver sequences a command.
const STEP: Duration = Duration::from_secs(5);
/// With a command every `STEP`, 26 checkpoints a finality delay if nothing held them back.
const CHECKPOINT_EVERY: u64 = 6;
const STALLED_AFTER: Duration = Duration::from_secs(30 * 60);
/// What an approval can wait: the rest of the checkpoint on its way and all of its own, the worker's
/// ticks that report the first final, post the next, and report that final, and the driver's step.
const MOST_AN_APPROVAL_WAITS: Duration = Duration::from_secs(2 * 13 * 60 + 3 * 12 + 5);
const DEPOSIT: i64 = 1_000;

#[tokio::test(start_paused = true)]
async fn a_slow_poster_keeps_one_checkpoint_on_its_way_and_every_approval_within_two_finality_delays() {
    let (_guard, mut alerts) = capture_alerts();
    let mut node = SlowNode::start("slow-poster").await;

    node.run_for(FINALITY * 3).await;
    let cadence = node.signed.len();
    assert!((3..=4).contains(&cadence), "one a finality delay, not 26: {cadence}");

    // An approval with one on its way is covered by the next, so it waits for both.
    let approved = Instant::now();
    node.approve(1).await;
    node.await_authorizations(1).await;
    assert!(approved.elapsed() <= MOST_AN_APPROVAL_WAITS, "an approval waited {:?}", approved.elapsed());

    // A burst shares a checkpoint, so it waits no longer than one approval and adds no depth.
    let approved = Instant::now();
    node.approve(20).await;
    node.await_authorizations(20).await;
    assert!(approved.elapsed() <= MOST_AN_APPROVAL_WAITS, "a burst waited {:?}", approved.elapsed());

    assert_eq!(backlog_alerts(&mut alerts), 0, "a poster keeping up raises no backlog");
    assert_eq!(node.deepest, 1, "never more than one signed checkpoint awaits finality");
    node.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_stalled_poster_raises_the_backlog_gains_no_queue_and_pays_once_it_is_back() {
    let (_guard, mut alerts) = capture_alerts();
    let mut node = SlowNode::start("slow-poster-stalled").await;
    node.run_for(FINALITY * 2).await;

    // The bound runs from the hold, begun at most a finality delay before the stall.
    node.chain.stall(true);
    let signed = node.signed.len();
    node.run_for(STALLED_AFTER - FINALITY - RETRY).await;
    assert_eq!(backlog_alerts(&mut alerts), 0, "a stall inside the bound is not yet a backlog");
    node.run_for(FINALITY + RETRY * 2).await;
    assert!(backlog_alerts(&mut alerts) > 0, "the stalled poster raises the backlog");

    // An approval made during the stall waits for the chain, without signing past it.
    node.approve(1).await;
    node.run_for(FINALITY).await;
    assert_eq!(node.authorized, 0, "nothing authorizes while the chain is down");
    assert!(node.signed.len() - signed <= 1, "a stalled poster gains no queue");
    assert_eq!(node.deepest, 1);

    node.chain.stall(false);
    let resumed = Instant::now();
    node.await_authorizations(1).await;
    assert!(resumed.elapsed() <= MOST_AN_APPROVAL_WAITS, "the approval waited {:?}", resumed.elapsed());
    node.stop().await;
}

// ── Helpers ──

/// How many `checkpoint_backlog` alerts were raised since the last call.
fn backlog_alerts(alerts: &mut mpsc::UnboundedReceiver<Alert>) -> usize {
    std::iter::from_fn(|| alerts.try_recv().ok()).filter(|alert| alert["alert"] == "checkpoint_backlog").count()
}

/// The vault: what it accepted, each with when it becomes final, and whether its endpoint answers.
#[derive(Clone)]
struct SlowChain {
    signer: AccountId,
    started: Instant,
    accepted: Arc<Mutex<Vec<(Instant, u64, String)>>>,
    stalled: Arc<AtomicBool>,
}

impl SlowChain {
    /// Stops or restarts the endpoint's answers.
    fn stall(&self, stalled: bool) {
        self.stalled.store(stalled, Ordering::SeqCst);
    }

    fn accepted(&self) -> MutexGuard<'_, Vec<(Instant, u64, String)>> {
        self.accepted.lock().unwrap()
    }

    /// The newest accepted checkpoint at `view`, now.
    fn checkpoint(&self, view: View) -> Option<(u64, String)> {
        let now = Instant::now();
        let accepted = self.accepted();
        let visible = accepted.iter().rev().find(|(at, ..)| view == View::Latest || *at <= now);
        visible.map(|(_, seq, root)| (*seq, root.clone()))
    }

    /// One block a second of paused time, the finalized one `FINALITY` behind.
    fn height(&self, view: View) -> u64 {
        let finalized = self.started.elapsed().as_secs() + 1;
        if view == View::Latest { finalized + FINALITY.as_secs() } else { finalized }
    }
}

impl CheckpointChain for SlowChain {
    async fn state(&self, view: View) -> anyhow::Result<VaultState> {
        Ok(self.observe(view, &[]).await?.state)
    }

    async fn observe(&self, view: View, approvals: &[ApprovalId]) -> anyhow::Result<Observation> {
        anyhow::ensure!(!self.stalled.load(Ordering::SeqCst), "the endpoint is down");
        let (checkpoint, height) = (self.checkpoint(view), self.height(view));
        let funded = Fixed::from_int(DEPOSIT);
        Ok(Observation {
            view,
            state: VaultState {
                height,
                signer: self.signer,
                checkpoint: checkpoint.clone(),
                balance: funded,
                deposits: 1,
                deposited: funded,
                distributions_paid: Fixed::ZERO,
            },
            lifecycle: Some(LifecycleEvidence {
                height,
                block_hash: alloy::primitives::B256::left_padding_from(&height.to_be_bytes()),
                signer: self.signer,
                signer_epoch: 1,
                withdrawals_paused: false,
                total_withdrawn: Fixed::ZERO,
                checkpoint_seq: checkpoint.as_ref().map(|(seq, _)| *seq),
                checkpoint_root: checkpoint.map(|(_, root)| root),
                approvals: unclaimed(approvals),
            }),
        })
    }

    async fn claims(&self, _: &LifecycleEvidence, approvals: &[ApprovalId]) -> anyhow::Result<Vec<ApprovalStatus>> {
        Ok(unclaimed(approvals))
    }

    /// Takes a child of the latest checkpoint once that is final, as the poster's relay does.
    async fn post(&self, cp: &Checkpoint) -> anyhow::Result<()> {
        let (latest, finalized) = (self.checkpoint(View::Latest), self.checkpoint(View::Finalized));
        anyhow::ensure!(latest == finalized, "previous checkpoint submission is still pending finality");
        anyhow::ensure!(cp.parent.unwrap().seq == latest.map_or(0, |(seq, _)| seq), "signed parent differs");
        self.accepted().push((Instant::now() + FINALITY, cp.seq, cp.root.clone()));
        Ok(())
    }
}

fn unclaimed(approvals: &[ApprovalId]) -> Vec<ApprovalStatus> {
    approvals.iter().map(|approval| ApprovalStatus { approval: *approval, claimed: false }).collect()
}

/// A node's sequencer and poster over `SlowChain`, driven one command per `STEP` of paused time.
struct SlowNode {
    chain: SlowChain,
    clock: Arc<TestClock>,
    tx: mpsc::Sender<Request>,
    stream: broadcast::Receiver<Arc<StreamMsg>>,
    withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>,
    /// The checkpoints signed since the bootstrap, oldest first.
    signed: Vec<u64>,
    authorized: usize,
    deepest: usize,
    sequencer: JoinHandle<anyhow::Result<()>>,
    poster: JoinHandle<anyhow::Result<()>>,
}

impl SlowNode {
    /// Bootstraps a funded account, has the vault accept the bootstrap's newest checkpoint as final,
    /// and starts both tasks.
    async fn start(name: &str) -> Self {
        let dir = scratch_dir(name);
        let (sink, mut queued) = mpsc::channel(64);
        let (withdrawals_tx, withdrawals) = mpsc::unbounded_channel();
        let clock = Arc::new(TestClock::default());
        let registry = Registry::default();
        let approvals = ApprovalRegistry::default();
        let mut genesis = dev_config().genesis;
        genesis.house_capital = Fixed::ZERO;
        let cfg = crate::sequencer::SequencerConfig {
            clock: clock.clone(),
            checkpoints: Some(sink),
            checkpoint_every: CHECKPOINT_EVERY,
            checkpoint_registry: Some(registry.clone()),
            checkpoint_stalled_after: STALLED_AFTER,
            approval_registry: Some(approvals.clone()),
            genesis: crate::sequencer::genesis_commands(&genesis),
            ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
        };
        let key = Arc::new(EnclaveKey::test_key());
        let signer = AccountId(key.address());
        let (stream_tx, _) = broadcast::channel(4096);
        let mut seq = Sequencer::recover(cfg, key, stream_tx.clone(), withdrawals_tx).unwrap();
        seq.activate_protocol_at(engine_core::protocol::ProtocolVersion::V23).unwrap();
        seq.genesis(&genesis).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        fund(&mut seq);
        let anchor: Work = std::iter::from_fn(|| queued.try_recv().ok()).last().expect("the bootstrap's checkpoints");
        seq.checkpoint_status(Some(anchor.checkpoint.seq), Some(Health::Healthy)).unwrap();
        let chain = SlowChain {
            signer,
            started: Instant::now(),
            accepted: Arc::new(Mutex::new(vec![(
                Instant::now(),
                anchor.checkpoint.seq,
                anchor.checkpoint.root.clone(),
            )])),
            stalled: Default::default(),
        };
        let poster = Poster {
            chain: chain.clone(),
            registry: Some(registry.clone()),
            domain: CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) },
            signer,
            retry: RETRY,
            pending_deposits_critical_after: Duration::from_secs(24 * 60 * 60),
            post_withheld: Default::default(),
            relayer_gas: None,
        };
        let (tx, rx) = mpsc::channel(64);
        Self {
            chain,
            clock,
            stream: stream_tx.subscribe(),
            withdrawals,
            signed: Vec::new(),
            authorized: 0,
            deepest: 0,
            sequencer: tokio::spawn(seq.run(rx)),
            poster: tokio::spawn(poster.run_with_approvals(queued, tx.clone(), vec![anchor], approvals)),
            tx,
        }
    }

    /// Sequences a command every `STEP` for `span` of paused time.
    async fn run_for(&mut self, span: Duration) {
        let until = Instant::now() + span;
        while Instant::now() < until {
            self.step().await;
        }
    }

    /// One `STEP`: the sequencer's clock, a time sample, a command, and what they left.
    async fn step(&mut self) {
        sleep(STEP).await;
        self.clock.advance(STEP);
        self.tx.send(Request::TimeSample(sample_around(self.clock.as_ref()))).await.unwrap();
        self.submit(Command::Tick).await;
        while let Ok(msg) = self.stream.try_recv() {
            if let StreamBody::Checkpoint(checkpoint) = &msg.body {
                self.signed.push(checkpoint.seq);
            }
        }
        let finalized = self.chain.checkpoint(View::Finalized).map_or(0, |(seq, _)| seq);
        self.deepest = self.deepest.max(self.signed.iter().filter(|seq| **seq > finalized).count());
        while self.withdrawals.try_recv().is_ok() {
            self.authorized += 1;
        }
    }

    /// Sequences `cmd` and returns its events.
    async fn submit(&self, cmd: Command) -> Vec<Event> {
        let (reply, answer) = oneshot::channel();
        self.tx.send(Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
        answer.await.expect("trusted time admits the command")
    }

    /// Approves `count` withdrawals at once.
    async fn approve(&mut self, count: usize) {
        for _ in 0..count {
            let events = self.submit(withdraw(1)).await;
            assert!(events.iter().any(|event| matches!(event, Event::WithdrawalApproved { .. })), "{events:?}");
        }
    }

    /// Runs until `count` more withdrawals are authorized.
    async fn await_authorizations(&mut self, count: usize) {
        let target = self.authorized + count;
        let deadline = Instant::now() + FINALITY * 200;
        while self.authorized < target {
            self.step().await;
            assert!(Instant::now() < deadline, "the withdrawals never authorized: {}", self.authorized);
        }
    }

    /// Ends both tasks.
    async fn stop(self) {
        self.poster.abort();
        let _ = self.poster.await;
        drop(self.tx);
        self.sequencer.await.unwrap().unwrap();
    }
}

/// Credits `DEPOSIT` to `TRADER` under a canonical scan and checkpoints it, as a deposit reaching
/// finality does.
fn fund(seq: &mut Sequencer) {
    let receipt = DepositReceipt {
        block: 1,
        block_hash: B256::repeat_byte(1),
        tx_hash: B256::repeat_byte(2),
        log_index: 0,
        transaction_deposit_index: 0,
        deposit_id: 0,
        beneficiary: DepositBeneficiary::Trader { account: TRADER },
        amount: Fixed::from_int(DEPOSIT),
    };
    seq.sequence(Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: receipt.clone() }))).unwrap();
    let observed_at_seq = seq.engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
    seq.sequence(Command::CustodyDeposit(Box::new(DepositUpdate::Finalise { receipt, observed_at_seq }))).unwrap();
    seq.sequence(Command::CustodyFinalised(CustodyCheckpoint {
        block: 1,
        block_hash: B256::repeat_byte(1),
        deposit_count: 1,
    }))
    .unwrap();
    seq.checkpoint().unwrap();
}
