//! Current access envelopes keep reconciliation admission closed while recording policy refusals.

use std::sync::Arc;

use engine_core::Fixed;
use engine_core::access::{AccessEvidence, Jurisdiction};
use engine_core::command::Command;
use engine_core::eip712;
use engine_core::event::{Event, Reject};
use engine_core::types::AccountId;
use tokio::sync::{broadcast, mpsc, watch};

use crate::clock::test_clock::TestClock;
use crate::log::{LogEntry, Manifest};
use crate::sequencer::Sequencer;
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
    seq.checkpoint_status(seq.last_seq(), true).unwrap();
    let command = crate::test_support::access::withdrawal(&seq.engine, 1, Fixed::ONE, true);
    let events = seq.sequence(command).unwrap().unwrap();
    assert!(matches!(events.as_slice(), [Event::WithdrawalApproved { amount, .. }] if *amount == Fixed::ONE));
    let approval = seq.last_seq().unwrap();
    assert_eq!(seq.engine.accounts()[&account].balance, Fixed::from_int(99));
    crate::checkpoints::accounting::tests::assert_matches(seq.accounting, &seq.engine, Fixed::ONE);
    seq.flush().unwrap();
    assert!(signed.try_recv().is_err(), "admission does not release an uncommitted signature");
    drop(seq);
    let mut recovered =
        Sequencer::recover(config(), Arc::new(EnclaveKey::test_key()), broadcast::channel(128).0, withdrawals).unwrap();
    assert_eq!(recovered.gate.as_ref().unwrap().pending.len(), 1);
    crate::checkpoints::accounting::tests::assert_matches(recovered.accounting, &recovered.engine, Fixed::ONE);
    for (posted, healthy) in [(Some(approval), false), (Some(approval - 1), true)] {
        recovered.checkpoint_status(posted, healthy).unwrap();
        assert!(signed.try_recv().is_err());
    }
    recovered.checkpoint_status(Some(approval), true).unwrap();
    recovered.flush().unwrap();
    let released = signed.try_recv().unwrap();
    assert_eq!((released.account, released.amount, released.approved_seq), (account, Fixed::ONE, Some(approval)));
    recovered.checkpoint_status(Some(approval), true).unwrap();
    assert!(signed.try_recv().is_err(), "the same confirmed prefix never releases twice");
    let replayed = crate::sequencer::replay_with_signer(
        &log_entries(&dir),
        crate::log::CheckpointDomain { chain_id: 31_337, vault: AccountId([0; 20]) },
        AccountId(EnclaveKey::test_key().address()),
    )
    .unwrap();
    assert!(replayed.deferred.is_empty());
    assert_eq!(replayed.engine.accounts()[&account].balance, Fixed::from_int(99));
}
