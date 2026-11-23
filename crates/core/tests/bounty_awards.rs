//! Bug bounty awards from `V89`, through `Engine::apply`: an award approved only by a passed normal vote
//! seven days after it is logged, at the scale's amount for its severity; refused for the operator's and
//! the node's own commands in every state and on the fast path; and paid by each epoch close that
//! distributes, oldest first, up to the cap, before the operator's share and the stakers, into the
//! recipient's cumulative distribution leaf (`bounty_awards/payment.rs`).

mod common;

#[path = "bounty_awards/payment.rs"]
mod payment;

use common::close_spread::{attest, ceilings, fund, install_expect, source_key};
use common::{Client, HOUSE_CAPITAL, Harness};
use engine_core::Fixed;
use engine_core::access::AccessPolicy;
use engine_core::awards::{AwardApproval, AwardCap, LAUNCH_CAP, Severity};
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::governance::{GovernanceConfig, PassedProposal, VotedChange, command_hash};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::UsdcUnits;
use engine_core::turns::{RegistryRotationSet, SetMember};
use engine_core::types::{AccountId, Millis};

const DAY_MS: Millis = 24 * 60 * 60 * 1000;
/// The vote contract the fixture records.
const VOTE: AccountId = AccountId([0x70; 20]);
/// The researcher the awards are credited to.
const RESEARCHER: AccountId = AccountId([0x7e; 20]);
/// Another researcher.
const SECOND: AccountId = AccountId([0x7f; 20]);
/// The stakers.
const STAKER_A: AccountId = AccountId([0xa1; 20]);
const STAKER_B: AccountId = AccountId([0xa2; 20]);
/// One USDC in USDC base units.
const USDC: u128 = 1_000_000;
/// The seed of the trader whose losses the house earns from.
const TRADER_SEED: u8 = 0x89;

/// Builds a harness in one of the states a test walks.
type Build = fn() -> Harness;

/// Staking in one-day epochs with the house `excess` USD above its floor, then `version`, the recorded vote
/// contract, a one-member rotation set in effect and rotation on, so votes govern, and two stakers at 3:1.
fn governed(version: ProtocolVersion, excess: i64) -> Harness {
    let mut h = voting(version, excess);
    let member =
        SetMember { candidate: AccountId([0xc1; 20]), admin_key: Client::operator().account, weight: TokenUnits(1) };
    let (block, chain_ref) = (h.log.len() as u64, "0xset:1".to_owned());
    let adoption = RegistryRotationSet {
        set: 1,
        epoch: 1,
        effective_at: h.ts,
        quorum: true,
        members: vec![member],
        block,
        chain_ref,
    };
    let events = h.cmd(Command::AdoptRotationSet(adoption));
    assert!(events.iter().any(|e| matches!(e, Event::RotationSetAdopted { .. })), "{events:?}");
    switch_rotation(&mut h, true);
    h
}

/// `governed` without the rotation set or rotation: the operator keeps its bounded commands. A screened
/// trader holds 10,000,000 USD, whose losses the house earns from.
fn voting(version: ProtocolVersion, excess: i64) -> Harness {
    let floor = Fixed::from_int(HOUSE_CAPITAL - excess);
    let mut h = Harness::staking(floor, DAY_MS, 3 * DAY_MS);
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 1_000 }));
    h.admin(AdminCommand::ActivateProtocol { version });
    let trader = Client::new(TRADER_SEED);
    attest(&mut h, &trader);
    fund(&mut h, &trader, "10000000");
    h.admin(AdminCommand::SetGovernance(GovernanceConfig { vote: VOTE, restriction_ttl_ms: DAY_MS }));
    for (account, amount) in [(STAKER_A, 3), (STAKER_B, 1)] {
        let chain_ref = format!("0xstake:{}", h.log.len());
        let block = h.log.len() as u64;
        let events = h.cmd(Command::Stake { account, amount: TokenUnits(amount), block, chain_ref });
        assert!(matches!(events[..], [Event::Staked { .. }]), "{events:?}");
    }
    h
}

fn switch_rotation(h: &mut Harness, enabled: bool) {
    let (block, chain_ref) = (h.log.len() as u64, format!("0xrotation:{}", h.log.len()));
    let events = h.cmd(Command::SwitchRotation { enabled, election_cancelled: false, block, chain_ref });
    assert!(events.iter().any(|e| matches!(e, Event::RotationSwitched { .. })), "{events:?}");
}

/// An award to `recipient` at the scale's amount for `severity`.
fn award(recipient: AccountId, severity: Severity, advisory: &str) -> AdminCommand {
    AdminCommand::ApproveAward(AwardApproval {
        recipient,
        severity,
        advisory: advisory.into(),
        amount: severity.amount(),
    })
}

