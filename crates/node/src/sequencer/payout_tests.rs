//! Payout release, durable redelivery and signer/expiry reissues on real approval proofs.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use tokio::sync::{broadcast, mpsc, watch};

use crate::checkpoints::{AcceptedCheckpoint, ApprovalId, ApprovalStatus, LifecycleEvidence, SignerHistory};
use crate::clock::test_clock::TestClock;
use crate::log::{Checkpoint, CheckpointDomain, LogEntry, Manifest};
use crate::sequencer::{AccountPayouts, Reissue, Sequencer, SequencerConfig, SignedWithdrawal};
use crate::tee::EnclaveKey;
use crate::test_support::{
    TRADER, deposit, dev_config, log_entries, sample_around, scratch_dir, sequencer_config, withdraw,
};

#[test]
fn a_release_batch_syncs_the_log_once() {
    let mut node = Node::new("payout-batch-sync");
    // Several unpaid approvals, each with its own approval checkpoint, released together.
    let accounts = [AccountId([1; 20]), AccountId([2; 20]), AccountId([3; 20]), AccountId([4; 20])];
    let mut observed = Vec::new();
    for (i, account) in accounts.iter().enumerate() {
        node.seq
            .sequence(Command::Deposit {
                account: *account,
                amount: Fixed::from_int(100),
                chain_ref: format!("batch-dep-{i}"),
            })
            .unwrap()
            .unwrap();
        let events = node
            .seq
            .sequence(Command::Withdraw { account: *account, amount: Fixed::from_int(10), auth: None })
            .unwrap()
            .unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::WithdrawalApproved { account: a, nonce: 1, .. } if a == account))
        );
        let cp = node.last_checkpoint();
        observed.push(ApprovalStatus {
            approval: ApprovalId { account: *account, nonce: 1, approved_seq: cp.seq },
            claimed: false,
        });
    }
    let last = node.last_checkpoint();
    node.seq.checkpoint_status(Some(last.seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    let mut evidence = node.evidence(&last);
    evidence.approvals = observed;
    let before = node.seq.log.sync_count();
    node.seq.release_payouts(&evidence).unwrap();
    assert_eq!(
        node.seq.log.sync_count() - before,
        1,
        "one log sync covers the whole released batch, however many authorizations it holds"
    );
    node.seq.flush().unwrap();
    for account in accounts {
        assert_eq!(node.signed.try_recv().unwrap().account, account);
    }
    assert!(node.signed.try_recv().is_err());
}

#[test]
fn typed_payout_waits_for_finalized_unpaid_evidence_and_publishes_only_after_sync() {
    let mut node = Node::new("payout-durable");
    let cp = node.approve();
    node.seq.flush().unwrap();
    assert!(node.signed.try_recv().is_err());
    assert!(!log_entries(&node.dir).iter().any(|entry| matches!(entry, LogEntry::Withdrawal(_))));
    let mut evidence = node.evidence(&cp);
    evidence.approvals.clear();
    node.seq.release_payouts(&evidence).unwrap();
    assert!(node.signed.try_recv().is_err(), "absence is not evidence that a nonce is unpaid");
    let mut behind = node.evidence(&cp);
    behind.checkpoint_seq = Some(cp.seq - 1);
    node.seq.release_payouts(&behind).unwrap();
    node.seq.checkpoint_status(Some(cp.seq), Some(crate::checkpoints::Health::Diverged)).unwrap();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    assert!(
        !log_entries(&node.dir).iter().any(|entry| matches!(entry, LogEntry::Withdrawal(_))),
        "an unfinalized prefix or unhealthy books cannot authorize signing"
    );
    node.seq.checkpoint_status(Some(cp.seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    assert!(node.signed.try_recv().is_err(), "custody cannot see an unflushed outbox");
    let durable = log_entries(&node.dir)
        .into_iter()
        .find_map(|entry| match entry {
            LogEntry::Withdrawal(withdrawal) => Some(withdrawal),
            _ => None,
        })
        .expect("authorization is synced before publication");
    assert_eq!(durable.amount, Fixed::from_int(40));
    assert_eq!(durable.approved_seq, Some(cp.seq));
    assert_eq!(durable.authorization.as_ref().unwrap().checkpoint_root.to_string(), cp.root);
    node.seq.flush().unwrap();
    assert_eq!(node.signed.try_recv().unwrap(), durable);
    crate::withdrawals::verify(&durable, node.domain(), AccountId(node.seq.key.address())).unwrap();
}

#[test]
fn out_of_order_claim_pages_cannot_sign_a_later_approval_first() {
    let mut node = Node::new("payout-claim-page-fifo");
    let first = node.approve();
    let later = AccountId([1; 20]);
    node.seq
        .sequence(Command::Deposit { account: later, amount: Fixed::from_int(100), chain_ref: "payout-later".into() })
        .unwrap()
        .unwrap();
    node.seq.sequence(Command::Withdraw { account: later, amount: Fixed::from_int(10), auth: None }).unwrap().unwrap();
    let last = log_entries(&node.dir)
        .into_iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(cp) => Some(cp),
            _ => None,
        })
        .unwrap();
    let mut evidence = node.evidence(&last);
    evidence.approvals = vec![ApprovalStatus {
        approval: ApprovalId { account: later, nonce: 1, approved_seq: last.seq },
        claimed: false,
    }];
    node.seq.release_payouts(&evidence).unwrap();
    node.seq.flush().unwrap();
    assert!(node.signed.try_recv().is_err(), "a later account's page cannot pass an unobserved earlier approval");
    evidence.approvals = vec![ApprovalStatus {
        approval: ApprovalId { account: TRADER, nonce: 1, approved_seq: first.seq },
        claimed: false,
    }];
    node.seq.release_payouts(&evidence).unwrap();
    node.seq.flush().unwrap();
    assert_eq!(node.signed.try_recv().unwrap().account, TRADER);
    assert_eq!(node.signed.try_recv().unwrap().account, later);
    assert!(node.signed.try_recv().is_err());
}

#[test]
fn restart_keeps_a_signed_unpaid_approval_without_debiting_it_or_signing_it_again() {
    let mut node = Node::new("payout-restart");
    let cp = node.approve();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    let issued = node.signed.try_recv().unwrap();
    let before = node.seq.engine.accounts()[&TRADER].balance;
    let mut recovered = node.restart(&cp);
    assert!(recovered.signed.try_recv().is_err(), "fresh authenticated time and custody evidence are required");
    recovered.seq.release_payouts(&recovered.evidence(&cp)).unwrap();
    recovered.seq.flush().unwrap();
    assert!(recovered.signed.try_recv().is_err(), "the durable authorization stands; nothing is signed again");
    let payout = &recovered.seq.gate.as_ref().unwrap().payouts[&(TRADER, 1)];
    let (account, nonce, amount) = (payout.approval.account, payout.approval.nonce, payout.approval.amount);
    assert_eq!((account, nonce, amount), (TRADER, issued.nonce, issued.amount));
    assert_eq!(payout.released, issued.authorization.map(|auth| (auth.signer_epoch, auth.expires_at)));
    assert_eq!(recovered.seq.engine.accounts()[&TRADER].balance, before);
    assert_eq!(recovered.seq.engine.withdrawal_approvals().count(), 1);
}

#[test]
fn signed_payout_history_cannot_disable_its_checkpoint_gate_on_restart() {
    let mut node = Node::new("payout-gate-cannot-disable");
    let cp = node.approve();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    assert!(node.signed.try_recv().is_ok());
    let manifest_path = node.dir.join("manifest.jsonl");
    let mut manifest = std::fs::read(&manifest_path).unwrap();
    manifest.extend_from_slice(b"{partial");
    std::fs::write(&manifest_path, &manifest).unwrap();
    let before = log_entries(&node.dir);
    let result = Sequencer::recover(
        sequencer_config(&node.dir, watch::channel(Manifest::default()).0),
        node.seq.key.clone(),
        broadcast::channel(128).0,
        mpsc::unbounded_channel().0,
    );
    assert!(result.err().unwrap().to_string().contains("checkpoint-gated withdrawals require the checkpoint service"));
    assert_eq!(log_entries(&node.dir), before, "a refused configuration must not append or repair history");
    assert_eq!(std::fs::read(manifest_path).unwrap(), manifest, "refusal must precede even partial-manifest repair");
}

#[test]
fn expiry_alone_never_reissues_and_authenticated_claim_stops_delivery() {
    let mut node = Node::new("payout-expiry");
    let cp = node.approve();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    node.signed.try_recv().unwrap();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    assert!(node.signed.try_recv().is_err(), "an unexpired authorization is not repeatedly appended");
    for _ in 0..3 {
        node.clock.advance(Duration::from_secs(crate::withdrawals::LIFETIME_SECONDS));
        node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
        node.seq.release_payouts(&node.evidence(&cp)).unwrap();
        node.seq.flush().unwrap();
        assert!(node.signed.try_recv().is_err(), "an expired authorization is not reissued on a schedule");
    }
    let mut paid = node.evidence(&cp);
    paid.approvals[0].claimed = true;
    node.seq.release_payouts(&paid).unwrap();
    assert!(node.seq.gate.as_ref().unwrap().payouts.is_empty());
    assert!(node.seq.gate.as_ref().unwrap().approvals.as_ref().unwrap().lock().unwrap().is_empty());
    node.clock.advance(Duration::from_secs(crate::withdrawals::LIFETIME_SECONDS));
    node.seq.trust(sample_around(node.clock.as_ref())).unwrap();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    assert!(node.signed.try_recv().is_err());
}

#[test]
fn rotation_requires_accepted_historical_signers_and_reissues_with_the_current_epoch() {
    let mut node = Node::new("payout-rotation");
    let cp = node.approve();
    node.seq.release_payouts(&node.evidence(&cp)).unwrap();
    node.seq.flush().unwrap();
    let old = node.signed.try_recv().unwrap();
    let key = Arc::new(EnclaveKey::generate());
    let signer = AccountId(key.address());
    let entries = log_entries(&node.dir);
    let checkpoints = entries
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Checkpoint(cp) => Some(AcceptedCheckpoint {
                seq: cp.seq,
                root: cp.root.clone(),
                signer: cp.signer.parse().unwrap(),
                signer_epoch: 1,
            }),
            _ => None,
        })
        .collect();
    let epochs = BTreeMap::from([(1, AccountId(node.seq.key.address())), (2, signer)]);
    let authorities = SignerHistory { checkpoints, epochs: epochs.clone() };
    let parent = crate::log::CheckpointParent { has_checkpoint: true, seq: cp.seq, root: cp.root.parse().unwrap() };
    let domain = node.domain();
    assert!(Sequencer::read_log_with_trust(&node.dir, Some(domain), Some(signer), |_| {}).is_err());
    let log = Sequencer::read_log_with_authorities(
        &node.dir,
        Some(domain),
        Some(signer),
        Some(parent),
        authorities,
        |_| {},
        None,
    )
    .unwrap();
    let cfg = SequencerConfig { checkpoint_parent: parent, ..node.config() };
    let (withdrawals, mut signed) = mpsc::unbounded_channel();
    let mut recovered = Sequencer::recover_from(log, cfg, key, broadcast::channel(128).0, withdrawals).unwrap();
    recovered.trust(sample_around(node.clock.as_ref())).unwrap();
    recovered.checkpoint_status(Some(cp.seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    let mut evidence = node.evidence(&cp);
    evidence.signer = signer;
    evidence.signer_epoch = 2;
    recovered.release_payouts(&evidence).unwrap();
    recovered.flush().unwrap();
    let reissued = signed.try_recv().unwrap();
    assert_eq!(reissued.nonce, old.nonce);
    assert_eq!(reissued.authorization.as_ref().unwrap().signer_epoch, 2);
    crate::withdrawals::verify(&reissued, domain, signer).unwrap();
    assert!(crate::withdrawals::verify(&old, domain, signer).is_err());
    let next_key = Arc::new(EnclaveKey::generate());
    let next_signer = AccountId(next_key.address());
    drop(recovered);
    let mut history = SignerHistory {
        checkpoints: log_entries(&node.dir)
            .into_iter()
            .filter_map(|entry| match entry {
                LogEntry::Checkpoint(cp) => Some(AcceptedCheckpoint {
                    seq: cp.seq,
                    root: cp.root,
                    signer: cp.signer.parse().unwrap(),
                    signer_epoch: 1,
                }),
                _ => None,
            })
            .collect(),
        epochs,
    };
    history.epochs.insert(3, next_signer);
    let log = Sequencer::read_log_with_authorities(
        &node.dir,
        Some(domain),
        Some(next_signer),
        Some(parent),
        history,
        |_| {},
        None,
    )
    .expect("epoch2 reissue on epoch1 root survives another rotation without an epoch2 checkpoint");
    let (withdrawals, mut signed) = mpsc::unbounded_channel();
    let cfg = SequencerConfig { checkpoint_parent: parent, ..node.config() };
    let mut recovered = Sequencer::recover_from(log, cfg, next_key, broadcast::channel(128).0, withdrawals).unwrap();
    recovered.trust(sample_around(node.clock.as_ref())).unwrap();
    recovered.checkpoint_status(Some(cp.seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    evidence.signer = next_signer;
    evidence.signer_epoch = 3;
    recovered.release_payouts(&evidence).unwrap();
    recovered.flush().unwrap();
    let next = signed.try_recv().unwrap();
    assert_eq!(next.nonce, old.nonce);
    assert_eq!(next.authorization.as_ref().unwrap().checkpoint_seq, cp.seq);
    assert_eq!(next.authorization.as_ref().unwrap().signer_epoch, 3);
    crate::withdrawals::verify(&next, domain, next_signer).unwrap();
    assert!(crate::withdrawals::verify(&reissued, domain, next_signer).is_err());
}

struct Node {
    seq: Sequencer,
    clock: Arc<TestClock>,
    dir: PathBuf,
    work: mpsc::Sender<crate::checkpoints::Work>,
    _queued: mpsc::Receiver<crate::checkpoints::Work>,
    signed: mpsc::UnboundedReceiver<SignedWithdrawal>,
}

impl Node {
    fn new(name: &str) -> Self {
        let dir = scratch_dir(name);
        let clock = Arc::new(TestClock::default());
        let (work, queued) = mpsc::channel(64);
        let (withdrawals, signed) = mpsc::unbounded_channel();
        let cfg = SequencerConfig {
            clock: clock.clone(),
            checkpoints: Some(work.clone()),
            approval_registry: Some(Default::default()),
            ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
        };
        let mut seq =
            Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(128).0, withdrawals).unwrap();
        seq.activate_legacy_protocol().unwrap();
        seq.genesis(&dev_config().genesis).unwrap();
        seq.trust(sample_around(clock.as_ref())).unwrap();
        seq.sequence(crate::test_support::empty_custody_frontier()).unwrap();
        seq.checkpoint().unwrap();
        assert_eq!(seq.engine.protocol_version(), ProtocolVersion::V23);
        seq.checkpoint_status(seq.last_seq(), Some(crate::checkpoints::Health::Healthy)).unwrap();
        Self { seq, clock, dir, work, _queued: queued, signed }
    }

    /// Recovers the sequencer from the log it wrote, as a same-key restart does.
    fn restart(&self, cp: &Checkpoint) -> Node {
        let key = self.seq.key.clone();
        let cfg = SequencerConfig {
            checkpoint_parent: crate::log::CheckpointParent {
                has_checkpoint: true,
                seq: cp.seq,
                root: cp.root.parse().unwrap(),
            },
            ..self.config()
        };
        let (stream, _) = broadcast::channel(128);
        let (withdrawals, signed) = mpsc::unbounded_channel();
        // An unflushed stream can disappear while its signed log entry is already durable.
        let log = Sequencer::read_log_for_recovery(
            &self.dir,
            Some(self.domain()),
            Some(AccountId(key.address())),
            Some(cfg.checkpoint_parent),
            |_| {},
        )
        .unwrap();
        let mut seq = Sequencer::recover_from(log, cfg, key, stream, withdrawals).unwrap();
        seq.trust(sample_around(self.clock.as_ref())).unwrap();
        seq.checkpoint_status(Some(cp.seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
        let (work, queued) = mpsc::channel(64);
        Node { seq, clock: self.clock.clone(), dir: self.dir.clone(), work, _queued: queued, signed }
    }

    fn approve(&mut self) -> Checkpoint {
        self.seq.sequence(deposit(100, "payout-test-deposit")).unwrap().unwrap();
        let events = self.seq.sequence(withdraw(40)).unwrap().unwrap();
        assert!(events.contains(&Event::WithdrawalApproved { account: TRADER, amount: Fixed::from_int(40), nonce: 1 }));
        self.last_checkpoint()
    }

    fn last_checkpoint(&self) -> Checkpoint {
        log_entries(&self.dir)
            .into_iter()
            .rev()
            .find_map(|entry| match entry {
                LogEntry::Checkpoint(cp) => Some(cp),
                _ => None,
            })
            .unwrap()
    }

    fn evidence(&self, cp: &Checkpoint) -> LifecycleEvidence {
        LifecycleEvidence {
            height: 10,
            block_hash: B256::from([1; 32]),
            signer: AccountId(self.seq.key.address()),
            signer_epoch: 1,
            withdrawals_paused: false,
            total_withdrawn: Fixed::ZERO,
            checkpoint_seq: Some(cp.seq),
            checkpoint_root: Some(cp.root.clone()),
            approvals: vec![ApprovalStatus {
                approval: ApprovalId { account: TRADER, nonce: 1, approved_seq: cp.seq },
                claimed: false,
            }],
        }
    }

    fn domain(&self) -> CheckpointDomain {
        CheckpointDomain { chain_id: self.seq.chain_id, vault: self.seq.vault }
    }

    fn config(&self) -> SequencerConfig<'_> {
        SequencerConfig {
            clock: self.clock.clone(),
            checkpoints: Some(self.work.clone()),
            approval_registry: Some(Default::default()),
            ..sequencer_config(&self.dir, watch::channel(Manifest::default()).0)
        }
    }
}

mod backlog;
mod claim_retirement;
mod halt;
mod origin;
mod read;
mod reissue;
mod restart_bound;
