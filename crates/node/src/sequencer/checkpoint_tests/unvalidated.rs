//! A missing collateral comparison cannot release startup or divergence signing holds.

use super::*;
use std::time::Duration;

use crate::checkpoints::{CheckpointChain, DepositCoverage, Poster, VaultState, View, Work};
use crate::log::Checkpoint;
use crate::sequencer::Request;

#[derive(Clone, Copy, Debug)]
pub(super) enum Comparison {
    MissingFinalized,
    OlderFinalized,
    UnprovenCoverage,
    ValidatedCatchup,
}

struct Chain {
    latest: VaultState,
    finalized: VaultState,
    credited: Fixed,
}

impl CheckpointChain for Chain {
    async fn state(&self, view: View) -> anyhow::Result<VaultState> {
        Ok(if view == View::Latest { self.latest.clone() } else { self.finalized.clone() })
    }
    async fn deposited_through(&self, _: &crate::checkpoints::Observation, _: u64) -> anyhow::Result<Fixed> {
        Ok(self.credited)
    }
    async fn post(&self, _: &Checkpoint) -> anyhow::Result<()> {
        panic!("the test checkpoint is already included")
    }
}

#[tokio::test]
async fn unknown_collateral_observations_preserve_startup_and_divergence_holds() {
    for comparison in [Comparison::MissingFinalized, Comparison::OlderFinalized, Comparison::UnprovenCoverage] {
        let (mut seq, mut queued, work) = bootstrap(&format!("unknown-collateral-{comparison:?}"));
        for before in [None, Some(Health::Diverged)] {
            seq.gate.as_mut().unwrap().health = before;
            let (posted, health) = worker_status(work.clone(), comparison).await;
            assert_eq!(health, None, "{comparison:?} has no collateral verdict");
            seq.checkpoint_status(posted, health).unwrap();
            assert_eq!(seq.gate.as_ref().unwrap().health, before);
            seq.sequence(Command::Tick).unwrap();
            assert!(queued.try_recv().is_err(), "{comparison:?} cannot release {before:?}");
            seq.checkpoint_status(posted, Some(Health::Unvalidated)).unwrap();
            assert_eq!(seq.gate.as_ref().unwrap().health, before, "direct callers cannot erase a verdict either");
        }
    }
}

#[tokio::test]
async fn compared_pending_collateral_can_release_a_signing_hold() {
    let (mut seq, mut queued, work) = bootstrap("compared-pending-collateral");
    for before in [None, Some(Health::Diverged)] {
        seq.gate.as_mut().unwrap().health = before;
        let (posted, health) = worker_status(work.clone(), Comparison::ValidatedCatchup).await;
        assert_eq!(health, Some(Health::Pending), "Pending requires a completed ledger comparison");
        seq.checkpoint_status(posted, health).unwrap();
        seq.sequence(Command::Tick).unwrap();
        assert!(queued.try_recv().is_ok(), "a compared catch-up state may sign its clearing checkpoint");
    }
}

fn bootstrap(name: &str) -> (Sequencer, mpsc::Receiver<Work>, Work) {
    let dir = scratch_dir(name);
    let (sink, mut queued) = mpsc::channel(64);
    let mut genesis = dev_config().genesis;
    genesis.house_capital = Fixed::ZERO;
    let cfg = crate::sequencer::SequencerConfig {
        checkpoints: Some(sink),
        checkpoint_every: 1,
        genesis: crate::sequencer::genesis_commands(&genesis),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(
        cfg,
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(128).0,
        mpsc::unbounded_channel().0,
    )
    .unwrap();
    seq.activate_protocol_at(engine_core::protocol::ProtocolVersion::V18).unwrap();
    seq.genesis(&genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    seq.sequence(deposit(10, "0xhealth:deposit")).unwrap();
    assert_eq!(seq.gate.as_ref().unwrap().health, Some(Health::Pending), "fresh deployment bootstrap remains possible");
    let mut last = None;
    while let Ok(work) = queued.try_recv() {
        last = Some(work);
    }
    (seq, queued, last.expect("bootstrap signs before its first comparison because no finalized anchor exists"))
}

pub(super) async fn worker_status(mut work: Work, comparison: Comparison) -> (Option<u64>, Option<Health>) {
    let domain = work.checkpoint.domain.unwrap();
    let signer = work.checkpoint.signer.parse().unwrap();
    let mut latest = VaultState {
        height: 8,
        signer,
        checkpoint: Some((work.checkpoint.seq, work.checkpoint.root.clone())),
        balance: work.liabilities,
        deposits: work.deposits,
        deposited: work.liabilities,
        distributions_paid: Fixed::ZERO,
    };
    let mut finalized = latest.clone();
    match comparison {
        Comparison::MissingFinalized => {
            finalized.height = 7;
            finalized.checkpoint = None;
        }
        Comparison::OlderFinalized => {
            finalized.height = 7;
            finalized.checkpoint = Some((work.checkpoint.seq - 1, format!("0x{}", "11".repeat(32))))
        }
        Comparison::UnprovenCoverage => work.coverage = DepositCoverage::AwaitingCanonicalScan,
        Comparison::ValidatedCatchup => {
            finalized.deposits += 1;
            finalized.deposited += Fixed::ONE;
            finalized.balance += Fixed::ONE;
            latest = finalized.clone();
        }
    }
    let poster = Poster {
        chain: Chain { latest, finalized, credited: work.liabilities },
        registry: None,
        domain,
        signer,
        retry: Duration::from_millis(1),
        pending_deposits_critical_after: Duration::ZERO,
    };
    let (work_tx, work_rx) = mpsc::channel(4);
    let (tx, mut rx) = mpsc::channel(4);
    work_tx.send(work).await.unwrap();
    let task = tokio::spawn(poster.run(work_rx, tx));
    let Some(Request::CheckpointStatus { posted, health, .. }) = rx.recv().await else {
        panic!("worker status missing")
    };
    drop(work_tx);
    while rx.recv().await.is_some() {}
    task.await.unwrap().unwrap();
    (posted, health)
}