/// Mirrors proposal `proposal` passing on `fast` with `command`.
fn pass(h: &mut Harness, proposal: u64, fast: bool, command: &AdminCommand) -> Vec<Event> {
    let command_hash = command_hash(command);
    let (block, chain_ref) = (h.log.len() as u64, format!("0xvote:{proposal}:{}", h.log.len()));
    let passed = PassedProposal { proposal, fast, command_hash, command: Some(command.clone()), block, chain_ref };
    h.cmd(Command::PassProposal(Box::new(passed)))
}

/// Sequences the node's `ApplyProposal` for `proposal` with `command`.
fn apply(h: &mut Harness, proposal: u64, command: &AdminCommand) -> Vec<Event> {
    h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal, command: command.clone() })))
}

/// Passes `command` as proposal `proposal`, waits out its notice and applies it, which must apply.
fn vote_in(h: &mut Harness, proposal: u64, command: &AdminCommand) -> Vec<Event> {
    let events = pass(h, proposal, false, command);
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = apply(h, proposal, command);
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
    events
}

fn refused(events: &[Event], reason: &str) {
    let found = matches!(events.last(), Some(Event::ProposalRefused { reason: r, .. }) if r.contains(reason));
    assert!(found, "{events:?}");
}

fn rejected(events: &[Event], reason: &str) {
    assert!(matches!(events.last(), Some(Event::AdminRejected { reason: r }) if r.contains(reason)), "{events:?}");
}

#[test]
fn an_award_applies_seven_days_after_its_vote_is_logged_and_queues_at_the_back() {
    let mut h = governed(ProtocolVersion::V89, 0);
    let high = award(RESEARCHER, Severity::High, "GHSA-4v2c-9xq8-jm3w");
    let logged = h.ts;
    let events = pass(&mut h, 11, false, &high);
    assert_eq!(events.last(), Some(&Event::ProposalScheduled { proposal: 11, applies_at: logged + NOTICE_MS }));
    assert!(h.engine.awards().is_empty(), "nothing is queued before the notice runs");

    h.advance(NOTICE_MS - 1);
    rejected(&apply(&mut h, 11, &high), "applies from");
    assert!(h.engine.awards().is_empty());

    h.advance(1);
    let events = apply(&mut h, 11, &high);
    let approved = engine_core::awards::AwardApproved {
        award: 0,
        proposal: 11,
        account: RESEARCHER,
        severity: Severity::High,
        advisory: "GHSA-4v2c-9xq8-jm3w".into(),
        amount: UsdcUnits(50_000 * USDC),
    };
    assert_eq!(events, [Event::AwardApproved(approved), Event::AdminApplied]);
    let queued = &h.engine.awards()[&0];
    assert_eq!((queued.proposal, queued.paid, queued.approved_at), (11, UsdcUnits(0), logged + NOTICE_MS));

    let low = award(SECOND, Severity::Low, "GHSA-2");
    vote_in(&mut h, 12, &low);
    let ids: Vec<(u64, u64)> = h.engine.awards().iter().map(|(id, award)| (*id, award.proposal)).collect();
    assert_eq!(ids, [(0, 11), (1, 12)], "awards queue in the order they apply");
}

#[test]
fn an_award_for_another_amount_than_its_severitys_is_refused_when_mirrored() {
    let mut h = governed(ProtocolVersion::V89, 0);
    let wrong = [
        (Severity::High, Severity::Critical.amount()),
        (Severity::Critical, Severity::High.amount()),
        (Severity::Medium, UsdcUnits(10_000 * USDC + 1)),
        (Severity::Low, UsdcUnits(999 * USDC)),
        (Severity::Low, UsdcUnits(0)),
    ];
    for (proposal, (severity, amount)) in (20..).zip(wrong) {
        let command = AdminCommand::ApproveAward(AwardApproval {
            recipient: RESEARCHER,
            severity,
            advisory: "GHSA-1".into(),
            amount,
        });
        refused(&pass(&mut h, proposal, false, &command), &format!("{severity:?} award is {}", severity.amount()));
    }
    assert!(h.engine.governance().pending.is_empty() && h.engine.awards().is_empty());
}

#[test]
fn the_operator_and_the_node_never_approve_an_award_or_move_the_cap_in_any_state() {
    let commands = [
        award(Client::operator().account, Severity::Critical, "GHSA-1"),
        AdminCommand::SetAwardCap(AwardCap { excess_bps: 2_000, max: UsdcUnits(250_000 * USDC) }),
    ];
    let states: [(&str, Build); 4] = [
        ("before V89", || voting(ProtocolVersion::V88, 0)),
        ("V89 with rotation off", || voting(ProtocolVersion::V89, 0)),
        ("V89 with rotation on and no set", || {
            let mut h = voting(ProtocolVersion::V89, 0);
            switch_rotation(&mut h, true);
            h
        }),
        ("V89 while votes govern", || governed(ProtocolVersion::V89, 0)),
    ];
    for (state, build) in states {
        for command in &commands {
            let mut h = build();
            let nonce = h.engine.next_request_nonce(Client::operator().account);
            let signed = h.operate(command.clone());
            assert!(matches!(signed.last(), Some(Event::AdminRejected { .. })), "{state}: {signed:?}");
            assert_eq!(
                h.engine.next_request_nonce(Client::operator().account),
                nonce + 1,
                "{state}: the nonce is spent"
            );
            let unsigned = h.cmd(Command::Admin(command.clone()));
            assert!(matches!(unsigned.last(), Some(Event::AdminRejected { .. })), "{state}: {unsigned:?}");
            assert!(h.engine.awards().is_empty(), "{state}");
            assert_eq!(h.engine.award_cap(), LAUNCH_CAP, "{state}");
        }
    }
}

