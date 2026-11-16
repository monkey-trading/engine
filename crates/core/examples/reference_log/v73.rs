//! The session's `V73` part: sets forgotten by the block time the staking mirror has completed. A vote
//! activates the version. Two sets naming `V53`'s first member and a new one are mirrored, the first in
//! effect at a turn's draw deadline and the second from six hours after it. The mirror completes a scan
//! at the deadline and stalls; two and a half days on it mirrors, in block order, a set adopted after
//! the deadline, which earlier versions would let forget the first, then the turn's draw, refused as
//! ended, and the schedule's promotion of the turn, which applies: the promoted member's key signs and
//! its `TurnTaken` is accepted. Once the mirror has caught up, the next adoption forgets the sets no
//! turn can draw from.

use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{DAY_MS, DRAW_LEAD_MS, RegistryRotationSet, RegistryTurn, SetMember, TurnReason, boundary};
use engine_core::types::{AccountId, Millis};

use crate::common::{BTC, Client, Harness};
use crate::v51::{MEMBER_SEEDS, MEMBERS};
use crate::v56::pass;

/// The seed of the promoted member's admin key, and its candidate address.
const PROMOTED_SEED: u8 = 0xd1;
const PROMOTED: AccountId = AccountId([0xd2; 20]);
const HOUR_MS: Millis = 60 * 60 * 1000;

/// Mirrors `set`, adopted at `block` and taking effect at `effective_at`, naming `V53`'s first member and
/// the promoted one.
fn adopt(h: &mut Harness, set: u32, block: u64, effective_at: Millis) {
    let members = [(MEMBERS[0], Client::new(MEMBER_SEEDS[0]).account), (PROMOTED, Client::new(PROMOTED_SEED).account)]
        .into_iter()
        .map(|(candidate, admin_key)| SetMember { candidate, admin_key, weight: TokenUnits(1) })
        .collect();
    let chain_ref = format!("0xset:73:{set}");
    let epoch = 70 + u64::from(set);
    let adoption = RegistryRotationSet { set, epoch, effective_at, quorum: true, members, block, chain_ref };
    let events = h.cmd(Command::AdoptRotationSet(adoption));
    assert!(matches!(events.last(), Some(Event::RotationSetAdopted { .. })), "{events:?}");
}

/// Completes a staking scan at `block`, stamped `block_time`.
fn frontier(h: &mut Harness, block: u64, block_time: Millis) {
    let block_hash = alloy_primitives::B256::repeat_byte(73);
    let events = h.cmd(Command::StakingProgress { block, block_hash, block_time: Some(block_time), named_turn: None });
    assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
}

/// Activates `V73` by vote; then the stalled mirror, the late draw and its promotion, and the catch-up.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V73 };
    let events = pass(h, 150, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance_to(h.ts + NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 150, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V73);

    // The sets' notice runs for seven days before the promoted key is named long enough.
    let turn = (0..).find(|turn| boundary(*turn).expect("in range") > h.ts + 8 * DAY_MS).expect("a later turn");
    let deadline = boundary(turn).expect("in range") - DRAW_LEAD_MS;
    adopt(h, 6, 73_000_006, deadline - HOUR_MS);
    adopt(h, 7, 73_000_007, deadline + 6 * HOUR_MS);

    // The mirror completes a scan at the deadline, then stalls while the schedule adopts a set and draws
    // the turn, and resumes two and a half days on, after the turn has ended.
    h.advance_to(deadline);
    frontier(h, 73_000_010, deadline);
    h.advance_to(deadline + 5 * DAY_MS / 2);
    adopt(h, 8, 73_000_015, deadline + HOUR_MS / 2 + NOTICE_MS);
    assert!(h.engine.daily_turns().sets.contains_key(&6));
    let drawn = RegistryTurn {
        turn,
        set: 6,
        position: 0,
        reason: TurnReason::Draw,
        candidate: MEMBERS[0],
        admin_key: Client::new(MEMBER_SEEDS[0]).account,
        block: 73_000_020,
        chain_ref: "0xturn:73:0".into(),
    };
    let events = h.cmd(Command::AssignTurn(drawn.clone()));
    assert!(matches!(events.last(), Some(Event::TurnRefused { .. })), "{events:?}");

    let promotion = RegistryTurn {
        position: 1,
        reason: TurnReason::Promotion,
        candidate: PROMOTED,
        admin_key: Client::new(PROMOTED_SEED).account,
        block: 73_000_030,
        chain_ref: "0xturn:73:1".into(),
        ..drawn
    };
    let mirrored = h.ts;
    let events = h.cmd(Command::AssignTurn(promotion));
    assert!(
        matches!(events.last(), Some(Event::TurnAssigned { applies_at, position: 1, .. }) if *applies_at == mirrored),
        "{events:?}"
    );
    let reduce = AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: false };
    let applied = h.operate_as(PROMOTED_SEED, reduce);
    assert_eq!(applied.last(), Some(&Event::AdminApplied), "{applied:?}");
    let release = alloy_primitives::B256::repeat_byte(74);
    let events = h.cmd(Command::TurnTaken { turn, release });
    assert_eq!(events.last(), Some(&Event::TurnTaken { turn, candidate: PROMOTED, release }));

    // The mirror catches up, and the next adoption forgets every set before the one in effect.
    let caught_up = h.ts;
    frontier(h, 73_000_040, caught_up);
    adopt(h, 9, 73_000_045, caught_up + NOTICE_MS);
    let kept: Vec<u32> = h.engine.daily_turns().sets.keys().copied().collect();
    assert_eq!(kept, [6, 7, 8, 9]);
}
