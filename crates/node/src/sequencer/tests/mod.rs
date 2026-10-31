use super::*;
use crate::sequencer::fixture_tests::{finished_genesis, replay_trusted as replay};
use std::fs;
use std::path::PathBuf;

use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::AdminCommand;
use engine_core::params::{ProtocolParams, StressTrigger};
use engine_core::protocol::ProtocolVersion;

use crate::clock::test_clock::TestClock;
use crate::log::{Manifest, Offset, segment_seq};
use crate::test_support::{
    SEGMENT_COMMANDS, TRADER, TestSequencer, deposit, dev_config, log_entries, log_messages, sample_around,
    scratch_dir, sequencer_config, trusted_now, withdraw,
};
use tokio::sync::oneshot;

const V1_DEV: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/v1-dev.jsonl");
const V3_REFERENCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v3.jsonl");

#[tokio::test]
async fn one_withdrawal_puts_command_withdrawal_and_checkpoint_at_one_seq() {
    let dir = scratch_dir("seq-offsets");
    let node = TestSequencer::start_legacy(&dir, 1024);
    let mut live = node.stream.subscribe();
    node.submit(deposit(100, "dep-1")).await;
    let seq = node.tick_to_checkpoint().await;
    node.submit(withdraw(40)).await;

    let at_seq: Vec<_> = std::iter::from_fn(|| live.try_recv().ok()).filter(|m| m.offset.seq == seq).collect();
    let kinds: Vec<_> = at_seq
        .iter()
        .map(|m| match &m.body {
            StreamBody::Command(_) => (m.offset.index, "command"),
            StreamBody::Withdrawal(_) => (m.offset.index, "withdrawal"),
            StreamBody::Checkpoint(_) => (m.offset.index, "checkpoint"),
        })
        .collect();
    assert_eq!(kinds, [(0, "command"), (1, "checkpoint"), (2, "withdrawal")]);
    // The stream and the log carry the same messages at the same offsets.
    let published: Vec<_> = at_seq.iter().map(|m| serde_json::to_string(&**m).unwrap()).collect();
    let logged = log_messages(&dir, Offset::start_of(0));
    let start = logged.iter().position(|m| *m == published[0]).expect("command on disk");
    assert_eq!(logged[start..start + 3], published);
}

#[test]
fn custody_gets_a_withdrawal_when_its_batch_is_flushed_not_when_it_is_signed() {
    let dir = scratch_dir("seq-custody");
    let (stream, _) = broadcast::channel(16);
    let (withdrawals_tx, mut withdrawals) = mpsc::unbounded_channel();
    let key = Arc::new(EnclaveKey::test_key());
    let manifest = watch::channel(Manifest::default()).0;
    let mut seq = Sequencer::recover(sequencer_config(&dir, manifest), key, stream, withdrawals_tx).unwrap();
    seq.trust(trusted_now()).unwrap();
    seq.sequence(deposit(100, "dep-1")).unwrap();
    seq.sequence(withdraw(40)).unwrap();
    assert!(withdrawals.try_recv().is_err());
    seq.flush().unwrap();
    assert_eq!(withdrawals.try_recv().unwrap().nonce, 1);
}

