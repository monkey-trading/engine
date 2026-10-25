use super::*;
use crate::log::{CommandEntry, LogEntry};
use crate::sequencer::Replayed;
use crate::sequencer::replay;
use engine_core::command::Sequenced;
use engine_core::event::Event;
use engine_core::{Engine, Fixed};
use std::collections::BTreeSet;

const TRADER: AccountId = AccountId([7; 20]);

mod activated;

#[test]
fn legacy_proof_includes_checkpoint_deferred_and_last_unsigned_approval_tuples() {
    let (mut engine, _, mut commands) = history(25);
    apply(&mut engine, &mut commands, Command::Withdraw { account: TRADER, amount: Fixed::from_int(50), auth: None });
    let key = crate::tee::EnclaveKey::test_key();
    let domain = crate::log::CheckpointDomain { chain_id: 31337, vault: TRADER };
    let root = engine.state_root();
    let parent = crate::log::CheckpointParent::default();
    commands.push(LogEntry::Checkpoint(crate::log::Checkpoint {
        seq: engine.last_seq().unwrap(),
        root: format!("0x{}", hex::encode(root)),
        signer: key.address_hex(),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, engine.last_seq().unwrap(), &root, parent),
        domain: Some(domain),
        parent: Some(parent),
        deferred_withdrawals: true,
    }));
    let deferred_only = replay(&commands).unwrap();
    assert!(deferred_only.unsigned.is_empty());
    assert!(!all_authorized(&deferred_only, &FinalityProof::default()));
    apply(&mut engine, &mut commands, Command::Withdraw { account: TRADER, amount: Fixed::from_int(5), auth: None });
    let replayed = replay(&commands).unwrap();
    assert_eq!(replayed.deferred, vec![(2, TRADER, Fixed::from_int(50), 1)]);
    assert_eq!(replayed.unsigned, vec![(TRADER, Fixed::from_int(5), 2)]);
    assert!(!all_authorized(&replayed, &FinalityProof::default()));
    let proved = FinalityProof { legacy: replayed.legacy_deposits.keys().cloned().collect(), ..Default::default() };
    let authorized = replayed.legacy_approvals.authorize(&replayed.engine, &replayed.legacy_deposits, &proved).unwrap();
    assert_eq!(authorized, BTreeMap::from([((TRADER, 1), 2), ((TRADER, 2), 3)]));
    assert_eq!(replayed.unpaid, Fixed::from_int(55));
}

#[test]
fn legacy_activation_rejects_omitted_duplicate_or_changed_verified_credit_records() {
    let (_, ledger, _) = history(0);
    let expected: Vec<_> = ledger.values().cloned().collect();
    let command = |legacy_deposits| {
        Command::Admin(AdminCommand::ActivateCustodyFinality { version: ProtocolVersion::LATEST, legacy_deposits })
    };
    assert!(check_activation_ledger(&command(expected.clone()), &ledger).is_ok());
    let mut changed = expected.clone();
    changed[0].amount = Fixed::ONE;
    for records in [vec![], vec![expected[0].clone(), expected[0].clone()], changed] {
        assert!(
            check_activation_ledger(&command(records), &ledger)
                .unwrap_err()
                .to_string()
                .contains("verified credit ledger")
        );
    }
}

#[test]
fn stranded_approval_is_safe_only_when_its_debit_leaves_unresolved_principal_untouched() {
    let (mut engine, ledger, mut commands) = history(25);
    let events = apply(
        &mut engine,
        &mut commands,
        Command::Withdraw { account: TRADER, amount: Fixed::from_int(50), auth: None },
    );
    assert!(matches!(events.as_slice(), [Event::WithdrawalApproved { nonce: 1, .. }]));
    let replayed = replay(&commands).unwrap();
    assert!(!all_authorized(&replayed, &FinalityProof::default()));
    let proof = FinalityProof { legacy: ledger.keys().cloned().collect(), ..Default::default() };
    assert!(all_authorized(&replayed, &proof));
    let invented = FinalityProof { legacy: BTreeSet::from(["host-cursor-says-final".into()]), ..Default::default() };
    assert!(!all_authorized(&replayed, &invented));

    let (mut engine, _, mut commands) = history(100);
    apply(&mut engine, &mut commands, Command::Withdraw { account: TRADER, amount: Fixed::from_int(50), auth: None });
    assert!(
        all_authorized(&replay(&commands).unwrap(), &FinalityProof::default()),
        "unrelated mock/final cash remains eligible"
    );
}

