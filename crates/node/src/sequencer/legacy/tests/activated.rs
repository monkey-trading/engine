//! Actual V20 recovery and signature-release contracts; chain-proof ingestion is tested in custody.

use super::*;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use engine_core::auth::ClientAuth;
use engine_core::deposits::{CustodyCheckpoint, DepositBeneficiary, DepositReceipt, DepositUpdate};
use engine_core::eip712::B256;
use engine_core::event::Reject;
use tokio::sync::{broadcast, mpsc, watch};

use crate::checkpoints::Work;
use crate::log::{Checkpoint, CheckpointDomain, CheckpointParent, Manifest, SegmentedLog};
use crate::sequencer::{ReplayedLog, Sequencer, SequencerConfig};
use crate::tee::EnclaveKey;
use crate::test_support::{dev_config, log_entries, scratch_dir, sequencer_config, trusted_now};

/// A checkpoint-gated approval: its approval seq, account, amount, and nonce.
type GatedApproval = (u64, AccountId, Fixed, u64);

#[tokio::test]
async fn activated_legacy_obligation_still_requires_proof_before_writable_recovery() {
    let dir = scratch_dir("legacy-already-activated-unproven");
    let (_, entries, _) = activated_history(&dir, false);
    write_log(&dir, &entries);
    let before = serde_json::to_vec(&log_entries(&dir)).unwrap();
    let mut log = read_log(&dir);
    assert!(log.replayed.engine.deposit_finality_enabled());
    assert_eq!(log.replayed.engine.accounts()[&TRADER].unfinalised_deposits, Fixed::from_int(100));
    let err = log.prove_legacy_withdrawals(&dev_config().custody).await.unwrap_err();
    assert!(err.to_string().contains("independently proven canonical finality"), "{err:#}");

    let (work, _work_rx) = mpsc::channel(16);
    let (withdrawals, mut received) = mpsc::unbounded_channel();
    let result = Sequencer::recover_from(
        log,
        config(&dir, work),
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(16).0,
        withdrawals,
    );
    let err = result.err().expect("unsafe inherited approval refuses recovery");
    assert!(err.to_string().contains("before writable recovery"), "{err:#}");
    assert!(received.try_recv().is_err());
    assert_eq!(serde_json::to_vec(&log_entries(&dir)).unwrap(), before);
}