#[tokio::test]
async fn replay_refuses_logged_events_or_withdrawals_that_replay_does_not_produce() {
    let dir = scratch_dir("seq-tamper");
    let node = TestSequencer::start_v18(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.submit(withdraw(40)).await;
    node.stop().await;
    let entries = log_entries(&dir);
    replay(&entries).unwrap();

    let mut altered = entries.clone();
    let deposit_at = altered
        .iter()
        .position(|e| matches!(e, LogEntry::Command(c) if matches!(c.sequenced.cmd, Command::Deposit { .. })))
        .unwrap();
    if let LogEntry::Command(c) = &mut altered[deposit_at] {
        c.events = Some(Vec::new());
    }
    let err = replay(&altered).err().unwrap();
    assert!(format!("{err:#}").contains("invalid command origin signature"), "{err:#}");
    // Without its seal, as in a historical entry, replay still compares the logged events.
    if let LogEntry::Command(c) = &mut altered[deposit_at] {
        c.seal = None;
    }
    let err = replay(&altered).err().unwrap();
    assert!(format!("{err:#}").contains("events logged at seq"), "{err:#}");

    let withdrawal_at = entries.iter().position(|e| matches!(e, LogEntry::Withdrawal(_))).unwrap();
    let mut doubled = entries.clone();
    doubled.insert(withdrawal_at, entries[withdrawal_at].clone());
    let err = replay(&doubled).err().unwrap();
    assert!(format!("{err:#}").contains("was not approved"), "{err:#}");

    let mut forged = entries.clone();
    if let LogEntry::Withdrawal(w) = &mut forged[withdrawal_at] {
        w.amount = Fixed::from_int(41);
    }
    let err = replay(&forged).err().unwrap();
    assert!(format!("{err:#}").contains("unauthorized withdrawal completion signer"), "{err:#}");
}

#[tokio::test]
async fn a_restarted_sequencer_continues_the_log_offsets_and_segments() {
    let dir = scratch_dir("seq-restart");
    let node = TestSequencer::start_legacy(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.stop().await;
    let before = replay(&log_entries(&dir)).unwrap().engine.last_seq().unwrap();

    let node = TestSequencer::start_legacy(&dir, 16);
    let mut live = node.stream.subscribe();
    for n in 0..SEGMENT_COMMANDS {
        node.submit(deposit(1, &format!("dep-after-{n}"))).await;
    }
    assert_eq!(live.try_recv().unwrap().offset, Offset::start_of(before + 1));
    node.stop().await;

    let replayed = replay(&log_entries(&dir)).unwrap();
    assert_eq!(replayed.engine.last_seq(), Some(before + SEGMENT_COMMANDS));
    assert!(Manifest::load(&dir).unwrap().segments.len() >= 2);
}

#[tokio::test]
async fn replay_refuses_an_approved_withdrawal_whose_signed_entry_is_missing() {
    let dir = scratch_dir("seq-missing-withdrawal");
    let node = TestSequencer::start_v18(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.submit(withdraw(40)).await;
    node.submit(Command::Tick).await;
    node.stop().await;
    let mut entries = log_entries(&dir);
    let at = entries.iter().position(|e| matches!(e, LogEntry::Withdrawal(_))).unwrap();
    entries.remove(at);
    let err = replay(&entries).err().unwrap();
    assert!(format!("{err:#}").contains("has no signed entry"), "{err:#}");
}

#[tokio::test]
async fn a_restart_signs_a_withdrawal_the_log_ends_before_and_hands_it_to_custody() {
    let dir = scratch_dir("seq-resign");
    let mut node = TestSequencer::start_v18(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.submit(withdraw(40)).await;
    assert_eq!(node.withdrawals.try_recv().unwrap().nonce, 1);
    node.stop().await;
    // As if the process died after the withdraw command's line reached disk and before the rest.
    let segment = last_segment(&dir);
    let text = fs::read_to_string(&segment).unwrap();
    let command = text.find(r#""cmd":{"type":"withdraw""#).unwrap();
    fs::write(&segment, &text[..command + text[command..].find('\n').unwrap() + 1]).unwrap();
    let approved = (TRADER, Fixed::from_int(40), 1);
    assert_eq!(replay(&log_entries(&dir)).unwrap().unsigned, [approved]);

    let mut node = TestSequencer::start_v18(&dir, 16);
    let signed = node.withdrawals.try_recv().unwrap();
    assert_eq!((signed.account, signed.amount, signed.nonce), approved);
    node.stop().await;
    assert!(replay(&log_entries(&dir)).unwrap().unsigned.is_empty());
}

#[tokio::test]
async fn replay_refuses_a_command_without_logged_events_after_ones_with_them() {
    let dir = scratch_dir("seq-events-cutover");
    let node = TestSequencer::start_legacy(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.stop().await;
    let mut entries = log_entries(&dir);
    let last_command = entries.iter().rposition(|e| matches!(e, LogEntry::Command(_))).unwrap();
    if let LogEntry::Command(c) = &mut entries[last_command] {
        c.events = None;
    }
    let err = replay(&entries).err().unwrap();
    assert!(format!("{err:#}").contains("has no logged events, though earlier commands do"), "{err:#}");
}

#[tokio::test]
async fn replay_refuses_a_checkpoint_after_an_approval_whose_signed_entry_is_missing() {
    let dir = scratch_dir("seq-missing-withdrawal-checkpoint");
    let node = TestSequencer::start_v18(&dir, 16);
    node.submit(deposit(100, "dep-1")).await;
    node.tick_to_checkpoint().await;
    node.submit(withdraw(40)).await;
    node.stop().await;
    let mut entries = log_entries(&dir);
    assert!(matches!(entries.last(), Some(LogEntry::Checkpoint(_))));
    let at = entries.iter().position(|e| matches!(e, LogEntry::Withdrawal(_))).unwrap();
    entries.remove(at);
    let err = replay(&entries).err().unwrap();
    assert!(format!("{err:#}").contains("has no signed entry"), "{err:#}");
}

#[test]
fn a_fresh_log_records_client_auth_and_the_operator_and_activates_the_latest_version_before_genesis() {
    let dir = scratch_dir("activation-fresh");
    let mut seq = recover(&dir, &signed(31_337));
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    let entries = log_entries(&dir);
    let cmds = commands(&entries);
    let operator = dev_config().operator().unwrap();
    assert_eq!(cmds[0], Command::Admin(AdminCommand::SetClientAuth(signed(31_337))));
    assert_eq!(cmds[1], Command::Admin(AdminCommand::SetOperator(operator)));
    assert_eq!(
        cmds[2],
        Command::Admin(AdminCommand::ActivateCustodyFinality {
            version: ProtocolVersion::LATEST,
            legacy_deposits: vec![],
        })
    );
    let replayed = replay(&entries).unwrap();
    assert_eq!(replayed.engine.protocol_version(), ProtocolVersion::LATEST);
    assert_eq!(replayed.engine.client_auth(), Some(&signed(31_337)));
    assert_eq!(replayed.engine.operator(), Some(&operator));
}

#[test]
fn an_existing_v1_log_activates_at_its_next_seq_and_still_verifies() {
    let dir = scratch_dir("activation-v1");
    fs::create_dir_all(&dir).unwrap();
    // A log written as one file before segments existed continues as the first segment.
    crate::sequencer::fixture_tests::copy_fixture(V1_DEV, dir.join("00000000000000000000.jsonl"));
    let logged = log_entries(&dir);
    let before = replay(&logged).unwrap().engine.params().cloned().expect("the log sets parameters");
    let mut seq = recover_with(&dir, &signed(31_337), finished_genesis(&logged)).unwrap();
    let weighting = dev_config().genesis.params.toxicity.notional.unwrap();
    seq.carry_toxicity_notional(weighting).unwrap();
    seq.activate_protocol().unwrap();
    // The original 63 commands and 3 checkpoints, then the config's notional weighting and its
    // checkpoint, then client auth, the operator, the carried ceilings, and the activation, with
    // the scheduled checkpoint after the operator and the activation's own.
    let replayed = replay(&log_entries(&dir)).unwrap();
    assert_eq!((replayed.commands, replayed.checkpoints), (68, 6));
    assert_eq!(replayed.activations, [(ProtocolVersion::LATEST, 67)]);
    // Activation also turns its absolute divergence triggers of 15 and 20 bps into 60% and 80% of its
    // widest bound, 25 bps: the shares the dev config takes.
    let mut carried = before;
    carried.toxicity.notional = Some(weighting);
    carried.share_divergence_triggers(25);
    assert_eq!(replayed.engine.params(), Some(&carried));
    let triggers = |params: &ProtocolParams| -> Vec<Option<StressTrigger>> {
        params.stress_levels.iter().map(|level| level.trigger.clone()).collect()
    };
    assert_eq!(triggers(&carried), triggers(&dev_config().genesis.params));
}

#[test]
fn protocol_activation_and_genesis_wait_for_trusted_time_then_go_in_order() {
    let dir = scratch_dir("boot-waits");
    let mut seq = try_recover(&dir, &signed(31_337)).0.unwrap();
    seq.activate_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    assert!(commands(&log_entries(&dir)).is_empty(), "nothing before the first sample");
    seq.trust(trusted_now()).unwrap();
    let written = commands(&log_entries(&dir));
    assert!(matches!(written[0], Command::Admin(AdminCommand::SetClientAuth(_))), "{:?}", written[0]);
    assert!(matches!(written[1], Command::Admin(AdminCommand::SetOperator(_))), "{:?}", written[1]);
    assert_eq!(
        written[2],
        Command::Admin(AdminCommand::ActivateCustodyFinality {
            version: ProtocolVersion::LATEST,
            legacy_deposits: vec![],
        })
    );
    assert!(matches!(written.last(), Some(Command::HouseCapital { .. })), "{written:?}");
    assert!(!seq.needs_genesis());
}

#[test]
fn a_v3_log_takes_the_state_tree_at_its_next_seq_and_still_verifies() {
    let dir = scratch_dir("activation-v3");
    fs::create_dir_all(&dir).unwrap();
    crate::sequencer::fixture_tests::copy_fixture(V3_REFERENCE, dir.join("00000000000000000000.jsonl"));
    let logged = log_entries(&dir);
    let before = replay(&logged).unwrap();
    // As the node does, the log's parameters take the config's notional weighting at its next seq;
    // the operator follows, and the latest version after it.
    let activated = before.engine.last_seq().unwrap() + 4;
    // The log is stamped ahead of the system clock, so the host clock and trusted time agree on a
    // time just after its last command.
    let stamps = Arc::new(TestClock::at(before.engine.now() + 1_000));
    let (stream, _) = broadcast::channel(16);
    let (manifest, _) = watch::channel(Manifest::default());
    let mut operator = dev_config().operator().unwrap();
    operator.vault = AccountId([0xaa; 20]);
    // The V3 log lists markets the dev config never ceilinged; `V39` needs an adopted ceiling for
    // each, so the config this test runs under names one per listed market.
    let ceilings: Vec<_> = before
        .engine
        .markets()
        .keys()
        .map(|market| CloseSpreadCeiling { market: *market, max_close_spread_bps: 30 })
        .collect();
    let cfg = SequencerConfig {
        client_auth: signed(31_337),
        vault: operator.vault,
        operator,
        clock: stamps.clone(),
        genesis: finished_genesis(&logged),
        close_spread_ceilings: ceilings,
        ..sequencer_config(&dir, manifest)
    };
    let key = Arc::new(EnclaveKey::test_key());
    let mut seq = Sequencer::recover(cfg, key, stream, mpsc::unbounded_channel().0).unwrap();
    seq.trust(sample_around(stamps.as_ref())).unwrap();
    seq.carry_toxicity_notional(dev_config().genesis.params.toxicity.notional.unwrap()).unwrap();
    seq.activate_protocol().unwrap();
    // The V3 checkpoints still verify under the hash of the state encoding, and the activation's
    // checkpoint under the state tree, whose proofs hold against it.
    let written = log_entries(&dir);
    let replayed = replay(&written).unwrap();
    assert_eq!(replayed.activations, [(ProtocolVersion::V3, 1), (ProtocolVersion::LATEST, activated)]);
    assert!(replayed.checkpoints > before.checkpoints);
    let Some(LogEntry::Checkpoint(last)) = written.last() else { panic!("the activation is checkpointed") };
    let account = *replayed.engine.accounts().keys().next().unwrap();
    let proof = replayed.engine.account_proof(account).unwrap();
    assert_eq!((proof.seq, proof.verify(last.root.parse().unwrap())), (activated, true));
}

#[test]
fn a_restart_with_the_recorded_client_auth_sequences_nothing() {
    let dir = scratch_dir("activation-restart");
    recover(&dir, &signed(31_337)).activate_protocol().unwrap();
    let written = log_entries(&dir).len();
    recover(&dir, &signed(31_337)).activate_protocol().unwrap();
    assert_eq!(log_entries(&dir).len(), written);
}

#[test]
fn a_log_holding_only_client_auth_activates_on_restart() {
    let dir = scratch_dir("activation-auth-only");
    let client_auth = ClientAuth::Eip712 { chain_id: 31337, vault: AccountId([0; 20]) };
    let auth = Command::Admin(AdminCommand::SetClientAuth(client_auth.clone()));
    recover(&dir, &client_auth).sequence_required("client auth", vec![auth.clone()]).unwrap();
    recover(&dir, &client_auth).activate_protocol().unwrap();
    let written = log_entries(&dir);
    assert_eq!(commands(&written).iter().filter(|cmd| **cmd == auth).count(), 1);
    assert_eq!(replay(&written).unwrap().engine.protocol_version(), ProtocolVersion::LATEST);
    recover(&dir, &client_auth).activate_protocol().unwrap();
    assert_eq!(log_entries(&dir).len(), written.len());
}

#[test]
fn a_log_recording_other_client_auth_refuses_to_start_and_writes_nothing() {
    let dir = scratch_dir("activation-mismatch");
    recover(&dir, &signed(31_337)).activate_protocol().unwrap();
    let written = log_entries(&dir).len();
    for other in [ClientAuth::Unsigned, signed(1)] {
        let err = try_recover(&dir, &other).0.err().unwrap();
        assert!(format!("{err:#}").contains("the log records client auth"), "{err:#}");
    }
    assert_eq!(log_entries(&dir).len(), written);
}

#[test]
fn a_restart_under_other_client_auth_signs_no_withdrawal_the_log_ends_before() {
    let dir = scratch_dir("activation-mismatch-withdrawal");
    let mut seq = recover(&dir, &ClientAuth::Unsigned);
    seq.activate_legacy_protocol().unwrap();
    seq.genesis(&dev_config().genesis).unwrap();
    seq.sequence(deposit(100, "dep-1")).unwrap();
    seq.sequence(withdraw(40)).unwrap();
    seq.flush().unwrap();
    drop(seq);
    // As if the process died after the withdraw command's line reached disk and before the rest.
    let segment = last_segment(&dir);
    let text = fs::read_to_string(&segment).unwrap();
    let command = text.find(r#""cmd":{"type":"withdraw""#).unwrap();
    let cut = &text[..command + text[command..].find('\n').unwrap() + 1];
    fs::write(&segment, cut).unwrap();

    let (recovered, mut withdrawals) = try_recover(&dir, &signed(31_337));
    let err = recovered.err().unwrap();
    assert!(format!("{err:#}").contains("the log records client auth"), "{err:#}");
    assert!(withdrawals.try_recv().is_err(), "a withdrawal was signed under the other chain and vault");
    assert_eq!(fs::read_to_string(&segment).unwrap(), cut);
}

#[tokio::test]
async fn a_sync_request_is_answered_after_the_commands_before_it_are_written() {
    let dir = scratch_dir("seq-sync");
    let node = TestSequencer::start_legacy(&dir, 16);
    let tick_seq = node.next_seq().await;
    node.tx.send(Request::Submit { cmd: Command::Tick, reply: None }).await.unwrap();
    let (reply, synced) = oneshot::channel();
    node.tx.send(Request::Sync(reply)).await.unwrap();

    let answer = tokio::time::timeout(std::time::Duration::from_secs(5), synced).await;
    assert_eq!(answer.expect("the sync is answered").unwrap(), Some(tick_seq));
    assert!(!log_messages(&dir, Offset::start_of(tick_seq)).is_empty(), "the tick is on disk");
    node.stop().await;
}

/// Recovery and `engine replay` replay each entry as they read it: an entry replay refuses fails
/// the read before it reaches a later segment, here one that cannot be read.
#[test]
fn recovery_and_engine_replay_replay_the_log_as_they_read_it() {
    let dir = scratch_dir("seq-replay-streams");
    let command = |seq, events| {
        LogEntry::Command(CommandEntry {
            seal: None,
            sequenced: Sequenced { seq, ts: 0, cmd: Command::Tick },
            events: Some(events),
        })
    };
    let manifest = watch::channel(Manifest::default()).0;
    let mut log = SegmentedLog::open(&dir, |_| Ok(())).unwrap().into_writer(2, manifest).unwrap();
    // Seq 1, in the first segment, logs an event its replay does not give.
    for seq in 0..5 {
        let events = if seq == 1 { vec![Event::AdminApplied] } else { Vec::new() };
        log.append(&command(seq, events)).unwrap();
    }
    log.flush().unwrap();
    drop(log);
    let newest = last_segment(&dir);
    fs::remove_file(&newest).unwrap();
    fs::create_dir(&newest).unwrap();

    let recovery = Sequencer::read_log(&dir, |_| {}).err().unwrap();
    let replayed = replay_path(&dir).err().unwrap();
    for (reader, err) in [("recovery", recovery), ("engine replay", replayed)] {
        assert!(format!("{err:#}").contains("events logged at seq 1 differ"), "{reader}: {err:#}");
    }
}

// ── Helpers ──

/// The segment file the log appends to.
fn last_segment(dir: &Path) -> PathBuf {
    let mut names: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| segment_seq(name).is_some())
        .collect();
    names.sort();
    dir.join(names.last().unwrap())
}

/// Signed requests under the vault `0xaa…` on `chain_id`.
fn signed(chain_id: u64) -> ClientAuth {
    ClientAuth::Eip712 { chain_id, vault: AccountId([0xaa; 20]) }
}

/// A sequencer recovered from the log in `dir` under `client_auth`, with a fresh enclave key, and
/// trusted time admitting stamps on the system clock.
fn recover(dir: &Path, client_auth: &ClientAuth) -> Sequencer {
    let mut seq = try_recover(dir, client_auth).0.unwrap();
    seq.trust(trusted_now()).unwrap();
    seq
}

/// As `recover`, under a config whose genesis sequences `genesis`.
/// Error cases:
/// - recovery fails (see `Sequencer::recover`)
fn recover_with(dir: &Path, client_auth: &ClientAuth, genesis: Vec<Command>) -> anyhow::Result<Sequencer> {
    let (stream, _) = broadcast::channel(16);
    let (withdrawals_tx, _) = mpsc::unbounded_channel();
    let (manifest, _) = watch::channel(Manifest::default());
    let cfg = SequencerConfig { client_auth: client_auth.clone(), genesis, ..sequencer_config(dir, manifest) };
    let mut seq = Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), stream, withdrawals_tx)?;
    seq.trust(trusted_now())?;
    Ok(seq)
}

/// `recover`'s result, and what custody would receive from it.
fn try_recover(
    dir: &Path,
    client_auth: &ClientAuth,
) -> (anyhow::Result<Sequencer>, mpsc::UnboundedReceiver<SignedWithdrawal>) {
    let (stream, _) = broadcast::channel(16);
    let (withdrawals_tx, withdrawals) = mpsc::unbounded_channel();
    let (manifest, _) = watch::channel(Manifest::default());
    let cfg = SequencerConfig { client_auth: client_auth.clone(), ..sequencer_config(dir, manifest) };
    (Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), stream, withdrawals_tx), withdrawals)
}

/// The commands among `entries`, in order.
fn commands(entries: &[LogEntry]) -> Vec<Command> {
    entries
        .iter()
        .filter_map(|e| match e {
            LogEntry::Command(c) => Some(c.sequenced.cmd.clone()),
            _ => None,
        })
        .collect()
}

mod benchmark_recovery;
mod benchmark_schedule;
mod benchmarks;
mod catch_up;
mod genesis;
mod operator;
mod rotation;
mod toxicity;

mod access_tests;
mod replay;
