//! What a carried genesis refuses: a part that does not prove against the source root, comes out of order or
//! after another command, carries into a log that has run more than genesis, or leaves a value out; and a source
//! whose carry would lose state. A refused part changes nothing.

use alloy_primitives::B256;
use engine_core::Fixed;
use engine_core::access::AccessPolicy;
use engine_core::awards::{AwardApproval, Severity};
use engine_core::carry::{CarriedBalance, CarryPart, CarrySource};
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::event::Event;
use engine_core::governance::{PassedProposal, command_hash};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::common::close_spread::source_key;
use crate::common::{HOUSE_CAPITAL, Harness};
use crate::fixture::{
    DAY_MS, PERIOD_BUDGET, RESEARCHER, TRADER_SEEDS, carry, destination, genesis, lose, part, refusal, source, stake,
    staking_config,
};

/// A change to the source's parts, which a carry must refuse with a reason containing its text.
type Tamper = (&'static str, fn(&mut Vec<CarryPart>), &'static str);
/// A change to the source's state, which leaves it a source that builds no parts, with a reason containing its text.
type Lossy = (&'static str, fn(&mut Harness), &'static str);

/// The index of the first part of `kind`.
fn first(parts: &[CarryPart], kind: fn(&CarryPart) -> bool) -> usize {
    parts.iter().position(kind).expect("the source carries a part of that kind")
}

fn is_entitlements(part: &CarryPart) -> bool {
    matches!(part, CarryPart::Entitlements { .. })
}

fn is_stakes(part: &CarryPart) -> bool {
    matches!(part, CarryPart::Stakes { .. })
}

fn is_awards(part: &CarryPart) -> bool {
    matches!(part, CarryPart::Awards { .. })
}

/// The entries of the first entitlements part.
fn entitlements(parts: &mut [CarryPart]) -> &mut Vec<CarriedBalance> {
    let at = first(parts, is_entitlements);
    let CarryPart::Entitlements { entries } = &mut parts[at] else { unreachable!("found above") };
    entries
}

/// The entries of the first stakes part.
fn stakes(parts: &mut [CarryPart]) -> &mut Vec<CarriedBalance> {
    let at = first(parts, is_stakes);
    let CarryPart::Stakes { entries } = &mut parts[at] else { unreachable!("found above") };
    entries
}

/// The begin part's fields.
fn begin(parts: &mut [CarryPart]) -> (&mut CarrySource, &mut String) {
    let CarryPart::Begin { source, globals, .. } = &mut parts[0] else { unreachable!("Begin comes first") };
    (source, globals)
}

const TAMPERS: [Tamper; 17] = [
    ("altered remaining state", |parts| begin(parts).1.push(' '), "does not prove against its root"),
    ("another root", |parts| begin(parts).0.root = B256::repeat_byte(1), "does not prove against its root"),
    ("another vault", |parts| begin(parts).0.vault = AccountId([0x0b; 20]), "does not record client auth"),
    ("another chain", |parts| begin(parts).0.chain_id += 1, "does not record client auth"),
    ("another seq", |parts| begin(parts).0.seq -= 1, "root is at seq"),
    ("a raised entitlement", |parts| entitlements(parts)[0].amount.0 += 1, "does not hold"),
    ("entitlements out of order", |parts| entitlements(parts).reverse(), "strictly increasing"),
    (
        "an entitlement twice",
        |parts| {
            let twice = entitlements(parts)[0].clone();
            entitlements(parts).insert(0, twice);
        },
        "strictly increasing",
    ),
    ("an entitlement left out", |parts| drop(entitlements(parts).remove(1)), "entitlements sum to"),
    ("a stake left out", |parts| drop(stakes(parts).remove(0)), "stakes sum to"),
    ("a zero stake", |parts| stakes(parts)[0].amount = TokenUnits(0), "positive"),
    (
        "stakes before entitlements",
        |parts| {
            let (entitled, staked) = (first(parts, is_entitlements), first(parts, is_stakes));
            parts.swap(entitled, staked);
        },
        "comes after",
    ),
    (
        "an empty part",
        |parts| {
            let at = first(parts, is_stakes);
            parts.insert(at, CarryPart::Stakes { entries: vec![] });
        },
        "1 to",
    ),
    (
        "an award under another id",
        |parts| {
            let at = first(parts, is_awards);
            let CarryPart::Awards { awards, .. } = &mut parts[at] else { unreachable!("found above") };
            awards[0].id = 1;
        },
        "in order",
    ),
    (
        "an altered award",
        |parts| {
            let at = first(parts, is_awards);
            let CarryPart::Awards { awards, .. } = &mut parts[at] else { unreachable!("found above") };
            awards[0].award.paid.0 += 1;
        },
        "does not hold the carried award",
    ),
    (
        "the awards' close left out",
        |parts| {
            let at = first(parts, is_awards);
            let CarryPart::Awards { next, .. } = &mut parts[at] else { unreachable!("found above") };
            *next = None;
        },
        "end with the proof",
    ),
    (
        "an absence proven at another id",
        |parts| {
            let at = first(parts, is_awards);
            let CarryPart::Awards { awards, .. } = &mut parts[at] else { unreachable!("found above") };
            awards.clear();
        },
        "holds no award at id 0",
    ),
];

