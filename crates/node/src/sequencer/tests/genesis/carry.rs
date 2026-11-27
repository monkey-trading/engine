//! A carried genesis is all or nothing too: parts that apply but leave the carry unfinished, and a genesis the
//! stamp it takes refuses, are refused before anything is written, and a log whose genesis credited its house
//! capital with a carry still open is refused at recovery. A log that carried stakes before `V93` refused them
//! still recovers: its parts are judged under its own rules, not the latest.

use std::path::Path;

use engine_core::Engine;
use engine_core::carry::CarryPart;
use engine_core::command::{Command, Sequenced};
use engine_core::staking::StakingConfig;

use super::*;
use crate::log::LogEntry;
use crate::sequencer::GenesisProgress;
use crate::sequencer::carried_genesis_commands;
use crate::sequencer::fixture_tests::{replay_trusted, signed_fixture};
use crate::sequencer::genesis::is_carry_part;
use crate::sequencer::required::activation_commands;

/// The node's reference log whose genesis carries the reference session's state, a source at its last checkpoint.
const CARRIED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v90-carried.jsonl");
/// The node's reference log whose genesis carried the reference session's state, stakes included, under `V92`.
const CARRIED_V92: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v92-carried.jsonl");

/// The dev genesis, staking from the start.
fn staked_genesis() -> Genesis {
    let mut genesis = dev_config().genesis;
    genesis.staking = Some(StakingConfig {
        liquidity_floor: engine_core::Fixed::from_int(1),
        epoch_ms: 86_400_000,
        start_ms: 0,
        cooldown_ms: 3 * 86_400_000,
    });
    genesis
}

/// The carry of `CARRIED`'s last checkpoint, without its `End`.
fn unfinished() -> Vec<CarryPart> {
    let mut parts = crate::carry::build(Path::new(CARRIED), None).unwrap().parts;
    assert_eq!(parts.pop(), Some(CarryPart::End));
    parts
}

#[test]
fn parts_that_apply_but_leave_the_carry_unfinished_write_no_genesis() {
    let dir = scratch_dir("genesis-carry-unfinished");
    let mut seq = recover(&dir, &signed(31_337));
    seq.activate_protocol().unwrap();
    let activated = log_entries(&dir);
    let err = seq.carried_genesis(&staked_genesis(), &unfinished()).unwrap_err();
    assert!(format!("{err:#}").contains("do not finish"), "{err:#}");
    drop(seq);
    assert_eq!(log_entries(&dir), activated, "no genesis command reached the log");
}

/// The dev genesis's staking starts at 0: the start's check at 0 passes, but the stamp trusted time admits is past
/// the start, which `InitStaking` refuses. The genesis pages Critical `carry_refused` and writes nothing.
#[test]
fn a_carried_genesis_its_stamp_refuses_writes_no_genesis() {
    let (_guard, mut alerts) = crate::test_support::capture_alerts();
    let dir = scratch_dir("genesis-carry-stamp");
    let mut seq = recover(&dir, &signed(31_337));
    seq.activate_protocol().unwrap();
    let activated = log_entries(&dir);
    let parts = crate::carry::build(Path::new(CARRIED), None).unwrap().parts;
    let err = seq.carried_genesis(&staked_genesis(), &parts).unwrap_err();
    assert!(format!("{err:#}").contains("the carried genesis at its stamp"), "{err:#}");
    drop(seq);
    assert_eq!(log_entries(&dir), activated, "no genesis command reached the log");
    let paged: Vec<_> = std::iter::from_fn(|| alerts.try_recv().ok())
        .map(|alert| alert["alert"].clone())
        .filter(|alert| alert.starts_with("carry_"))
        .collect();
    assert_eq!(paged, ["carry_refused"]);
}

#[test]
fn a_genesis_credited_with_its_carry_still_open_is_refused_at_recovery() {
    let mut engine = Engine::new();
    let activation = activation_commands(&engine, &signed(31_337), dev_config().operator().unwrap(), Vec::new());
    let cmds = activation.into_iter().chain(carried_genesis_commands(&staked_genesis(), &unfinished()));
    let mut progress = GenesisProgress::default();
    for (seq, cmd) in (0..).zip(cmds) {
        let sequenced = Sequenced { seq, ts: 0, cmd };
        let events = engine.apply(&sequenced).unwrap();
        assert!(rejection(&events).is_none(), "{events:?}");
        progress.command(&sequenced, &events);
    }
    progress.checkpoint();
    let err = progress.ensure_closed().unwrap_err();
    assert!(format!("{err:#}").contains("did not end"), "{err:#}");
    assert!(engine.carry_progress().is_some());
}

#[test]
fn a_log_that_carried_stakes_before_v93_still_matches_its_genesis_under_the_latest_rules() {
    let entries = signed_fixture(Path::new(CARRIED_V92));
    let stakes = |entry: &LogEntry| match entry {
        LogEntry::Command(command) => {
            matches!(command.sequenced.cmd, Command::Admin(AdminCommand::CarryState(CarryPart::Stakes { .. })))
        }
        _ => false,
    };
    assert!(entries.iter().any(stakes), "the log carried stakes, which `V93` refuses");
    let replayed = replay_trusted(&entries).unwrap();
    let (cfg, operator) = (dev_config(), dev_config().operator().unwrap());
    let ceilings = cfg.genesis.close_spread_ceilings;
    let notional = cfg.genesis.params.toxicity.notional;
    let genesis: Vec<_> =
        replayed.genesis.as_config(notional, &ceilings).into_iter().filter(|cmd| !is_carry_part(cmd)).collect();
    replayed.genesis.ensure_matches(&genesis, &ceilings, &signed(31_337), operator).unwrap();
}