#[test]
fn verified_replay_recovers_legacy_credit_provenance_without_rewriting_historical_events_or_roots() {
    let (mut engine, ledger, commands) = history(25);
    let before = engine.state_root();
    let mut replayed = replay(&commands).unwrap();
    assert_eq!(replayed.legacy_deposits, ledger);
    assert_eq!(replayed.engine.state_root(), before);
    assert_eq!(replayed.engine.accounts()[&TRADER].balance, Fixed::from_int(125));
    assert_eq!(replayed.engine.accounts()[&TRADER].unfinalised_deposits, Fixed::ZERO);
    assert_eq!(replayed.engine.protocol_version(), ProtocolVersion::V2);
}

#[test]
fn writable_recovery_refuses_an_unproven_legacy_signature_without_appending_or_delivering() {
    use crate::log::{Manifest, SegmentedLog};
    use crate::sequencer::Sequencer;
    use crate::tee::EnclaveKey;
    use crate::test_support::{log_entries, scratch_dir, sequencer_config};
    use std::sync::Arc;
    use tokio::sync::{broadcast, mpsc, watch};

    assert!(ProtocolVersion::LATEST.supports_deposit_finality(), "actual V18/V19 integration required");
    let (mut engine, _, mut entries) = history(25);
    apply(&mut engine, &mut entries, Command::Withdraw { account: TRADER, amount: Fixed::from_int(50), auth: None });
    let dir = scratch_dir("legacy-unsigned-recovery");
    let mut writer =
        SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(1000, watch::channel(Manifest::default()).0).unwrap();
    for entry in &entries {
        writer.append(entry).unwrap();
    }
    writer.sync().unwrap();
    drop(writer);
    let before = serde_json::to_vec(&log_entries(&dir)).unwrap();
    let (withdrawals, mut received) = mpsc::unbounded_channel();
    let result = Sequencer::recover(
        sequencer_config(&dir, watch::channel(Manifest::default()).0),
        Arc::new(EnclaveKey::generate()),
        broadcast::channel(8).0,
        withdrawals,
    );
    assert!(result.err().unwrap().to_string().contains("before writable recovery"));
    assert!(received.try_recv().is_err());
    assert_eq!(serde_json::to_vec(&log_entries(&dir)).unwrap(), before);
}

#[test]
fn later_final_funding_requires_its_actual_prefix_and_never_relabels_an_old_receipt() {
    let (mut engine, _, mut entries) = history(25);
    apply(&mut engine, &mut entries, Command::Withdraw { account: TRADER, amount: Fixed::from_int(50), auth: None });
    defer(&mut engine, &mut entries);
    let new_reference = format!("{}:2", alloy::primitives::B256::repeat_byte(3));
    apply(
        &mut engine,
        &mut entries,
        Command::Deposit { account: TRADER, amount: Fixed::from_int(50), chain_ref: new_reference.clone() },
    );
    let replayed = replay(&entries).unwrap();
    assert_eq!(replayed.engine.accounts()[&TRADER].balance, Fixed::from_int(125));
    assert!(!all_authorized(&replayed, &FinalityProof::default()), "later unproved funding remains unavailable");
    let proof = FinalityProof { legacy: BTreeSet::from([new_reference]), ..Default::default() };
    let authorized = replayed.legacy_approvals.authorize(&replayed.engine, &replayed.legacy_deposits, &proof).unwrap();
    assert_eq!(authorized, BTreeMap::from([((TRADER, 1), 3)]), "later funds require their own authenticated prefix");
    assert_eq!(replayed.deferred, vec![(2, TRADER, Fixed::from_int(50), 1)]);
    assert_eq!(replayed.legacy_approvals.release_seq(&authorized, TRADER, 1, 2), Some(3));
    assert_eq!(replayed.legacy_approvals.release_seq(&Authorizations::new(), TRADER, 1, 2), None);
    assert_eq!(
        replayed.legacy_approvals.release_seq(&authorized, TRADER, 99, 4),
        Some(4),
        "later core approvals have no inherited obligation"
    );
    assert_eq!(replayed.legacy_deposits.len(), 2, "delivery proof never mutates deposit state or economics");
}

#[test]
fn safe_original_reservation_survives_later_debits_and_is_not_charged_for_later_credits() {
    let (mut engine, _, mut entries) = history(100);
    apply(&mut engine, &mut entries, Command::Withdraw { account: TRADER, amount: Fixed::from_int(50), auth: None });
    defer(&mut engine, &mut entries);
    apply(&mut engine, &mut entries, Command::Withdraw { account: TRADER, amount: Fixed::from_int(100), auth: None });
    defer(&mut engine, &mut entries);
    apply(
        &mut engine,
        &mut entries,
        Command::Deposit {
            account: TRADER,
            amount: Fixed::from_int(300),
            chain_ref: format!("{}:1", alloy::primitives::B256::repeat_byte(3)),
        },
    );
    let replayed = replay(&entries).unwrap();
    let authorized = replayed
        .legacy_approvals
        .authorize(&replayed.engine, &replayed.legacy_deposits, &FinalityProof::default())
        .unwrap();
    assert_eq!(replayed.engine.accounts()[&TRADER].balance, Fixed::from_int(350));
    assert_eq!(authorized, BTreeMap::from([((TRADER, 1), 2)]));
    assert!(
        !replayed.legacy_approvals.all_authorized(&authorized),
        "unsafe later reservation cannot borrow the earlier proof"
    );
    assert_eq!(replayed.unpaid, Fixed::from_int(150));
}

