//! A carried genesis is all or nothing too: parts that apply but leave the carry unfinished are refused before
//! anything is written, and a log whose genesis credited its house capital with a carry still open is refused
//! at recovery.

use std::path::Path;

use engine_core::Engine;
use engine_core::carry::CarryPart;
use engine_core::command::Sequenced;
use engine_core::staking::StakingConfig;

use super::*;
use crate::sequencer::GenesisProgress;
use crate::sequencer::carried_genesis_commands;
use crate::sequencer::required::activation_commands;

/// The node's reference log whose genesis carries the reference session's state, a source at its last checkpoint.
const CARRIED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v90-carried.jsonl");

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
