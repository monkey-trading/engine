//! The session's `V72` part: a late promotion of a turn whose draw the engine refused. A vote
//! activates the version. A set naming `V53`'s first member and a new one is mirrored; a turn is drawn
//! from it for a key it does not name, so the engine refuses it. The staking mirror then stalls for ten
//! days, and the next set, adopted on chain after the draw, is mirrored when it resumes, which earlier
//! versions would let forget the first. The schedule's promotion of the turn applies: the promoted
//! member's key signs and its `TurnTaken` is accepted.

use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{DAY_MS, RegistryRotationSet, RegistryTurn, SetMember, TurnReason, boundary};
use engine_core::types::{AccountId, Millis};

use crate::common::{BTC, Client, Harness};
use crate::v51::{MEMBER_SEEDS, MEMBERS};
use crate::v56::pass;

/// The seed of the promoted member's admin key, and its candidate address.
const PROMOTED_SEED: u8 = 0xc1;
const PROMOTED: AccountId = AccountId([0xc2; 20]);

/// Mirrors `set`, adopted at `block` and taking effect at `effective_at`, naming `V53`'s first member and
/// the promoted one.
fn adopt(h: &mut Harness, set: u32, block: u64, effective_at: Millis) {
    let members = [(MEMBERS[0], Client::new(MEMBER_SEEDS[0]).account), (PROMOTED, Client::new(PROMOTED_SEED).account)]
        .into_iter()
        .map(|(candidate, admin_key)| SetMember { candidate, admin_key, weight: TokenUnits(1) })
        .collect();
    let chain_ref = format!("0xset:72:{set}");
    let epoch = 68 + u64::from(set);
    let adoption = RegistryRotationSet { set, epoch, effective_at, quorum: true, members, block, chain_ref };
    let events = h.cmd(Command::AdoptRotationSet(adoption));
    assert!(matches!(events.last(), Some(Event::RotationSetAdopted { .. })), "{events:?}");
}

/// Activates `V72` by vote; then the refused draw and its late promotion.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V72 };
    let events = pass(h, 140, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance_to(h.ts + NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 140, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V72);

    let effective_at = h.ts + 60_000;
    adopt(h, 4, 72_000_004, effective_at);

    // The set's notice runs for seven days before its keys are named long enough.
    let turn = (0..).find(|turn| boundary(*turn).expect("in range") > h.ts + 8 * DAY_MS).expect("a later turn");
    let begins = boundary(turn).expect("in range");
    h.advance_to(begins - DAY_MS);
    let stranger = RegistryTurn {
        turn,
        set: 4,
        position: 0,
        reason: TurnReason::Draw,
        candidate: MEMBERS[0],
        admin_key: AccountId([0x99; 20]),
        block: 72_000_020,
        chain_ref: "0xturn:72:0".into(),
    };
    let events = h.cmd(Command::AssignTurn(stranger.clone()));
    assert!(matches!(events.last(), Some(Event::TurnRefused { .. })), "{events:?}");

    // The turn ends with nothing drawn after it, and the mirror stalls. A set adopted after the draw takes
    // effect seven days on, and its mirror, ten days after the turn began, replaces the one that drew it.
    h.advance_to(begins + 10 * DAY_MS);
    h.cmd(Command::Tick);
    adopt(h, 5, 72_000_025, begins + 7 * DAY_MS);
    assert!(h.engine.daily_turns().sets.contains_key(&4));

    let promotion = RegistryTurn {
        position: 1,
        reason: TurnReason::Promotion,
        candidate: PROMOTED,
        admin_key: Client::new(PROMOTED_SEED).account,
        block: 72_000_030,
        chain_ref: "0xturn:72:1".into(),
        ..stranger
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
    let release = alloy_primitives::B256::repeat_byte(73);
    let events = h.cmd(Command::TurnTaken { turn, release });
    assert_eq!(events.last(), Some(&Event::TurnTaken { turn, candidate: PROMOTED, release }));
}
