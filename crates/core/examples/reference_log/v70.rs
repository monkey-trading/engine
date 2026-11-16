//! The session's `V70` part: a proven operator fault. A vote activates `V70` and the node records the
//! fault proof contract; a turn for `V51`'s first member is drawn and taken, its proven fault is
//! mirrored, and the epoch's close cuts the member's part of the operator's share by the haircut, the
//! stakers taking what it withholds; the same fault mirrored again is refused.

use engine_core::access::AccessDeposit;
use engine_core::command::{AdminCommand, Command};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::operator_faults::{FaultProofConfig, Penalty, ProvenFault};
use engine_core::protocol::ProtocolVersion;
use engine_core::turns::{DAY_MS, RegistryTurn, TurnReason, boundary};
use engine_core::types::{AccountId, Side};

use crate::common::close_spread::{attest, evidence, trade};
use crate::common::{Client, ETH, Harness, f};
use crate::v47::LOSER_SEED;
use crate::v51::{MEMBER_SEEDS, MEMBERS};
use crate::v56::pass;

/// The seed of the account key that restocks the house, which the earlier parts paid out.
const PAYER_SEED: u8 = 0x90;
/// Half the share at the fault, back to the whole over ten days.
const PENALTY: Penalty = Penalty { initial_bps: 5_000, decay_ms: 10 * DAY_MS };

/// A vote activates `V70`, and the node records the contract. The first member's turn, drawn from the
/// latest set naming it, is taken at its boundary; a fault proven an hour in restarts its haircut, and,
/// with the house restocked, the epoch's close pays the member less than its time in office earned, the
/// difference to stakers. A mirror under the same reference is refused.
pub(crate) fn session(h: &mut Harness) {
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V70 };
    let events = pass(h, 90, false, Some(activate.clone()));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 90, command: activate })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.protocol_version(), ProtocolVersion::V70);
    h.admin(AdminCommand::SetFaultProof(FaultProofConfig { contract: AccountId([0xfa; 20]), penalty: PENALTY }));

    let (member, admin_key) = (MEMBERS[0], Client::new(MEMBER_SEEDS[0]).account);
    let set = h
        .engine
        .daily_turns()
        .sets
        .iter()
        .rev()
        .find(|(_, set)| set.members.iter().any(|m| m.candidate == member && m.admin_key == admin_key))
        .map(|(id, _)| *id)
        .expect("a mirrored set names the member");
    let turn = (0..).find(|turn| boundary(*turn).expect("in range") > h.ts + DAY_MS).expect("a later turn");
    let drawn = RegistryTurn {
        turn,
        set,
        position: 0,
        reason: TurnReason::Draw,
        candidate: member,
        admin_key,
        block: 70_000_000,
        chain_ref: "0xturn:70:0".into(),
    };
    let events = h.cmd(Command::AssignTurn(drawn));
    assert!(matches!(events.last(), Some(Event::TurnAssigned { .. })), "{events:?}");
    h.advance_to(boundary(turn).expect("in range"));
    let release = alloy_primitives::B256::repeat_byte(69);
    let events = h.cmd(Command::TurnTaken { turn, release });
    assert_eq!(events.last(), Some(&Event::TurnTaken { turn, candidate: member, release }));

    h.advance(60 * 60 * 1000);
    let fault = ProvenFault {
        member,
        turn,
        position: 0,
        signer_epoch: 1,
        seq: h.log.len() as u64 - 2,
        block: 70_000_010,
        chain_ref: "0xfault:70:0".into(),
    };
    let events = h.cmd(Command::ProveFault(fault.clone()));
    assert!(matches!(events.last(), Some(Event::OperatorFaultProven { member: m, .. }) if *m == member), "{events:?}");
    assert_eq!(h.engine.haircut_bps(member, h.ts), PENALTY.initial_bps);

    // The house is restocked, so the close distributes.
    let payer = Client { expires_at: h.ts + 60_000, ..Client::new(PAYER_SEED) };
    attest(h, &payer);
    let house = AccessDeposit {
        account: None,
        payer: Some(payer.account),
        amount: f("2000000"),
        chain_ref: "0xhouse:70".into(),
    };
    let screened = evidence(h, eip712::deposit_screening_hash(&house), None, Some(payer.account));
    let events = h.cmd(Command::AccessDeposit { deposit: house, evidence: screened });
    assert!(events.iter().any(|event| matches!(event, Event::HouseCapitalMoved { .. })), "{events:?}");
    let events = close(h);
    let part = |withheld: bool| {
        events.iter().find_map(|event| match event {
            Event::OperatorShareAllocated { account, amount, .. } if !withheld && *account == member => Some(amount.0),
            Event::OperatorShareWithheld { account, amount, .. } if withheld && *account == member => Some(amount.0),
            _ => None,
        })
    };
    let (kept, withheld) = (part(false).expect("the member keeps a part"), part(true).expect("a part is withheld"));
    assert!(withheld > 0 && kept > withheld, "{events:?}");

    let events = h.cmd(Command::ProveFault(fault));
    assert!(matches!(events.last(), Some(Event::ElectionRejected { .. })), "{events:?}");
}

/// `V47`'s loser loses on a round trip, so the house earns above the floor, then a tick at the end of
/// the open epoch closes it; returns the tick's events.
fn close(h: &mut Harness) -> Vec<Event> {
    let loser = Client { expires_at: h.ts + 60_000, ..Client::new(LOSER_SEED) };
    for (side, price) in [(Side::Buy, "100"), (Side::Sell, "99.7")] {
        h.price(ETH, price);
        let events = trade(h, &loser, ETH, side, "1000", price);
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    }
    let staking = h.engine.staking().expect("the session staked");
    h.advance_to(staking.config.epoch_end(staking.config.epochs_elapsed(h.ts)));
    h.cmd(Command::Tick)
}
