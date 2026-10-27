//! Replay roots, ordering, and historical logs without logged events.

use super::*;
use crate::log::Checkpoint;

fn tick(seq: u64) -> LogEntry {
    LogEntry::Command(CommandEntry {
        seal: None,
        sequenced: Sequenced { seq, ts: 0, cmd: Command::Tick },
        events: None,
    })
}

fn checkpoint(seq: u64, root: String) -> LogEntry {
    let key = EnclaveKey::test_key();
    let domain = crate::log::CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
    let signature = key.sign_checkpoint(
        domain.chain_id,
        &domain.vault.0,
        seq,
        &root.parse::<engine_core::eip712::B256>().unwrap_or_default().0,
    );
    LogEntry::Checkpoint(Checkpoint {
        parent: None,
        seq,
        root,
        signature,
        signer: key.address_hex(),
        domain: Some(domain),
        deferred_withdrawals: false,
    })
}

#[test]
fn replay_verifies_checkpoint_roots() {
    let mut engine = Engine::new();
    engine.apply(&Sequenced { seq: 0, ts: 0, cmd: Command::Tick }).unwrap();
    let root = format!("0x{}", hex::encode(engine.state_root()));
    let replayed = replay(&[tick(0), checkpoint(0, root)]).unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (1, 1));
}

#[test]
fn replay_refuses_a_tampered_root_or_a_misplaced_checkpoint() {
    let err = replay(&[tick(0), checkpoint(0, "0xbad".into())]).err().unwrap();
    assert!(format!("{err:#}").contains("state root mismatch"), "{err:#}");
    let err = replay(&[tick(0), checkpoint(5, "0x".into())]).err().unwrap();
    assert!(format!("{err:#}").contains("checkpoint for seq 5"), "{err:#}");
}

#[test]
fn replay_refuses_a_gap_in_the_log() {
    let err = replay(&[tick(0), tick(2)]).err().unwrap();
    assert!(format!("{err:#}").contains("applying seq 2"), "{err:#}");
}

#[test]
fn replay_accepts_a_log_written_before_events_and_withdrawals_were_logged() {
    let mut engine = Engine::new();
    let commands = [deposit(100, "dep-1"), withdraw(40)];
    let mut entries: Vec<_> = commands
        .into_iter()
        .enumerate()
        .map(|(seq, cmd)| {
            let sequenced = Sequenced { seq: seq as u64, ts: 0, cmd };
            engine.apply(&sequenced).unwrap();
            LogEntry::Command(CommandEntry { seal: None, sequenced, events: None })
        })
        .collect();
    entries.push(checkpoint(1, format!("0x{}", hex::encode(engine.state_root()))));
    let replayed = replay(&entries).unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (2, 1));
    // The old node signed that withdrawal itself; recovery must not sign it again.
    assert!(replayed.unsigned.is_empty());
}

/// Both kill shapes leave the approval unsigned: the process died after the checkpoint sync and
/// before the authorization sync, or before the checkpoint itself was durable.
async fn a_restart_resigns_a_mock_withdrawal_the_log_ends_before(cut: fn(&str) -> usize, name: &str) {
    let dir = scratch_dir(name);
    let node = TestSequencer::start_legacy(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.submit(withdraw(40)).await;
    node.stop().await;
    let segment = last_segment(&dir);
    let text = fs::read_to_string(&segment).unwrap();
    fs::write(&segment, &text[..cut(&text)]).unwrap();
    let replayed = replay(&log_entries(&dir)).unwrap();
    assert_eq!(
        replayed
            .unreleased
            .iter()
            .map(|approval| (approval.account, approval.amount, approval.nonce))
            .collect::<Vec<_>>(),
        [(TRADER, Fixed::from_int(40), 1)]
    );
    assert!(
        replayed.unsigned.is_empty(),
        "the durable approval is tracked separately from legacy unsigned withdrawals"
    );

    let mut node = TestSequencer::start_legacy(&dir, 16);
    let signed = node.withdrawals.try_recv().unwrap();
    assert_eq!((signed.account, signed.amount, signed.nonce), (TRADER, Fixed::from_int(40), 1));
    assert!(signed.authorization.is_some(), "the re-signed mock payout carries its authorization");
    node.stop().await;
    // Recovery re-signed against a covering checkpoint, and the durable entry replays cleanly: a
    // restart neither refuses the log nor hands custody a duplicate payout.
    let replayed = replay(&log_entries(&dir)).unwrap();
    assert!(replayed.unreleased.is_empty());
    let mut node = TestSequencer::start_legacy(&dir, 16);
    assert!(node.withdrawals.try_recv().is_err(), "recovery does not re-sign a payout custody already took");
    node.stop().await;
}

#[tokio::test]
async fn a_restart_resigns_a_mock_withdrawal_the_log_ends_before_its_authorization() {
    a_restart_resigns_a_mock_withdrawal_the_log_ends_before(
        |text| {
            let withdrawal = text.rfind(r#""entry":"withdrawal""#).unwrap();
            text[..withdrawal].rfind('\n').map(|at| at + 1).unwrap()
        },
        "seq-mock-resign",
    )
    .await;
}

#[tokio::test]
async fn a_restart_writes_the_missing_checkpoint_before_resigning_a_mock_withdrawal() {
    a_restart_resigns_a_mock_withdrawal_the_log_ends_before(
        |text| {
            let command = text.find(r#""cmd":{"type":"withdraw""#).unwrap();
            command + text[command..].find('\n').unwrap() + 1
        },
        "seq-mock-resign-no-checkpoint",
    )
    .await;
}