#[test]
fn every_tampered_part_is_refused_and_changes_nothing() {
    let alpha = source();
    let (parts, _) = alpha.engine.carry_parts(ProtocolVersion::V90).expect("the source carries");
    for (what, tamper, reason) in TAMPERS {
        let mut tampered = parts.clone();
        tamper(&mut tampered);
        let mut h = destination(&alpha);
        let mut refused = None;
        for carried in tampered {
            let before = h.engine.carry_progress().cloned();
            let entitled = h.engine.entitlements().clone();
            let events = part(&mut h, carried);
            if events.last() != Some(&Event::AdminApplied) {
                assert_eq!(h.engine.carry_progress(), before.as_ref(), "{what}: a refused part changed the carry");
                assert_eq!(h.engine.entitlements(), &entitled, "{what}: a refused part changed an entitlement");
                refused = Some(refusal(&events).to_owned());
                break;
            }
        }
        let refused = refused.unwrap_or_else(|| panic!("{what}: every part applied"));
        assert!(refused.contains(reason), "{what}: {refused}");
        assert!(h.engine.carried_from().is_none(), "{what}");
    }
}

#[test]
fn a_part_after_another_command_or_a_second_begin_is_refused() {
    let alpha = source();
    let (parts, _) = alpha.engine.carry_parts(ProtocolVersion::V90).expect("the source carries");
    let mut h = destination(&alpha);
    assert_eq!(part(&mut h, parts[0].clone()).last(), Some(&Event::AdminApplied));
    assert!(refusal(&part(&mut h, parts[0].clone())).contains("already carried state"));
    h.cmd(Command::Tick);
    assert!(refusal(&part(&mut h, parts[1].clone())).contains("consecutive commands"));

    // Once carried, a log carries nothing again.
    let mut h = destination(&alpha);
    carry(&mut h, &parts);
    assert!(refusal(&part(&mut h, parts[0].clone())).contains("already carried state"));
    assert!(refusal(&part(&mut h, CarryPart::End)).contains("no carried genesis has begun"));
}

#[test]
fn a_carry_into_a_log_past_its_genesis_or_before_v90_is_refused() {
    let alpha = source();
    let (parts, _) = alpha.engine.carry_parts(ProtocolVersion::V90).expect("the source carries");
    let begin = parts[0].clone();

    let mut unstaked = genesis(ProtocolVersion::V90);
    assert!(refusal(&part(&mut unstaked, begin.clone())).contains("staking initialised under V90"));

    let mut credited = destination(&alpha);
    credited.cmd(Command::HouseCapital { amount: Fixed::from_int(HOUSE_CAPITAL), chain_ref: "genesis".into() });
    assert!(refusal(&part(&mut credited, begin.clone())).contains("before anything is credited"));

    let mut mirrored = destination(&alpha);
    stake(&mut mirrored, AccountId([0xa9; 20]), 1, 1);
    assert!(refusal(&part(&mut mirrored, begin.clone())).contains("with nothing mirrored"));

    let mut earlier = genesis(ProtocolVersion::V89);
    earlier.advance_to(alpha.ts);
    earlier.admin(AdminCommand::InitStaking(staking_config(earlier.ts)));
    assert!(refusal(&part(&mut earlier, begin.clone())).contains("needs protocol V90"));

    let mut signed = destination(&alpha);
    let events = signed.operate(AdminCommand::CarryState(begin));
    assert!(refusal(&events).contains("the operator may not send this command"), "{events:?}");
}

