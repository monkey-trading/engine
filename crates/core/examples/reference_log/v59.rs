//! The session's `V59` part: when votes govern. A vote activates `V59`, which takes the turn in
//! effect's boundary as when turns name the operator; with the switch off that turn still names it,
//! until an installation; with the switch on again a set is in effect; a turn drawn the day before
//! its boundary names the operator only from the boundary, which the staking frontier carries too; and
//! a proposal the registry passes with no turn the engine sees shows one, as a turn refused before
//! `V59` would.

use engine_core::command::{AdminCommand, Command};
use engine_core::election::RegistryInstallation;
use engine_core::event::Event;
use engine_core::governance::{PassedProposal, VotedChange, command_hash};
use engine_core::market::MarketSpec;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{DAY_MS, RegistryTurn, TurnReason, boundary};

use crate::common::{BTC, Client, ETH, Harness};
use crate::v51::{MEMBER_SEEDS, MEMBERS};

/// Votes govern under `V58`, so a passed proposal activates `V59`. The turn in effect names the operator
/// from its boundary. With the switch off, the turn's key's house cap change is refused until an
/// installation ends the turns, then applies; with the switch on again it is refused. A turn mirrored
/// the day before its boundary, with the switch then off, leaves the change to the key until the
/// boundary. After another installation, a fast-path stress floor the registry passes applies, and
/// turns name the operator from then.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 };
    let events = pass(h, 14, false, activation.clone());
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { proposal: 14, .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let applied = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 14, command: activation })));
    assert_eq!(applied.last(), Some(&Event::AdminApplied));
    let in_effect = h.engine.daily_turns().in_effect.expect("`V53`'s part leaves a turn in effect");
    assert_eq!(h.engine.governance().turns_from, boundary(in_effect.turn));

    switch(h, false, 0);
    refused(&house_cap(h, -1));
    install(h, 0);
    assert_eq!(house_cap(h, -1).last(), Some(&Event::AdminApplied));
    switch(h, true, 1);
    refused(&house_cap(h, -2));

    let turn = (0..).find(|turn| boundary(*turn).expect("in range") - DAY_MS > h.ts).expect("a later turn");
    let begins = boundary(turn).expect("in range");
    h.advance_to(begins - DAY_MS);
    let drawn = RegistryTurn {
        turn,
        set: 2,
        position: 0,
        reason: TurnReason::Draw,
        candidate: MEMBERS[0],
        admin_key: Client::new(MEMBER_SEEDS[0]).account,
        block: 59_000_020,
        chain_ref: "0xturn:59:0".into(),
    };
    let events = h.cmd(Command::AssignTurn(drawn));
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
    switch(h, false, 2);
    assert_eq!(house_cap(h, -2).last(), Some(&Event::AdminApplied));
    h.advance_to(begins);
    refused(&house_cap(h, -3));
    // The staking frontier past the boundary carries the turn the registry names the operator by.
    let block_hash = alloy_primitives::B256::repeat_byte(59);
    let progress = Command::StakingProgress {
        block: 59_000_150,
        block_hash,
        block_time: Some(h.ts),
        named_turn: Some(Some(begins)),
    };
    assert_eq!(h.cmd(progress), []);
    assert_eq!(h.engine.governance().turns_from, Some(begins));

    h.advance(DAY_MS);
    install(h, 1);
    let floor = AdminCommand::SetStressFloor { market: Some(BTC), level: 1 };
    let events = pass(h, 15, true, floor);
    assert!(events.iter().any(|event| matches!(event, Event::RestrictionApplied { proposal: 15, .. })), "{events:?}");
    assert_eq!(h.engine.governance().turns_from, Some(h.ts));
}

/// ETH's baseline listing with its house cap moved by `delta` bps, signed by the turn's key.
fn house_cap(h: &mut Harness, delta: i64) -> Vec<Event> {
    let spec = h.engine.baseline_spec(ETH).expect("listed");
    let listing = AdminCommand::UpsertMarket(MarketSpec { house_cap_bps: spec.house_cap_bps + delta, ..spec });
    h.operate_as(MEMBER_SEEDS[0], listing)
}

fn refused(events: &[Event]) {
    assert!(
        matches!(events.last(), Some(Event::AdminRejected { reason }) if reason.contains("only a passed vote changes this")),
        "{events:?}"
    );
}

/// Mirrors the registry turning rotation on or off, the `n`th switch of this part.
fn switch(h: &mut Harness, enabled: bool, n: u64) {
    let (block, chain_ref) = (59_000_000 + n, format!("0xrotation:59:{n}"));
    let events = h.cmd(Command::SwitchRotation { enabled, election_cancelled: false, block, chain_ref });
    assert!(matches!(events.last(), Some(Event::RotationSwitched { .. })), "{events:?}");
}

/// Mirrors the timelock installing the turn's member under its own key, the `n`th installation of this
/// part.
fn install(h: &mut Harness, n: u64) {
    let (candidate, admin_key) = (MEMBERS[0], Client::new(MEMBER_SEEDS[0]).account);
    let (block, chain_ref) = (59_000_010 + n, format!("0xinstall:59:{n}"));
    let events = h.cmd(Command::InstallOperator(RegistryInstallation { candidate, admin_key, block, chain_ref }));
    assert!(matches!(events.last(), Some(Event::OperatorInstalled { .. })), "{events:?}");
}

/// Mirrors proposal `proposal` passing on `fast` with `command`.
fn pass(h: &mut Harness, proposal: u64, fast: bool, command: AdminCommand) -> Vec<Event> {
    let (block, chain_ref) = (59_000_100 + proposal, format!("0xvote:59:{proposal}"));
    let command_hash = command_hash(&command);
    let passed = PassedProposal { proposal, fast, command_hash, command: Some(command), block, chain_ref };
    h.cmd(Command::PassProposal(Box::new(passed)))
}
