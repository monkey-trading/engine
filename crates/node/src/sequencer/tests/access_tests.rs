//! Current access envelopes keep reconciliation admission closed while recording policy refusals.

use std::sync::Arc;
use std::time::Duration;

use engine_core::Fixed;
use engine_core::access::{AccessEvidence, AccessReject, Jurisdiction, SanctionsStatus, Screening};
use engine_core::command::Command;
use engine_core::eip712;
use engine_core::event::{Event, Reject};
use engine_core::types::AccountId;
use tokio::sync::{broadcast, mpsc, watch};

use crate::checkpoints::{ApprovalId, ApprovalStatus, LifecycleEvidence};
use crate::clock::test_clock::TestClock;
use crate::log::{LogEntry, Manifest};
use crate::sequencer::{Reissue, ReissueScreening, Sequencer};
use crate::tee::EnclaveKey;
use crate::test_support::{access, dev_config, log_entries, sample_around, scratch_dir, sequencer_config};

const NOW: i64 = 1_792_504_800_000;

#[test]
fn access_withdrawals_record_refusals_and_recover_only_checkpoint_confirmed_approvals() {
    let dir = scratch_dir("access-reconciliation-gate");
    let (work, _queued) = mpsc::channel(64);
    let (withdrawals, mut signed) = mpsc::unbounded_channel();
    let clock = Arc::new(TestClock::at(NOW));
    let config = || crate::sequencer::SequencerConfig {
        clock: clock.clone(),
        checkpoints: Some(work.clone()),
        access_policy: Some(access::policy()),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq =
        Sequencer::recover(config(), Arc::new(EnclaveKey::test_key()), broadcast::channel(128).0, withdrawals.clone())
            .unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(crate::test_support::empty_custody_frontier()).unwrap();
    let account = access::account(1);
    let declaration = access::declaration(&seq.engine, 1);
    let declared = seq.sequence(declaration).unwrap().unwrap();
    assert!(matches!(declared.as_slice(), [Event::EligibilityAttested { .. }]));
    let deposit =
        access::funding(&seq.engine, Some(account), Some(account), Fixed::from_int(100), "0xaccess-gate-funding:1");
    let funded = seq.sequence(deposit).unwrap().unwrap();
    assert_eq!(funded, [Event::Deposited { account, amount: Fixed::from_int(100) }]);
    for (nonce, jurisdiction, expected) in [
        (1, Jurisdiction::Allowed, Reject::PayoutUnavailable),
        (2, Jurisdiction::UnitedStates, Reject::Access { reason: engine_core::access::AccessReject::UnitedStates }),
    ] {
        let mut command = access::withdrawal(&seq.engine, 1, Fixed::ONE, true);
        let Command::AccessWithdraw { evidence: AccessEvidence::Screened(screening), .. } = &mut command else {
            unreachable!()
        };
        screening.jurisdiction = jurisdiction;
        screening.signature =
            eip712::sign(&access::key(0x77), &eip712::screening_hash(&seq.engine.access_domain().unwrap(), screening));
        let events = seq.sequence(command).unwrap().unwrap();
        assert_eq!(events, [Event::WithdrawalRejected { account, amount: Fixed::ONE, reason: expected }]);
        assert_eq!(seq.engine.accounts()[&account].balance, Fixed::from_int(100));
        assert_eq!(seq.engine.next_request_nonce(account), nonce + 1);
    }
    seq.flush().unwrap();
    assert!(signed.try_recv().is_err());
    let entries = log_entries(&dir);
    assert!(!entries.iter().any(|entry| matches!(entry, LogEntry::Withdrawal(_))));
    let requests: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            LogEntry::Command(command) if matches!(command.sequenced.cmd, Command::AccessWithdraw { .. }) => {
                Some(command)
            }
            _ => None,
        })
        .collect();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|command| matches!(command.sequenced.cmd, Command::AccessWithdraw { payout_available: false, .. }))
    );
    assert!(seq.gate.as_ref().unwrap().pending.is_empty());
    seq.checkpoint_status(seq.last_seq(), Some(crate::checkpoints::Health::Healthy)).unwrap();
    let command = crate::test_support::access::withdrawal(&seq.engine, 1, Fixed::from_int(10), true);
    let events = seq.sequence(command).unwrap().unwrap();
    // From V22 a withdrawal reserves its payout in the public queue before its approval.
    assert!(
        matches!(
            events.as_slice(),
            [
                Event::WithdrawalQueued { amount, .. },
                Event::WithdrawalApproved { amount: approved, nonce: 1, .. },
                Event::WithdrawalQueueUpdated { remaining, .. },
            ] if *amount == Fixed::from_int(10) && *approved == Fixed::from_int(10) && *remaining == Fixed::ZERO
        ),
        "{events:?}"
    );
    let approval = seq.last_seq().unwrap();
    assert_eq!(seq.engine.accounts()[&account].balance, Fixed::from_int(90));
    crate::checkpoints::accounting::tests::assert_matches(seq.accounting, &seq.engine, Fixed::from_int(10));
    seq.flush().unwrap();
    assert!(signed.try_recv().is_err(), "admission does not release an uncommitted signature");
    drop(seq);
    let mut recovered =
        Sequencer::recover(config(), Arc::new(EnclaveKey::test_key()), broadcast::channel(128).0, withdrawals).unwrap();
    recovered.trust(sample_around(clock.as_ref())).unwrap();
    assert_eq!(recovered.gate.as_ref().unwrap().payouts.len(), 1);
    crate::checkpoints::accounting::tests::assert_matches(recovered.accounting, &recovered.engine, Fixed::from_int(10));
    let checkpoint = log_entries(&dir)
        .into_iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(cp) => Some(cp),
            _ => None,
        })
        .unwrap();
    let evidence = |checkpoint_seq: u64| LifecycleEvidence {
        height: 10,
        block_hash: eip712::B256::from([1; 32]),
        signer: AccountId(EnclaveKey::test_key().address()),
        signer_epoch: 1,
        total_withdrawn: Fixed::ZERO,
        checkpoint_seq: Some(checkpoint_seq),
        checkpoint_root: Some(checkpoint.root.clone()),
        approvals: vec![ApprovalStatus {
            approval: ApprovalId { account, nonce: 1, approved_seq: approval },
            claimed: false,
        }],
    };
    // Unhealthy books cannot authorize signing, and a healthy checkpoint behind the approval cannot
    // either.
    recovered.checkpoint_status(Some(approval), Some(crate::checkpoints::Health::Diverged)).unwrap();
    recovered.release_payouts(&evidence(approval)).unwrap();
    assert!(signed.try_recv().is_err());
    recovered.checkpoint_status(Some(approval), Some(crate::checkpoints::Health::Healthy)).unwrap();
    recovered.release_payouts(&evidence(approval - 1)).unwrap();
    assert!(signed.try_recv().is_err());
    // Healthy books at the covering checkpoint release the payout, durable before custody sees it.
    recovered.release_payouts(&evidence(approval)).unwrap();
    assert!(signed.try_recv().is_err(), "custody cannot see an unflushed outbox");
    recovered.flush().unwrap();
    let released = signed.try_recv().unwrap();
    assert_eq!(
        (released.account, released.amount, released.approved_seq),
        (account, Fixed::from_int(10), Some(approval))
    );
    recovered.release_payouts(&evidence(approval)).unwrap();
    recovered.flush().unwrap();
    assert!(signed.try_recv().is_err(), "the same confirmed prefix never releases twice");
    let replayed = crate::sequencer::replay_with_signer(
        &log_entries(&dir),
        crate::log::CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) },
        AccountId(EnclaveKey::test_key().address()),
    )
    .unwrap();
    assert!(replayed.deferred.is_empty());
    assert_eq!(replayed.engine.accounts()[&account].balance, Fixed::from_int(90));
}