#[test]
fn the_fast_path_refuses_an_award_and_the_cap() {
    let mut h = governed(ProtocolVersion::V89, 0);
    refused(&pass(&mut h, 30, true, &award(RESEARCHER, Severity::High, "GHSA-1")), "the fast path takes only");
    let cap = AdminCommand::SetAwardCap(AwardCap { excess_bps: 1_000, max: UsdcUnits(100_000 * USDC) });
    refused(&pass(&mut h, 31, true, &cap), "the fast path takes only");
    assert!(h.engine.awards().is_empty() && h.engine.governance().restrictions.is_empty());
    assert_eq!(h.engine.award_cap(), LAUNCH_CAP);
}

#[test]
fn before_v89_no_vote_carries_an_award_or_the_cap() {
    let mut h = governed(ProtocolVersion::V88, 0);
    refused(&pass(&mut h, 40, false, &award(RESEARCHER, Severity::Low, "GHSA-1")), "a normal vote changes only");
    let cap = AdminCommand::SetAwardCap(AwardCap { excess_bps: 1_000, max: UsdcUnits(100_000 * USDC) });
    refused(&pass(&mut h, 41, false, &cap), "a normal vote changes only");
    assert!(h.engine.governance().pending.is_empty());
}

#[test]
fn a_vote_moves_the_cap_within_its_bounds_only() {
    let mut h = governed(ProtocolVersion::V89, 0);
    assert_eq!(h.engine.award_cap(), LAUNCH_CAP);
    let outside = [
        AwardCap { excess_bps: 499, max: UsdcUnits(50_000 * USDC) },
        AwardCap { excess_bps: 2_001, max: UsdcUnits(50_000 * USDC) },
        AwardCap { excess_bps: 500, max: UsdcUnits(50_000 * USDC - 1) },
        AwardCap { excess_bps: 500, max: UsdcUnits(250_000 * USDC + 1) },
    ];
    for (proposal, cap) in (50..).zip(outside) {
        refused(&pass(&mut h, proposal, false, &AdminCommand::SetAwardCap(cap)), "award cap");
    }
    let widest = AwardCap { excess_bps: 2_000, max: UsdcUnits(250_000 * USDC) };
    vote_in(&mut h, 60, &AdminCommand::SetAwardCap(widest));
    assert_eq!(h.engine.award_cap(), widest);
}

#[test]
fn an_award_and_the_cap_each_move_the_root_alone() {
    // Engines that run the same commands at the same seqs and times, and differ only in what one vote names.
    let root_after = |command: AdminCommand| {
        let mut h = governed(ProtocolVersion::V89, 0);
        vote_in(&mut h, 70, &command);
        h.engine.state_root()
    };
    let medium = root_after(award(RESEARCHER, Severity::Medium, "GHSA-1"));
    assert_eq!(medium, root_after(award(RESEARCHER, Severity::Medium, "GHSA-1")));
    assert_ne!(medium, root_after(award(RESEARCHER, Severity::Medium, "GHSA-2")), "the advisory is committed");
    assert_ne!(medium, root_after(award(SECOND, Severity::Medium, "GHSA-1")), "the recipient is committed");
    assert_ne!(medium, root_after(award(RESEARCHER, Severity::Low, "GHSA-1")), "the severity is committed");
    let cap = |excess_bps| AdminCommand::SetAwardCap(AwardCap { excess_bps, max: UsdcUnits(60_000 * USDC) });
    assert_ne!(root_after(cap(700)), root_after(cap(800)), "the cap is committed");
}

#[test]
fn a_replay_reproduces_the_awards_the_cap_and_the_root() {
    let mut h = governed(ProtocolVersion::V89, 0);
    vote_in(&mut h, 70, &award(RESEARCHER, Severity::Medium, "GHSA-1"));
    vote_in(&mut h, 71, &AdminCommand::SetAwardCap(AwardCap { excess_bps: 700, max: UsdcUnits(60_000 * USDC) }));
    let expected = h.engine.state_root();

    let mut replica = engine_core::Engine::new();
    for sequenced in &h.log {
        replica.apply(sequenced).unwrap();
    }
    assert_eq!(replica.awards(), h.engine.awards());
    assert_eq!(replica.award_cap(), h.engine.award_cap());
    assert_eq!(replica.state_root(), expected);
}
