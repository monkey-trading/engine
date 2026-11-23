use std::path::{Path, PathBuf};

use engine_core::carry::CarryPart;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::types::AccountId;

use super::*;
use crate::log::{CommandEntry, LogEntry};
use crate::sequencer::fixture_tests::signed_fixture;
use crate::test_support::scratch_dir;

/// The node's reference log whose genesis carries the reference session's state, at its last checkpoint a source
/// that can be carried again.
const CARRIED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v90-carried.jsonl");
/// The node's reference log of the session through `V90`'s activation, with rotation on.
const SESSION: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v90.jsonl");

/// `entries`, one JSON line each, in a single log file under a scratch directory for `label`.
fn write_entries(label: &str, entries: &[LogEntry]) -> PathBuf {
    let dir = scratch_dir(label);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("log.jsonl");
    let mut file = fs::File::create(&path).unwrap();
    for entry in entries {
        writeln!(file, "{}", serde_json::to_string(entry).unwrap()).unwrap();
    }
    path
}

/// Checks that `file`'s parts apply in full after a fresh node's genesis staking, as a start's pre-run does.
fn apply_after_genesis(file: &CarryFile) {
    let mut cfg = crate::test_support::dev_config();
    cfg.genesis.staking = Some(engine_core::staking::StakingConfig {
        liquidity_floor: engine_core::Fixed::from_int(1),
        epoch_ms: 86_400_000,
        start_ms: 0,
        cooldown_ms: 3 * 86_400_000,
    });
    let cmds = crate::sequencer::carried_genesis_commands(&cfg.genesis, &file.parts);
    let engine =
        crate::sequencer::genesis::check_genesis_commands(&cfg.client_auth().unwrap(), cfg.operator().unwrap(), cmds)
            .unwrap();
    assert_eq!(engine.carried_from(), Some(file.source));
}

#[test]
fn a_carry_file_builds_from_a_source_logs_last_checkpoint_and_reads_back() {
    let file = build(Path::new(CARRIED), None).unwrap();
    assert!(matches!(file.parts.first(), Some(CarryPart::Begin { source, .. }) if *source == file.source));
    assert_eq!(file.parts.last(), Some(&CarryPart::End));
    assert!(file.totals.accounts > 0 && file.totals.awards > 0, "{:?}", file.totals);
    let out = scratch_dir("carry-file-round-trip").join("carry.json");
    fs::create_dir_all(out.parent().unwrap()).unwrap();
    let written = write(&file, &out).unwrap();
    let (read_back, sha256) = read(&out).unwrap();
    assert_eq!((read_back, sha256), (file.clone(), written));
    assert!(write(&file, &out).is_err(), "an existing file is never overwritten");
    // The same checkpoint named by its seq gives the same file.
    assert_eq!(build(Path::new(CARRIED), Some(file.source.seq)).unwrap(), file);
}

#[test]
fn a_source_whose_carry_would_lose_state_or_has_no_checkpoint_there_is_refused() {
    // The session's last checkpoint has rotation on, and its mirror short of the blocks it mirrored.
    let error = build(Path::new(SESSION), None).unwrap_err();
    assert!(format!("{error:#}").contains("cannot be carried: the source's staking mirror"), "{error:#}");
    let not_a_checkpoint = build(Path::new(CARRIED), Some(1)).unwrap_err();
    assert!(format!("{not_a_checkpoint:#}").contains("holds no checkpoint at seq 1"), "{not_a_checkpoint:#}");
}

#[test]
fn a_file_whose_source_is_not_its_first_parts_is_refused() {
    let mut file = build(Path::new(CARRIED), None).unwrap();
    file.source.seq += 1;
    let out = scratch_dir("carry-file-mismatched").join("carry.json");
    fs::create_dir_all(out.parent().unwrap()).unwrap();
    fs::write(&out, serde_json::to_vec(&file).unwrap()).unwrap();
    assert!(format!("{:#}", read(&out).unwrap_err()).contains("do not begin with its source"));
}

#[test]
fn the_replay_check_ties_a_carried_log_to_its_signed_source() {
    let source = write_entries("carry-replay-source", &signed_fixture(Path::new(CARRIED)));
    let file = build(&source, None).unwrap();
    apply_after_genesis(&file);
    let carried = write_entries("carry-replay-carried", &engine_core_parts(&file.parts));
    let from = check_carried(&carried, &source).unwrap();
    assert_eq!((from.source, from.accounts, from.awards), (file.source, file.totals.accounts, file.totals.awards));
    assert_eq!(from.signer, AccountId(crate::tee::EnclaveKey::test_key().address()));

    // A source whose checkpoint there is signed by no key it names is refused.
    let mut forged = signed_fixture(Path::new(CARRIED));
    for entry in &mut forged {
        if let LogEntry::Checkpoint(checkpoint) = entry
            && checkpoint.seq == file.source.seq
        {
            checkpoint.signer = format!("{}", AccountId([0x5e; 20]));
        }
    }
    let forged = write_entries("carry-replay-forged", &forged);
    assert!(format!("{:#}", check_carried(&carried, &forged).unwrap_err()).contains("checkpoint"));

    // A carried log that left a stake out of its genesis carried other values than the source's state.
    let mut short = file.clone();
    for part in &mut short.parts {
        if let CarryPart::Stakes { entries } = part {
            entries.pop();
        }
    }
    let entries: Vec<LogEntry> = engine_core_parts(&short.parts);
    let partial = write_entries("carry-replay-partial", &entries);
    let error = check_carried(&partial, &source).unwrap_err();
    assert!(format!("{error:#}").contains("other values"), "{error:#}");
}

/// `parts` as unapplied command entries, which `check_carried` reads without replaying.
fn engine_core_parts(parts: &[CarryPart]) -> Vec<LogEntry> {
    (0..)
        .zip(parts)
        .map(|(seq, part)| {
            let cmd = Command::Admin(AdminCommand::CarryState(part.clone()));
            LogEntry::Command(CommandEntry { sequenced: Sequenced { seq, ts: 0, cmd }, events: None, seal: None })
        })
        .collect()
}