#[test]
fn a_reissue_needs_the_screening_a_withdrawal_gets_and_a_refused_account_stays_held() {
    let dir = scratch_dir("access-reissue-screening");
    let (work, _queued) = mpsc::channel(64);
    let (withdrawals, mut signed) = mpsc::unbounded_channel();
    let clock = Arc::new(TestClock::at(NOW));
    let cfg = crate::sequencer::SequencerConfig {
        clock: clock.clone(),
        checkpoints: Some(work),
        access_policy: Some(access::policy()),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq =
        Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), broadcast::channel(128).0, withdrawals).unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(crate::test_support::empty_custody_frontier()).unwrap();
    let account = access::account(1);
    seq.sequence(access::declaration(&seq.engine, 1)).unwrap().unwrap();
    let funding =
        access::funding(&seq.engine, Some(account), Some(account), Fixed::from_int(100), "0xreissue-funding:1");
    seq.sequence(funding).unwrap().unwrap();
    seq.checkpoint_status(seq.last_seq(), Some(crate::checkpoints::Health::Healthy)).unwrap();
    // The account is screened clear and its withdrawal approved and signed.
    let events = seq.sequence(access::withdrawal(&seq.engine, 1, Fixed::from_int(10), true)).unwrap().unwrap();
    assert!(events.iter().any(|event| matches!(event, Event::WithdrawalApproved { nonce: 1, .. })), "{events:?}");
    let approved_seq = seq.last_seq().unwrap();
    let checkpoint = log_entries(&dir)
        .into_iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(cp) => Some(cp),
            _ => None,
        })
        .unwrap();
    let evidence = LifecycleEvidence {
        height: 10,
        block_hash: eip712::B256::from([1; 32]),
        signer: AccountId(EnclaveKey::test_key().address()),
        signer_epoch: 1,
        total_withdrawn: Fixed::ZERO,
        checkpoint_seq: Some(checkpoint.seq),
        checkpoint_root: Some(checkpoint.root.clone()),
        approvals: vec![ApprovalStatus { approval: ApprovalId { account, nonce: 1, approved_seq }, claimed: false }],
    };
    seq.release_payouts(&evidence).unwrap();
    seq.flush().unwrap();
    let first = signed.try_recv().unwrap();
    let expires_at = first.authorization.as_ref().unwrap().expires_at;

    // Its authorization expires unclaimed; by now the account is sanctioned or calls from the US.
    clock.advance(Duration::from_secs(crate::withdrawals::LIFETIME_SECONDS));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    let now = clock.as_ref().true_ms();
    let hash = crate::api::reissue::reissue_hash(&seq.engine.access_domain().unwrap(), account, 1, expires_at);
    let domain = seq.engine.access_domain().unwrap();
    let AccessEvidence::Screened(clear) = access::proof(&seq.engine, hash, Some(account), None) else { unreachable!() };
    let screened = |edit: &dyn Fn(&mut Screening)| {
        let mut screening = Screening { observed_at: now, ..clear.clone() };
        edit(&mut screening);
        screening.signature = eip712::sign(&access::key(0x77), &eip712::screening_hash(&domain, &screening));
        Some(ReissueScreening { request_hash: hash, evidence: AccessEvidence::Screened(screening) })
    };
    let refusals = [
        (screened(&|s| s.account_status = SanctionsStatus::Sanctioned), AccessReject::AccountSanctioned),
        (screened(&|s| s.jurisdiction = Jurisdiction::UnitedStates), AccessReject::UnitedStates),
        (screened(&|s| s.observed_at = now - access::policy().max_age_ms - 1), AccessReject::EvidenceStale),
        (screened(&|s| s.request_hash = eip712::B256::ZERO), AccessReject::EvidenceMismatch),
        (None, AccessReject::EvidenceMissing),
    ];
    for (screening, reason) in refusals {
        assert_eq!(seq.request_reissue(account, 1, expires_at, screening.as_ref()), Reissue::Refused { reason });
        seq.release_payouts(&evidence).unwrap();
        seq.flush().unwrap();
        assert!(signed.try_recv().is_err(), "{reason:?} got a fresh authorization");
    }
    // The approved funds stay held: debited from the account, still approved and unpaid.
    assert_eq!(seq.engine.accounts()[&account].balance, Fixed::from_int(90));
    assert!(seq.engine.withdrawal_approval(account, 1).is_some());
    assert_eq!(seq.gate.as_ref().unwrap().payouts[&(account, 1)].released.map(|(_, expiry)| expiry), Some(expires_at));
    // Clear screening of the same request reissues it.
    assert_eq!(seq.request_reissue(account, 1, expires_at, screened(&|_| {}).as_ref()), Reissue::Queued);
    seq.release_payouts(&evidence).unwrap();
    seq.flush().unwrap();
    let reissued = signed.try_recv().unwrap();
    assert_eq!((reissued.account, reissued.nonce, reissued.amount), (first.account, first.nonce, first.amount));
    assert!(reissued.authorization.unwrap().expires_at > expires_at);
}