/// The source's state, at the root of its last command, after `change`.
fn source_after(change: fn(&mut Harness)) -> Result<(), String> {
    let mut h = source();
    change(&mut h);
    h.engine.state_root();
    h.engine.carry_parts(ProtocolVersion::V90).map(|_| ())
}

/// Mirrors the registry turning rotation on or off in `block`, and the mirror's frontier completing it.
fn switch(h: &mut Harness, enabled: bool, block: u64) {
    let chain_ref = format!("0xrotation:{enabled}:{block}");
    h.cmd(Command::SwitchRotation { enabled, election_cancelled: false, block, chain_ref });
    let block_hash = B256::repeat_byte(block as u8);
    h.cmd(Command::StakingProgress { block, block_hash, block_time: None, named_turn: None });
}

#[test]
fn a_source_whose_carry_would_lose_state_builds_no_parts() {
    let refused: [Lossy; 4] = [
        ("rotation on", |h| switch(h, true, 9), "rotation is on"),
        ("results in the open period", |h| lose(h, TRADER_SEEDS[0]), "results in the open emission period"),
        (
            "a stake past the completed frontier",
            |h| drop(stake(h, AccountId([0xa9; 20]), 1, 99)),
            "has not completed a scan",
        ),
        (
            "a vote's award waiting",
            |h| {
                switch(h, true, 9);
                let award = AdminCommand::ApproveAward(AwardApproval {
                    recipient: RESEARCHER,
                    severity: Severity::Low,
                    advisory: "GHSA-2".into(),
                    amount: Severity::Low.amount(),
                });
                let (command_hash, command) = (command_hash(&award), Some(award));
                let passed = PassedProposal {
                    proposal: 2,
                    fast: false,
                    command_hash,
                    command,
                    block: 9,
                    chain_ref: "0xvote:9".into(),
                };
                let events = h.cmd(Command::PassProposal(Box::new(passed)));
                assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
                switch(h, false, 10);
            },
            "award proposal 2 waits to apply",
        ),
    ];
    for (what, change, reason) in refused {
        let error = source_after(change).expect_err(what);
        assert!(error.contains(reason), "{what}: {error}");
    }
}

#[test]
fn a_source_before_v90_or_without_signed_requests_builds_no_parts() {
    let mut earlier = genesis(ProtocolVersion::V89);
    earlier.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: DAY_MS }));
    earlier.cmd(Command::HouseCapital { amount: Fixed::from_int(HOUSE_CAPITAL), chain_ref: "genesis".into() });
    let schedule = EmissionSchedule { start_ms: earlier.ts, period_budget: TokenUnits(PERIOD_BUDGET) };
    earlier.operator_admin(AdminCommand::InitEmissions(schedule));
    earlier.operator_admin(AdminCommand::InitStaking(staking_config(earlier.ts)));
    let progress =
        Command::StakingProgress { block: 1, block_hash: B256::repeat_byte(1), block_time: None, named_turn: None };
    earlier.cmd(progress);
    earlier.engine.state_root();
    let error = earlier.engine.carry_parts(ProtocolVersion::V90).expect_err("a V89 source");
    assert!(error.contains("before V90"), "{error}");

    let mut unsigned = Harness::empty();
    unsigned.admin(AdminCommand::SetClientAuth(engine_core::auth::ClientAuth::Unsigned));
    unsigned.admin(AdminCommand::SetOperator(crate::common::operator()));
    unsigned.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V90 });
    unsigned.engine.state_root();
    let error = unsigned.engine.carry_parts(ProtocolVersion::V90).expect_err("an unsigned source");
    assert!(error.contains("no EIP-712 client auth"), "{error}");
}
