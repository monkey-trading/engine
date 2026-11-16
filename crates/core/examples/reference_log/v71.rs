//! The session's `V71` part: a promotion finalised after its turn's nominal end. A vote activates the
//! version. A set naming `V53`'s two members and a third is mirrored; a turn drawn from it for the
//! first member is taken, and with no later turn drawn the schedule finalises a promotion of that turn
//! only ten minutes after its end. The promotion applies when it is mirrored: the promoted member's key
//! signs, the first member's is refused, and the promoted member's `TurnTaken` is accepted.

use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{DAY_MS, RegistryRotationSet, RegistryTurn, SetMember, TurnReason, boundary};
use engine_core::types::AccountId;

use crate::common::{BTC, Client, Harness};
use crate::v51::{MEMBER_SEEDS, MEMBERS};
use crate::v56::pass;

/// The seed of the promoted member's admin key, and its candidate address.
const PROMOTED_SEED: u8 = 0xb1;
const PROMOTED: AccountId = AccountId([0xb2; 20]);

/// Activates `V71` by vote; then the late promotion described above.
pub(crate) fn session(h: &mut Harness) {
    let activation = AdminCommand::ActivateProtocol { version: ProtocolVersion::V71 };
    let events = pass(h, 130, false, Some(activation.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance_to(h.ts + NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 130, command: activation })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V71);

    let (block, chain_ref) = (71_000_000, "0xrotation:71:0".to_owned());
    let events = h.cmd(Command::SwitchRotation { enabled: true, election_cancelled: false, block, chain_ref });
    assert!(matches!(events.last(), Some(Event::RotationSwitched { .. })), "{events:?}");
    let members = [
        (MEMBERS[0], Client::new(MEMBER_SEEDS[0]).account),
        (MEMBERS[1], Client::new(MEMBER_SEEDS[1]).account),
        (PROMOTED, Client::new(PROMOTED_SEED).account),
    ]
    .into_iter()
    .map(|(candidate, admin_key)| SetMember { candidate, admin_key, weight: TokenUnits(1) })
    .collect();
    let (block, chain_ref, effective_at) = (71_000_010, "0xset:71:0".to_owned(), h.ts + 60_000);
    let adoption = RegistryRotationSet { set: 3, epoch: 71, effective_at, quorum: true, members, block, chain_ref };
    let events = h.cmd(Command::AdoptRotationSet(adoption));
    assert!(matches!(events.last(), Some(Event::RotationSetAdopted { set: 3, .. })), "{events:?}");

    // The set's notice runs for seven days before the promoted key is named by it long enough.
    let turn = (0..).find(|turn| boundary(*turn).expect("in range") > h.ts + 8 * DAY_MS).expect("a later turn");
    let begins = boundary(turn).expect("in range");
    h.advance_to(begins - DAY_MS);
    let drawn = RegistryTurn {
        turn,
        set: 3,
        position: 0,
        reason: TurnReason::Draw,
        candidate: MEMBERS[0],
        admin_key: Client::new(MEMBER_SEEDS[0]).account,
        block: 71_000_020,
        chain_ref: "0xturn:71:0".into(),
    };
    let events = h.cmd(Command::AssignTurn(drawn.clone()));
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
    h.advance_to(begins + 60_000);
    let release = alloy_primitives::B256::repeat_byte(71);
    let events = h.cmd(Command::TurnTaken { turn, release });
    assert_eq!(events.last(), Some(&Event::TurnTaken { turn, candidate: MEMBERS[0], release }));

    // No later turn is drawn, and the promotion is finalised ten minutes after the turn's end.
    h.advance_to(begins + DAY_MS + 600_000);
    h.cmd(Command::Tick);
    let promotion = RegistryTurn {
        position: 1,
        reason: TurnReason::Promotion,
        candidate: PROMOTED,
        admin_key: Client::new(PROMOTED_SEED).account,
        block: 71_000_030,
        chain_ref: "0xturn:71:1".into(),
        ..drawn
    };
    let mirrored = h.ts;
    let events = h.cmd(Command::AssignTurn(promotion));
    assert!(
        matches!(events.last(), Some(Event::TurnAssigned { applies_at, position: 1, .. }) if *applies_at == mirrored),
        "{events:?}"
    );
    let reduce = AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: false };
    let refused = h.operate_as(MEMBER_SEEDS[0], reduce.clone());
    assert!(matches!(refused.last(), Some(Event::AdminRejected { .. })), "{refused:?}");
    let applied = h.operate_as(PROMOTED_SEED, reduce);
    assert_eq!(applied.last(), Some(&Event::AdminApplied), "{applied:?}");
    let release = alloy_primitives::B256::repeat_byte(72);
    let events = h.cmd(Command::TurnTaken { turn, release });
    assert_eq!(events.last(), Some(&Event::TurnTaken { turn, candidate: PROMOTED, release }));
}
