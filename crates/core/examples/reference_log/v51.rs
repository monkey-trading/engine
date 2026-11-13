//! The session's `V51` part: daily operator turns. A rotation set is mirrored, and past its seven
//! days' notice a drawn turn's key signs from its boundary while the key before it signs nothing; the
//! member takes its turn and earns the operator's share from then; a turn naming a key the set does
//! not name is credited and refused.

use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{DAY_MS, RegistryRotationSet, RegistryTurn, SetMember, TurnReason, boundary};
use engine_core::types::AccountId;

use crate::common::{BTC, Client, Harness};
use crate::v47::close;
use crate::v50::ELECTED_SEED;

/// The seeds of the two members' admin keys, and their candidate addresses.
pub(crate) const MEMBER_SEEDS: [u8; 2] = [0x5c, 0x5d];
const MEMBERS: [AccountId; 2] = [AccountId([0x5e; 20]), AccountId([0x5f; 20])];

/// `V51` activates with `V50`'s elected operator in effect and rotation on. A set of two members
/// is mirrored; eight days on, the drawn member's key signs from its turn's boundary and the elected
/// key is refused; a turn naming a key the set does not is refused; the member's `TurnTaken` starts
/// its time in office, which the epoch's close pays.
pub(crate) fn session(h: &mut Harness) {
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V51 });
    let members = [70, 30]
        .into_iter()
        .enumerate()
        .map(|(i, weight)| SetMember {
            candidate: MEMBERS[i],
            admin_key: Client::new(MEMBER_SEEDS[i]).account,
            weight: TokenUnits(weight),
        })
        .collect();
    let effective_at = h.ts + NOTICE_MS + 60 * 60 * 1000;
    let (block, chain_ref) = (51_000_000, "0xset:51:0".to_owned());
    let adoption = RegistryRotationSet { set: 1, epoch: 51, effective_at, quorum: true, members, block, chain_ref };
    let events = h.cmd(Command::AdoptRotationSet(adoption));
    assert!(matches!(events.last(), Some(Event::RotationSetAdopted { set: 1, .. })), "{events:?}");

    let turn = (0..).find(|turn| boundary(*turn).expect("in range") > h.ts + 8 * DAY_MS).expect("a later turn");
    let begins = boundary(turn).expect("in range");
    h.advance_to(begins - DAY_MS);
    let drawn = RegistryTurn {
        turn,
        set: 1,
        position: 0,
        reason: TurnReason::Draw,
        candidate: MEMBERS[0],
        admin_key: Client::new(MEMBER_SEEDS[0]).account,
        block: 51_000_010,
        chain_ref: "0xturn:51:0".into(),
    };
    let events = h.cmd(Command::AssignTurn(drawn.clone()));
    assert!(
        matches!(events.last(), Some(Event::TurnAssigned { applies_at, .. }) if *applies_at == begins),
        "{events:?}"
    );
    let stranger = RegistryTurn {
        turn: turn + 1,
        candidate: MEMBERS[1],
        block: 51_000_011,
        chain_ref: "0xturn:51:1".into(),
        ..drawn
    };
    let events = h.cmd(Command::AssignTurn(stranger));
    assert!(matches!(events.last(), Some(Event::TurnRefused { .. })), "{events:?}");

    h.advance_to(begins);
    let reduce = |reduce_only| AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only };
    let refused = h.operate_as(ELECTED_SEED, reduce(true));
    assert!(matches!(refused.last(), Some(Event::AdminRejected { .. })), "{refused:?}");
    let applied = h.operate_as(MEMBER_SEEDS[0], reduce(true));
    assert_eq!(applied.last(), Some(&Event::AdminApplied));
    h.operate_as(MEMBER_SEEDS[0], reduce(false));
    let release = alloy_primitives::B256::repeat_byte(51);
    let events = h.cmd(Command::TurnTaken { turn, release });
    assert_eq!(events.last(), Some(&Event::TurnTaken { turn, candidate: MEMBERS[0], release }));
    let (shares, excess, allocated) = close(h);
    assert!(shares.iter().any(|share| share.0 == MEMBERS[0]), "{shares:?}");
    assert!(allocated.0 <= excess.0, "{allocated:?} {excess:?}");
}