#[test]
fn provenance_is_removed_only_by_the_authenticated_matching_signature_not_by_a_checkpoint() {
    let (mut engine, _, mut entries) = history(25);
    apply(&mut engine, &mut entries, Command::Withdraw { account: TRADER, amount: Fixed::from_int(50), auth: None });
    defer(&mut engine, &mut entries);
    let replayed = replay(&entries).unwrap();
    assert!(replayed.legacy_approvals.contains(TRADER, 1));
    let (key, domain) = signing();
    let signed = crate::sequencer::SignedWithdrawal {
        account: TRADER,
        amount: Fixed::from_int(50),
        nonce: 1,
        chain_id: domain.chain_id,
        vault: domain.vault,
        approved_seq: Some(2),
        signature: key.sign_withdrawal(domain.chain_id, &domain.vault.0, TRADER, Fixed::from_int(50), 1),
    };
    let mut invalid = signed.clone();
    invalid.approved_seq = Some(1);
    let mut bad_entries = entries.clone();
    bad_entries.push(LogEntry::Withdrawal(invalid));
    assert!(
        crate::sequencer::replay_with_signer(&bad_entries, domain, AccountId(key.address()))
            .err()
            .unwrap()
            .to_string()
            .contains("not approved")
    );
    entries.push(LogEntry::Withdrawal(signed));
    let complete = crate::sequencer::replay_with_signer(&entries, domain, AccountId(key.address())).unwrap();
    assert!(complete.deferred.is_empty());
    assert!(!complete.legacy_approvals.contains(TRADER, 1));
    assert!(all_authorized(&complete, &FinalityProof::default()));
    assert_eq!(complete.unpaid, Fixed::from_int(50), "signing is not settlement");
    assert_eq!(complete.engine.accounts()[&TRADER].balance, Fixed::from_int(75));
}

/// Commits the exact original approval prefix using the real checkpoint format and signer.
fn defer(engine: &mut Engine, entries: &mut Vec<LogEntry>) {
    let (key, domain) = signing();
    let seq = engine.last_seq().unwrap();
    let root = engine.state_root();
    let parent = crate::log::CheckpointParent::default();
    entries.push(LogEntry::Checkpoint(crate::log::Checkpoint {
        seq,
        root: format!("0x{}", hex::encode(root)),
        signer: key.address_hex(),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, seq, &root, parent),
        domain: Some(domain),
        parent: Some(parent),
        deferred_withdrawals: true,
    }));
}

/// Uses the existing published development signer and a private fixture domain.
fn signing() -> (crate::tee::EnclaveKey, crate::log::CheckpointDomain) {
    (crate::tee::EnclaveKey::test_key(), crate::log::CheckpointDomain { chain_id: 31337, vault: TRADER })
}

fn history(mock: i64) -> (Engine, BTreeMap<String, LegacyDeposit>, Vec<LogEntry>) {
    let (mut engine, mut commands) = (Engine::new(), Vec::new());
    let command = Command::Deposit {
        account: TRADER,
        amount: Fixed::from_int(100),
        chain_ref: format!("{}:1", alloy::primitives::B256::repeat_byte(2)),
    };
    let events = apply(&mut engine, &mut commands, command.clone());
    let legacy = LegacyDeposit::from_command(&command, &events).unwrap();
    if mock > 0 {
        apply(
            &mut engine,
            &mut commands,
            Command::Deposit { account: TRADER, amount: Fixed::from_int(mock), chain_ref: "dev:mock".into() },
        );
    }
    (engine, BTreeMap::from([(legacy.chain_ref.clone(), legacy)]), commands)
}

fn apply(engine: &mut Engine, commands: &mut Vec<LogEntry>, cmd: Command) -> Vec<Event> {
    let sequenced = Sequenced { seq: engine.last_seq().map_or(0, |seq| seq + 1), ts: 1, cmd };
    let events = engine.apply(&sequenced).unwrap();
    commands.push(LogEntry::Command(CommandEntry { sequenced, events: Some(events.clone()) }));
    events
}

/// Uses the same pure authorization as startup, with no protocol activation bypass.
fn all_authorized(replayed: &Replayed, proof: &FinalityProof) -> bool {
    replayed.legacy_approvals.all_authorized(
        &replayed.legacy_approvals.authorize(&replayed.engine, &replayed.legacy_deposits, proof).unwrap(),
    )
}
