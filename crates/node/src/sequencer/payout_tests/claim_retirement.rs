//! Finalized claim acknowledgement survives pauses in authorization creation.

use super::*;

use std::sync::atomic::{AtomicBool, Ordering};

use crate::checkpoints::{CheckpointChain, Observation, Poster, VaultState, View, Work};
use crate::sequencer::Request;

#[tokio::test]
async fn recovered_claims_retire_without_trusted_time_or_healthy_reconciliation() {
    for unavailable_time in [false, true] {
        let mut node = Node::new(if unavailable_time { "claim-no-time" } else { "claim-unhealthy" });
        let first = node.approve();
        let later = AccountId([1; 20]);
        node.seq
            .sequence(Command::Deposit {
                account: later,
                amount: Fixed::from_int(20),
                chain_ref: "claim-following-deposit".into(),
            })
            .unwrap()
            .unwrap();
        node.seq.sequence(Command::Withdraw { account: later, amount: Fixed::ONE, auth: None }).unwrap().unwrap();
        let last = log_entries(&node.dir)
            .into_iter()
            .rev()
            .find_map(|entry| match entry {
                LogEntry::Checkpoint(cp) => Some(cp),
                _ => None,
            })
            .unwrap();
        let balances = node.seq.engine.accounts().clone();
        let key = node.seq.key.clone();
        let log =
            Sequencer::read_log_with_trust(&node.dir, Some(node.domain()), Some(AccountId(key.address())), |_| {})
                .unwrap();
        let cfg = SequencerConfig {
            checkpoint_parent: crate::log::CheckpointParent {
                has_checkpoint: true,
                seq: last.seq,
                root: last.root.parse().unwrap(),
            },
            ..node.config()
        };
        let (withdrawals, mut signed) = mpsc::unbounded_channel();
        let mut recovered = Sequencer::recover_from(log, cfg, key, broadcast::channel(128).0, withdrawals).unwrap();
        recovered.checkpoint_status(Some(last.seq), unavailable_time).unwrap();
        if !unavailable_time {
            recovered.trust(sample_around(node.clock.as_ref())).unwrap();
        }
        let registry = recovered.gate.as_ref().unwrap().approvals.as_ref().unwrap().clone();
        // The fixture credits deposits without vault references; the mock vault holds them as one
        // and its claim is final, so the work is canonical despite the unscanned ledger receipts.
        let work = Work {
            deposits: 1,
            distributed: Default::default(),
            coverage: crate::checkpoints::DepositCoverage::Canonical,
            ..Work::capture(last.clone(), node.seq.accounting, &node.seq.engine)
        };
        let healthy = Arc::new(AtomicBool::new(unavailable_time));
        let chain = ClaimChain {
            work: work.clone(),
            evidence: node.evidence(&last),
            paid: ApprovalId { account: TRADER, nonce: 1, approved_seq: first.seq },
            healthy: healthy.clone(),
        };
        let poster = Poster {
            chain,
            registry: None,
            domain: node.domain(),
            signer: AccountId(node.seq.key.address()),
            retry: Duration::from_millis(5),
            pending_deposits_critical_after: Duration::from_secs(10),
        };
        let (tx, rx) = mpsc::channel(16);
        let task = tokio::spawn(recovered.run(rx));
        let (work_tx, work_rx) = mpsc::channel(2);
        work_tx.send(work).await.unwrap();
        let worker = tokio::spawn(poster.run_with_approvals(work_rx, tx.clone(), Vec::new(), registry.clone()));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let pending = registry.lock().unwrap().contains_key(&(TRADER, 1));
                if !pending {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual worker/actor must acknowledge claims while signing is paused");
        assert!(signed.try_recv().is_err());
        healthy.store(true, Ordering::SeqCst);
        tx.send(Request::TimeSample(sample_around(node.clock.as_ref()))).await.unwrap();
        let next = tokio::time::timeout(Duration::from_secs(5), signed.recv()).await.unwrap().unwrap();
        assert_eq!(next.account, later, "paid head cannot strand actual worker/actor delivery");
        assert!(signed.try_recv().is_err());
        drop(work_tx);
        tokio::time::timeout(Duration::from_secs(5), worker).await.unwrap().unwrap().unwrap();
        drop(tx);
        task.await.unwrap().unwrap();
        let replayed = Sequencer::read_log_with_trust(
            &node.dir,
            Some(node.domain()),
            Some(AccountId(node.seq.key.address())),
            |_| {},
        )
        .unwrap();
        assert_eq!(replayed.replayed.engine.accounts(), &balances, "claim retirement changes delivery state only");
    }
}

/// Finalized mock observations exercise the real poster/actor handoff without external IO.
struct ClaimChain {
    work: Work,
    evidence: LifecycleEvidence,
    paid: ApprovalId,
    healthy: Arc<AtomicBool>,
}

impl ClaimChain {
    /// Returns one internally coherent adapter observation and the requested bounded nonce page.
    fn observation(&self, view: View, approvals: &[ApprovalId]) -> Observation {
        let healthy = self.healthy.load(Ordering::SeqCst);
        let height = if healthy { 20 } else { 10 };
        let mut lifecycle = self.evidence.clone();
        lifecycle.height = height;
        lifecycle.block_hash = B256::from([height as u8; 32]);
        lifecycle.total_withdrawn = Fixed::from_int(40);
        lifecycle.approvals = approvals
            .iter()
            .map(|approval| ApprovalStatus { approval: *approval, claimed: *approval == self.paid })
            .collect();
        let state = VaultState {
            height,
            signer: lifecycle.signer,
            checkpoint: Some((self.work.checkpoint.seq, self.work.checkpoint.root.clone())),
            // A shortfall makes custody unhealthy; a surplus would not.
            balance: self.work.liabilities - lifecycle.total_withdrawn - if healthy { Fixed::ZERO } else { Fixed::ONE },
            deposits: self.work.deposits,
            deposited: self.work.liabilities,
            distributions_paid: engine_core::Fixed::ZERO,
        };
        Observation { view, state, lifecycle: Some(lifecycle) }
    }
}

impl CheckpointChain for ClaimChain {
    async fn state(&self, view: View) -> anyhow::Result<VaultState> {
        Ok(self.observation(view, &[]).state)
    }

    async fn observe(&self, view: View, approvals: &[ApprovalId]) -> anyhow::Result<Observation> {
        assert!(approvals.len() <= crate::checkpoints::APPROVAL_PAGE_SIZE);
        Ok(self.observation(view, approvals))
    }

    async fn accepted_checkpoints(
        &self,
        _: &Observation,
        sequences: &[u64],
    ) -> anyhow::Result<Vec<AcceptedCheckpoint>> {
        sequences
            .iter()
            .map(|seq| {
                anyhow::ensure!(*seq == self.work.checkpoint.seq, "mock observed an unknown accepted checkpoint");
                Ok(AcceptedCheckpoint {
                    seq: *seq,
                    root: self.work.checkpoint.root.clone(),
                    signer: self.evidence.signer,
                    signer_epoch: self.evidence.signer_epoch,
                })
            })
            .collect()
    }

    async fn post(&self, _: &Checkpoint) -> anyhow::Result<()> {
        unreachable!("fixture checkpoint is already finalized")
    }
}
