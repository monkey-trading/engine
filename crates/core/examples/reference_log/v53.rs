//! The session's `V53` part: a member's notice kept across sets, and no office past the first turn's
//! start. A set naming `V51`'s members unchanged is mirrored, and a turn drawn from it a day on
//! applies though the set itself has had less than seven days. An installation then ends the turns;
//! with rotation back on, the next turn applies at its boundary while nothing is sequenced, and the
//! command after it credits the account in office before the turn only up to the boundary.

use engine_core::command::{AdminCommand, Command};
use engine_core::election::RegistryInstallation;
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{DAY_MS, RegistryRotationSet, RegistryTurn, SetMember, TurnReason, boundary};
use engine_core::types::{AccountId, Millis};

use crate::common::{Client, Harness};
use crate::v51::{MEMBER_SEEDS, MEMBERS};

/// The installed operator's key seed and candidate.
const INSTALLED_SEED: u8 = 0x61;
const INSTALLED: AccountId = AccountId([0x60; 20]);

/// `V53` activates with `V51`'s set mirrored. Set 2 names both members unchanged, and a turn drawn
/// from it for the second member applies a day on. An installation ends the turns; rotation turns
/// on again, and the next turn, for the first member, applies a minute after the last command and is
/// taken five minutes later: the minute alone is credited to the account in office before it.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V53 });
    let member = |i: usize| (MEMBERS[i], Client::new(MEMBER_SEEDS[i]).account);
    let members = [70, 30]
        .into_iter()
        .enumerate()
        .map(|(i, weight)| SetMember { candidate: member(i).0, admin_key: member(i).1, weight: TokenUnits(weight) })
        .collect();
    // Mirrored late, already in effect on chain.
    let (effective_at, block, chain_ref) = (h.ts, 52_000_000, "0xset:52:0".to_owned());
    let adoption = RegistryRotationSet { set: 2, epoch: 52, effective_at, quorum: true, members, block, chain_ref };
    let events = h.cmd(Command::AdoptRotationSet(adoption));
    assert!(matches!(events.last(), Some(Event::RotationSetAdopted { set: 2, .. })), "{events:?}");

    let turn = next_turn(h.ts + DAY_MS);
    let (candidate, admin_key) = member(1);
    let drawn = RegistryTurn {
        turn,
        set: 2,
        position: 0,
        reason: TurnReason::Draw,
        candidate,
        admin_key,
        block: 52_000_010,
        chain_ref: "0xturn:52:0".into(),
    };
    let events = h.cmd(Command::AssignTurn(drawn));
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
    h.advance_to(boundary(turn).expect("in range"));
    let release = alloy_primitives::B256::repeat_byte(52);
    let events = h.cmd(Command::TurnTaken { turn, release });
    assert_eq!(events.last(), Some(&Event::TurnTaken { turn, candidate, release }));

    let switch = |enabled: bool, block: u64, chain_ref: &str| Command::SwitchRotation {
        enabled,
        election_cancelled: false,
        block,
        chain_ref: chain_ref.into(),
    };
    h.cmd(switch(false, 52_000_020, "0xrotation:52:0"));
    let installation = RegistryInstallation {
        candidate: INSTALLED,
        admin_key: Client::new(INSTALLED_SEED).account,
        block: 52_000_030,
        chain_ref: "0xinstall:52:0".into(),
    };
    let events = h.cmd(Command::InstallOperator(installation));
    assert!(matches!(events.last(), Some(Event::OperatorInstalled { .. })), "{events:?}");
    assert_eq!(h.engine.daily_turns().in_effect, None, "the installation ended the turns");
    h.cmd(switch(true, 52_000_040, "0xrotation:52:1"));

    let next = next_turn(h.ts + DAY_MS);
    let begins = boundary(next).expect("in range");
    let (candidate, admin_key) = member(0);
    let drawn = RegistryTurn {
        turn: next,
        set: 2,
        position: 0,
        reason: TurnReason::Draw,
        candidate,
        admin_key,
        block: 52_000_050,
        chain_ref: "0xturn:52:1".into(),
    };
    let events = h.cmd(Command::AssignTurn(drawn));
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
    // The last command a minute before the boundary and the member's `TurnTaken` five minutes after it
    // fall in one distribution epoch, so no close runs between them.
    let (last, taken) = (begins - 60_000, begins + 300_000);
    let config = h.engine.staking().expect("the session staked").config;
    assert_eq!(config.epochs_elapsed(last), config.epochs_elapsed(taken), "a close runs between the commands");
    h.advance_to(last);
    h.cmd(Command::Tick);
    let served = |h: &Harness| -> Millis {
        h.engine.staking().and_then(|staking| staking.office.as_ref()).map_or(0, |office| office.served.values().sum())
    };
    let before = served(h);
    h.advance_to(taken);
    let release = alloy_primitives::B256::repeat_byte(53);
    let events = h.cmd(Command::TurnTaken { turn: next, release });
    assert_eq!(events.last(), Some(&Event::TurnTaken { turn: next, candidate, release }));
    assert_eq!(served(h) - before, begins - last, "the account before the turn was credited past its boundary");
}

/// The first turn whose boundary is after `ts`.
fn next_turn(ts: Millis) -> u64 {
    (0..).find(|turn| boundary(*turn).expect("in range") > ts).expect("a later turn")
}
