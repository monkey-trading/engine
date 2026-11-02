//! The parameters and latest checkpoint `GET /v1/house/state` answers from memory: before either
//! exists, as the log sets and replaces them, and across a restart.

use super::*;

use engine_core::eip712;
use k256::ecdsa::SigningKey;

use crate::admin::sign;
use crate::clock::Clock;
use crate::sequencer::house::HouseState;

const START: Millis = 1_792_530_000_000;

/// What the sequencer would answer a read with now, once the batch is flushed.
fn read(seq: &mut Sequencer) -> HouseState {
    let (reply, mut answer) = oneshot::channel();
    seq.house.wait(reply);
    seq.flush().unwrap();
    answer.try_recv().expect("a flush answers the waiting read")
}

/// A sequencer recovered from the log in `dir` on a test clock, with genesis run when `genesis`.
fn start(dir: &Path, clock: &Arc<TestClock>, genesis: bool) -> Sequencer {
    let finished = finished_genesis(&log_entries(dir));
    let cfg = SequencerConfig {
        clock: clock.clone(),
        genesis: finished,
        ..sequencer_config(dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(
        cfg,
        Arc::new(EnclaveKey::test_key()),
        broadcast::channel(16).0,
        mpsc::unbounded_channel().0,
    )
    .unwrap();
    if genesis {
        seq.activate_protocol().unwrap();
        seq.genesis(&dev_config().genesis).unwrap();
    }
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq
}

/// The operator signs `params` as the next admin command, returning the engine's events.
fn set_params(seq: &mut Sequencer, clock: &TestClock, params: ProtocolParams) -> Vec<Event> {
    let key = SigningKey::from_slice(&[0x42; 32]).unwrap();
    let nonce = seq.engine.next_request_nonce(eip712::address(key.verifying_key()));
    let signed = sign(&key, &seq.operator, AdminCommand::SetParams(params), nonce, clock.now_ms() + 150_000);
    seq.sequence(Command::SignedAdmin(Box::new(signed))).unwrap().expect("admitted")
}

/// The seq of the last command in `dir`'s log that is a `SetParams`, rejected or not.
fn last_set_params(dir: &Path) -> u64 {
    let logged = log_entries(dir);
    let mut sets = logged.iter().filter_map(|entry| match entry {
        LogEntry::Command(c) if crate::sequencer::house::set_params_of(&c.sequenced.cmd).is_some() => {
            Some(c.sequenced.seq)
        }
        _ => None,
    });
    sets.next_back().expect("the log holds a SetParams")
}

/// The stream's message for the last checkpoint in `dir`'s log, as it encodes.
fn logged_checkpoint(dir: &Path) -> String {
    log_messages(dir, Offset::start_of(0))
        .into_iter()
        .rfind(|message| message.contains(r#""type":"checkpoint""#))
        .expect("the log holds a checkpoint")
}

#[test]
fn a_log_with_no_command_has_neither_parameters_nor_a_checkpoint() {
    let dir = scratch_dir("house-state-empty");
    let (mut seq, _) = try_recover(&dir, &ClientAuth::Unsigned);
    let state = read(seq.as_mut().unwrap());
    let expected = serde_json::json!({ "seq": null, "params": null, "checkpoint": null });
    assert_eq!(serde_json::to_value(state).unwrap(), expected);
}

#[test]
fn the_parameters_carry_the_seq_that_set_them_and_a_later_set_params_replaces_them() {
    let dir = scratch_dir("house-state-params");
    let clock = Arc::new(TestClock::at(START));
    let mut seq = start(&dir, &clock, true);
    let state = read(&mut seq);
    let genesis_seq = last_set_params(&dir);
    let at = state.params.expect("genesis sets the parameters");
    assert_eq!((at.seq, &at.params), (Some(genesis_seq), seq.engine.params().unwrap()));
    assert_eq!(state.seq, seq.engine.last_seq());

    let mut replacement = at.params.clone();
    replacement.house_total_cap_bps += 100;
    assert_eq!(set_params(&mut seq, &clock, replacement.clone()), [Event::AdminApplied]);
    let replaced_at = seq.engine.last_seq().unwrap();
    assert!(replaced_at > genesis_seq);
    let state = read(&mut seq);
    let at = state.params.unwrap();
    assert_eq!((at.seq, at.params, state.seq), (Some(replaced_at), replacement.clone(), Some(replaced_at)));

    // A rejected SetParams changes nothing, so the seq stays with the parameters in force.
    let mut invalid = replacement.clone();
    invalid.stress_levels.clear();
    let events = set_params(&mut seq, &clock, invalid);
    assert!(matches!(&events[..], [Event::AdminRejected { .. }]), "{events:?}");
    let state = read(&mut seq);
    let at = state.params.unwrap();
    assert_eq!((at.seq, at.params), (Some(replaced_at), replacement));
    assert!(state.seq > Some(replaced_at));
}

#[test]
fn the_checkpoint_is_the_latest_one_logged_in_the_streams_form_and_none_before_the_first() {
    let dir = scratch_dir("house-state-checkpoint");
    let clock = Arc::new(TestClock::at(START));
    let mut seq = start(&dir, &clock, false);
    seq.sequence(deposit(100, "dep-1")).unwrap();
    assert!(read(&mut seq).checkpoint.is_none(), "no command has reached a checkpoint yet");

    seq.checkpoint().unwrap();
    let first = read(&mut seq).checkpoint.expect("a checkpoint was logged");
    assert_eq!(serde_json::to_string(&first).unwrap(), logged_checkpoint(&dir));
    assert_eq!(first.offset.seq, seq.engine.last_seq().unwrap());

    seq.sequence(deposit(5, "dep-2")).unwrap();
    seq.checkpoint().unwrap();
    let second = read(&mut seq).checkpoint.unwrap();
    assert!(second.offset > first.offset);
    assert_eq!(serde_json::to_string(&second).unwrap(), logged_checkpoint(&dir));
}

#[test]
fn a_restart_restores_the_parameters_seq_and_the_checkpoint_from_the_replay_it_runs() {
    let dir = scratch_dir("house-state-restart");
    let clock = Arc::new(TestClock::at(START));
    let mut seq = start(&dir, &clock, true);
    let mut replacement = seq.engine.params().unwrap().clone();
    replacement.house_total_cap_bps += 100;
    assert_eq!(set_params(&mut seq, &clock, replacement), [Event::AdminApplied]);
    seq.checkpoint().unwrap();
    // Commands after the checkpoint leave it behind the head, and the restart must find it there; the
    // schedule checkpoints every few commands, so go on until one lands without a checkpoint.
    let mut before = read(&mut seq);
    for n in 0.. {
        seq.sequence(deposit(100, &format!("dep-{n}"))).unwrap();
        before = read(&mut seq);
        if before.checkpoint.as_ref().is_some_and(|checkpoint| checkpoint.offset.seq < before.seq.unwrap()) {
            break;
        }
    }
    assert_eq!(before.params.as_ref().map(|at| at.seq), Some(Some(last_set_params(&dir))));
    drop(seq);

    let mut recovered = start(&dir, &clock, false);
    let after = read(&mut recovered);
    assert_eq!(serde_json::to_value(after).unwrap(), serde_json::to_value(before).unwrap());
}