#[test]
fn later_final_funding_releases_only_its_covered_prefix_and_restart_preserves_new_approval() {
    let dir = scratch_dir("legacy-activated-funding-prefix");
    let (engine, entries, original_seq) = activated_history(&dir, true);
    let funding_seq = engine.last_seq().unwrap();
    assert_eq!(engine.accounts()[&TRADER].balance, Fixed::from_int(275));
    assert_eq!(engine.accounts()[&TRADER].unfinalised_deposits, Fixed::from_int(100));
    let legacy = entries
        .iter()
        .find_map(|entry| match entry {
            LogEntry::Command(c) => {
                LegacyDeposit::from_command(&c.sequenced.cmd, c.events.as_deref().expect("computed historical events"))
            }
            _ => None,
        })
        .expect("recorded old credit");
    assert!(!engine.legacy_credit_finalised(&legacy), "a frontier does not prove the old receipt");
    write_log(&dir, &entries);
    let (work, _work_rx) = mpsc::channel(16);
    let (withdrawals, mut received) = mpsc::unbounded_channel();
    let mut seq = Sequencer::recover_from(
        read_log(&dir),
        config(&dir, work),
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(16).0,
        withdrawals,
    )
    .unwrap();
    assert_eq!(seq.legacy_authorizations.get(&(TRADER, 1)), Some(&funding_seq));
    seq.trust(trusted_now()).unwrap();
    let before = seq.last_seq();
    assert!(
        seq.sequence(Command::Withdraw { account: TRADER, amount: Fixed::from_int(200), auth: None })
            .unwrap()
            .is_none()
    );
    assert_eq!(seq.last_seq(), before, "initial reconciliation blocks admission");
    seq.checkpoint().unwrap();
    seq.checkpoint_status(Some(original_seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    seq.flush().unwrap();
    assert!(received.try_recv().is_err(), "the original checkpoint cannot cover later funding");
    let events =
        seq.sequence(Command::Withdraw { account: TRADER, amount: Fixed::from_int(200), auth: None }).unwrap().unwrap();
    assert!(events.iter().any(|event| matches!(event,
        Event::WithdrawalRejected { reason: Reject::InsufficientFunds { available }, .. }
        if *available == Fixed::from_int(175))));
    let events =
        seq.sequence(Command::Withdraw { account: TRADER, amount: Fixed::from_int(175), auth: None }).unwrap().unwrap();
    assert!(events.iter().any(|event| matches!(event, Event::WithdrawalApproved { nonce: 2, .. })));
    let new_seq = seq.engine.last_seq().unwrap();
    assert!(new_seq > funding_seq);

    for (posted, healthy) in [(funding_seq - 1, true), (funding_seq, false)] {
        seq.checkpoint_status(
            Some(posted),
            Some(if healthy { crate::checkpoints::Health::Healthy } else { crate::checkpoints::Health::Pending }),
        )
        .unwrap();
        seq.flush().unwrap();
        assert!(received.try_recv().is_err());
    }
    seq.checkpoint_status(Some(funding_seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    seq.flush().unwrap();
    let old = received.try_recv().expect("old reservation covered at its actual funding prefix");
    assert_eq!(
        (old.account, old.amount, old.nonce, old.approved_seq),
        (TRADER, Fixed::from_int(50), 1, Some(original_seq))
    );
    assert!(received.try_recv().is_err(), "a new approval cannot borrow the older proof prefix");
    seq.checkpoint_status(Some(funding_seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    seq.flush().unwrap();
    assert!(received.try_recv().is_err());
    assert_eq!(seq.engine.accounts()[&TRADER].balance, Fixed::from_int(100));
    drop(seq);

    let (work, _restart_work_rx) = mpsc::channel(16);
    let (withdrawals, mut received) = mpsc::unbounded_channel();
    let mut seq = Sequencer::recover_from(
        read_log(&dir),
        config(&dir, work),
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(16).0,
        withdrawals,
    )
    .unwrap();
    assert!(!seq.legacy_approvals.contains(TRADER, 1));
    seq.checkpoint_status(Some(funding_seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    seq.flush().unwrap();
    assert!(received.try_recv().is_err());
    seq.checkpoint_status(Some(new_seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    seq.flush().unwrap();
    let new = received.try_recv().expect("new approval waits for its own checkpoint");
    assert_eq!(
        (new.account, new.amount, new.nonce, new.approved_seq),
        (TRADER, Fixed::from_int(175), 2, Some(new_seq))
    );
    seq.checkpoint_status(Some(new_seq), Some(crate::checkpoints::Health::Healthy)).unwrap();
    seq.flush().unwrap();
    assert!(received.try_recv().is_err());
    assert_eq!(seq.engine.accounts()[&TRADER].balance, Fixed::from_int(100));
    assert_eq!(seq.engine.accounts()[&TRADER].unfinalised_deposits, Fixed::from_int(100));
    let root = seq.engine.state_root();
    drop(seq);
    let mut replayed = read_log(&dir);
    assert_eq!(replayed.replayed.engine.state_root(), root);
    assert!(replayed.replayed.deferred.is_empty());
}

#[test]
fn checkpoint_release_reuses_proven_prefixes_and_batches_uncached_approval_work() {
    for (approvals, unrelated) in [(4, 32), (32, 96)] {
        let dir = scratch_dir("legacy-release-batch-work");
        let (engine, entries, expected) = scaling_history(&dir, approvals, unrelated);
        assert!(engine.deposit_finality_enabled());
        write_log(&dir, &entries);
        let mut log = read_log(&dir);
        let proof =
            FinalityProof { legacy: log.replayed.legacy_deposits.keys().cloned().collect(), ..Default::default() };
        let authenticated = log
            .replayed
            .legacy_approvals
            .authorize(&log.replayed.engine, &log.replayed.legacy_deposits, &proof)
            .unwrap();
        let prefixes: BTreeMap<_, _> =
            expected.iter().map(|(seq, account, _, nonce)| ((*account, *nonce), *seq)).collect();
        assert_eq!(authenticated, prefixes);
        assert_eq!(log.replayed.legacy_deposits.len(), usize::from(unrelated) + 1);
        log.legacy_authorizations = authenticated.clone();
        let (work, _work_rx) = mpsc::channel(16);
        let (withdrawals, mut received) = mpsc::unbounded_channel();
        let mut seq = Sequencer::recover_from(
            log,
            config(&dir, work),
            Arc::new(EnclaveKey::test_key()),
            broadcast::channel(16).0,
            withdrawals,
        )
        .unwrap();
        seq.checkpoint().unwrap();
        let posted = seq.last_seq().unwrap();
        let root = seq.engine.state_root();
        let calls = seq.legacy_approvals.authorization_passes();
        let below = expected[0].0 - 1;
        for healthy in [false, true, true] {
            seq.checkpoint_status(
                Some(below),
                Some(if healthy { crate::checkpoints::Health::Healthy } else { crate::checkpoints::Health::Pending }),
            )
            .unwrap();
            seq.flush().unwrap();
            assert!(received.try_recv().is_err());
            assert_eq!(seq.gate.as_ref().unwrap().pending, expected);
            assert_eq!(seq.legacy_approvals.authorization_passes(), calls, "proved keys do not scan the ledger");
        }

        // Losing the derived cache cannot authorize an unproved reservation or cause per-key scans.
        seq.legacy_authorizations.clear();
        seq.checkpoint_status(Some(posted), Some(crate::checkpoints::Health::Healthy)).unwrap();
        seq.flush().unwrap();
        assert!(received.try_recv().is_err());
        assert_eq!(seq.gate.as_ref().unwrap().pending, expected);
        assert_eq!(seq.legacy_approvals.authorization_passes(), calls + 1, "one empty authorization batch");

        seq.legacy_authorizations = authenticated;
        seq.checkpoint_status(Some(posted), Some(crate::checkpoints::Health::Healthy)).unwrap();
        seq.flush().unwrap();
        let mut released = Vec::new();
        while let Ok(signed) = received.try_recv() {
            released.push((signed.approved_seq.unwrap(), signed.account, signed.amount, signed.nonce));
        }
        assert_eq!(released, expected);
        assert!(seq.gate.as_ref().unwrap().pending.is_empty());
        assert_eq!(seq.legacy_approvals.authorization_passes(), calls + 1);
        seq.checkpoint_status(Some(posted), Some(crate::checkpoints::Health::Healthy)).unwrap();
        seq.flush().unwrap();
        assert!(received.try_recv().is_err());
        assert_eq!(seq.legacy_approvals.authorization_passes(), calls + 1, "an empty pass does no authorization work");
        assert_eq!(seq.engine.state_root(), root, "signature delivery changes neither money nor principal locks");
        assert_eq!(seq.engine.accounts()[&TRADER].balance, Fixed::from_int(25));
        let principal = Fixed::from_int(i64::try_from(approvals).unwrap() * 50);
        assert_eq!(seq.engine.accounts()[&TRADER].unfinalised_deposits, principal);
        drop(seq);
        assert_eq!(read_log(&dir).replayed.unpaid, principal, "signature release is not a settled payout");
    }
}

fn scaling_history(dir: &Path, approvals: usize, unrelated: u8) -> (Engine, Vec<LogEntry>, Vec<GatedApproval>) {
    let cfg = sequencer_config(dir, watch::channel(Manifest::default()).0);
    let (mut engine, mut entries) = (Engine::new(), Vec::new());
    for command in [
        AdminCommand::SetClientAuth(ClientAuth::Unsigned),
        AdminCommand::SetOperator(cfg.operator),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V17 },
    ] {
        assert!(apply(&mut engine, &mut entries, Command::Admin(command)).contains(&Event::AdminApplied));
    }
    for id in 0..unrelated {
        apply(
            &mut engine,
            &mut entries,
            Command::Deposit {
                account: AccountId([9; 20]),
                amount: Fixed::from_int(10),
                chain_ref: format!("{}:0", B256::repeat_byte(id.checked_add(2).unwrap())),
            },
        );
    }
    apply(
        &mut engine,
        &mut entries,
        Command::Deposit {
            account: TRADER,
            amount: Fixed::from_int(i64::try_from(approvals).unwrap() * 50),
            chain_ref: format!("{}:0", B256::repeat_byte(1)),
        },
    );
    apply(
        &mut engine,
        &mut entries,
        Command::Deposit { account: TRADER, amount: Fixed::from_int(25), chain_ref: "dev:known-funds".into() },
    );
    let mut expected = Vec::new();
    let mut last_parent = CheckpointParent::default();
    for _ in 0..approvals {
        let events = apply(
            &mut engine,
            &mut entries,
            Command::Withdraw { account: TRADER, amount: Fixed::from_int(50), auth: None },
        );
        let (account, amount, nonce) = events
            .iter()
            .find_map(|event| match event {
                Event::WithdrawalApproved { account, amount, nonce } => Some((*account, *amount, *nonce)),
                _ => None,
            })
            .expect("valid historical approval");
        // Each approval is checkpoint-deferred before the next command, as the gated sequencer logs it.
        let seq = deferred_checkpoint(
            &mut engine,
            &mut entries,
            CheckpointDomain { chain_id: cfg.chain_id, vault: cfg.vault },
        );
        last_parent = CheckpointParent { has_checkpoint: true, seq, root: engine.state_root().into() };
        expected.push((seq, account, amount, nonce));
    }
    let legacy_deposits = replay(&entries).unwrap().legacy_deposits.values().cloned().collect();
    assert!(
        apply(
            &mut engine,
            &mut entries,
            Command::Admin(AdminCommand::ActivateCustodyFinality { version: ProtocolVersion::V20, legacy_deposits })
        )
        .contains(&Event::AdminApplied)
    );
    // The tail checkpoint covers the unsigned finality-activation command: recovery adopts no
    // command its last authenticated checkpoint did not cover.
    parented_checkpoint(
        &mut engine,
        &mut entries,
        CheckpointDomain { chain_id: cfg.chain_id, vault: cfg.vault },
        last_parent,
    );
    (engine, entries, expected)
}

fn deferred_checkpoint(engine: &mut Engine, entries: &mut Vec<LogEntry>, domain: CheckpointDomain) -> u64 {
    parented_checkpoint(engine, entries, domain, CheckpointParent::default()).seq
}

/// A checkpoint whose signed parent names the previous checkpoint, covering every command before it.
fn parented_checkpoint(
    engine: &mut Engine,
    entries: &mut Vec<LogEntry>,
    domain: CheckpointDomain,
    parent: CheckpointParent,
) -> CheckpointParent {
    let seq = engine.last_seq().unwrap();
    let root = engine.state_root();
    let key = EnclaveKey::test_key();
    entries.push(LogEntry::Checkpoint(Checkpoint {
        seq,
        root: format!("0x{}", hex::encode(root)),
        signer: key.address_hex(),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, seq, &root, parent),
        domain: Some(domain),
        parent: Some(parent),
        deferred_withdrawals: true,
    }));
    CheckpointParent { has_checkpoint: true, seq, root: root.into() }
}

fn config(dir: &Path, work: mpsc::Sender<Work>) -> SequencerConfig<'_> {
    let mut cfg = sequencer_config(dir, watch::channel(Manifest::default()).0);
    cfg.client_auth = ClientAuth::Unsigned;
    cfg.checkpoints = Some(work);
    cfg.checkpoint_every = 1_000;
    cfg
}

fn activated_history(dir: &Path, later_funding: bool) -> (Engine, Vec<LogEntry>, u64) {
    let cfg = sequencer_config(dir, watch::channel(Manifest::default()).0);
    let (mut engine, mut entries) = (Engine::new(), Vec::new());
    for command in [
        AdminCommand::SetClientAuth(ClientAuth::Unsigned),
        AdminCommand::SetOperator(cfg.operator),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::V17 },
    ] {
        assert!(apply(&mut engine, &mut entries, Command::Admin(command)).contains(&Event::AdminApplied));
    }
    let command = Command::Deposit {
        account: TRADER,
        amount: Fixed::from_int(100),
        chain_ref: format!("{}:0", B256::repeat_byte(1)),
    };
    let events = apply(&mut engine, &mut entries, command.clone());
    let legacy = LegacyDeposit::from_command(&command, &events).expect("actual successful historical credit");
    apply(
        &mut engine,
        &mut entries,
        Command::Deposit { account: TRADER, amount: Fixed::from_int(25), chain_ref: "dev:known-funds".into() },
    );
    assert!(
        apply(
            &mut engine,
            &mut entries,
            Command::Withdraw { account: TRADER, amount: Fixed::from_int(50), auth: None }
        )
        .iter()
        .any(|event| matches!(event, Event::WithdrawalApproved { nonce: 1, .. }))
    );
    let original_seq =
        deferred_checkpoint(&mut engine, &mut entries, CheckpointDomain { chain_id: cfg.chain_id, vault: cfg.vault });
    // The tail checkpoint below covers the unsigned finality-activation command: recovery adopts
    // no command its last authenticated checkpoint did not cover.
    let original_root = engine.state_root();
    assert!(
        apply(
            &mut engine,
            &mut entries,
            Command::Admin(AdminCommand::ActivateCustodyFinality {
                version: ProtocolVersion::V20,
                legacy_deposits: vec![legacy],
            })
        )
        .contains(&Event::AdminApplied)
    );
    if later_funding {
        let receipt = DepositReceipt {
            block: 2,
            block_hash: B256::repeat_byte(3),
            tx_hash: B256::repeat_byte(4),
            log_index: 0,
            transaction_deposit_index: 0,
            deposit_id: 1,
            beneficiary: DepositBeneficiary::Trader { account: TRADER },
            amount: Fixed::from_int(200),
        };
        apply(
            &mut engine,
            &mut entries,
            Command::CustodyDeposit(Box::new(DepositUpdate::Credit { receipt: receipt.clone() })),
        );
        let observed_at_seq = engine.recorded_deposit(&receipt.reference()).unwrap().updated_at_seq;
        apply(
            &mut engine,
            &mut entries,
            Command::CustodyDeposit(Box::new(DepositUpdate::Finalise { receipt: receipt.clone(), observed_at_seq })),
        );
        apply(
            &mut engine,
            &mut entries,
            Command::CustodyFinalised(CustodyCheckpoint {
                block: receipt.block,
                block_hash: receipt.block_hash,
                deposit_count: 2,
            }),
        );
        assert_eq!(engine.custody_deposit_coverage(), Some(2));
    }
    parented_checkpoint(
        &mut engine,
        &mut entries,
        CheckpointDomain { chain_id: cfg.chain_id, vault: cfg.vault },
        CheckpointParent { has_checkpoint: true, seq: original_seq, root: original_root.into() },
    );
    (engine, entries, original_seq)
}

fn write_log(dir: &Path, entries: &[LogEntry]) {
    let mut writer =
        SegmentedLog::open(dir, |_| Ok(())).unwrap().into_writer(1_000, watch::channel(Manifest::default()).0).unwrap();
    for entry in entries {
        writer.append(entry).unwrap();
    }
    writer.sync().unwrap();
}

fn read_log(dir: &Path) -> ReplayedLog {
    let cfg = sequencer_config(dir, watch::channel(Manifest::default()).0);
    let key = EnclaveKey::test_key();
    Sequencer::read_log_with_trust(
        dir,
        Some(CheckpointDomain { chain_id: cfg.chain_id, vault: cfg.vault }),
        Some(AccountId(key.address())),
        |_| {},
    )
    .unwrap()
}
