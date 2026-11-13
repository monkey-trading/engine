//! Parameter changes by stake-weighted operator vote from `V56`, through `Engine::apply`: passed
//! proposals mirrored once, a normal change applying no sooner than seven days after it is logged, a
//! fast-path restriction in force at once and lapsing at its lifetime, the values in force the
//! stricter of the baseline and every restriction, and the operator's and the node's own commands
//! for the governed fields refused while rotation is on.

mod common;

use common::close_spread::{ceilings, install_expect};
use common::{AAPL, BTC, ETH, Harness, acct, f};
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::governance::{GovernanceConfig, Limit, PassedProposal, VotedChange, command_hash, decode_command};
use engine_core::market::{MarketSpec, Tier};
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice};
use engine_core::params::{ProtocolParams, StressLevel};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, MarketId, Millis, Side};

/// The vote contract the fixture records.
const VOTE: AccountId = AccountId([0x70; 20]);
/// How long a restriction lasts: a day.
const TTL: Millis = 24 * 60 * 60 * 1000;

/// `V21` with three stress levels, a trader holding 9 BTC at 10% margin on 15,000 of equity, then
/// staking, `V37`, the ceilings, `V56`, the recorded vote contract and rotation on.
fn governed() -> Harness {
    governed_with(TTL)
}

/// `governed`, with restrictions lasting `ttl`.
fn governed_with(ttl: Millis) -> Harness {
    let mut h = Harness::v21();
    let mut parameters = h.engine.params().expect("the fixture sets parameters").clone();
    let top = StressLevel { min_spread_bps: 8, trigger: None, ..parameters.stress_levels[1].clone() };
    parameters.stress_levels.push(top);
    h.operator_admin(AdminCommand::SetParams(parameters));
    h.deposit(acct(1), "15000");
    h.price(BTC, "10000");
    let events = h.market_order(acct(1), BTC, Side::Buy, "9", "10000");
    assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V25 });
    let config = engine_core::StakingConfig {
        liquidity_floor: f("1000000"),
        epoch_ms: 24 * 60 * 60 * 1000,
        start_ms: h.ts,
        cooldown_ms: 3 * 24 * 60 * 60 * 1000,
    };
    h.operator_admin(AdminCommand::InitStaking(config));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V37 });
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V56 });
    h.admin(AdminCommand::SetGovernance(GovernanceConfig { vote: VOTE, restriction_ttl_ms: ttl }));
    switch_rotation(&mut h, true);
    h
}

fn switch_rotation(h: &mut Harness, enabled: bool) {
    let (block, chain_ref) = (h.log.len() as u64, format!("0xrotation:{}", h.log.len()));
    let events = h.cmd(Command::SwitchRotation { enabled, election_cancelled: false, block, chain_ref });
    assert!(matches!(events.last(), Some(Event::RotationSwitched { .. })), "{events:?}");
}

/// Mirrors proposal `proposal` passing on `fast` with `command`.
fn pass(h: &mut Harness, proposal: u64, fast: bool, command: AdminCommand) -> Vec<Event> {
    let command_hash = command_hash(&command);
    let (block, chain_ref) = (h.log.len() as u64, format!("0xvote:{proposal}:{}", h.log.len()));
    let passed = PassedProposal { proposal, fast, command_hash, command: Some(command), block, chain_ref };
    h.cmd(Command::PassProposal(Box::new(passed)))
}

/// Sequences the node's `ApplyProposal` for `proposal` with `command`.
fn apply(h: &mut Harness, proposal: u64, command: AdminCommand) -> Vec<Event> {
    h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal, command })))
}

/// The baseline spec of `market` with its single margin band at `bps`.
fn margin(h: &Harness, market: MarketId, bps: i64) -> AdminCommand {
    let spec = h.engine.baseline_spec(market).expect("listed");
    AdminCommand::UpsertMarket(MarketSpec { margin_tiers: vec![Tier { up_to: None, bps }], ..spec })
}

fn margin_bps(h: &Harness, market: MarketId) -> Vec<Tier> {
    h.engine.markets()[&market].spec.margin_tiers.clone()
}

fn one_band(bps: i64) -> Vec<Tier> {
    vec![Tier { up_to: None, bps }]
}

fn with_params(h: &Harness, change: impl FnOnce(&mut ProtocolParams)) -> AdminCommand {
    let mut params = h.engine.baseline_params().expect("set");
    change(&mut params);
    AdminCommand::SetParams(params)
}

fn level(h: &Harness, market: MarketId) -> usize {
    h.engine.markets()[&market].stress.level
}

fn refused(events: &[Event], reason: &str) {
    assert!(
        matches!(events.last(), Some(Event::ProposalRefused { reason: r, .. }) if r.contains(reason)),
        "{events:?}"
    );
}

fn rejected(events: &[Event], reason: &str) {
    assert!(matches!(events.last(), Some(Event::AdminRejected { reason: r }) if r.contains(reason)), "{events:?}");
}

#[test]
fn a_vote_mirror_needs_v56_and_the_recorded_contract_and_credits_once() {
    let mut h = Harness::v37();
    let events = pass(&mut h, 1, false, AdminCommand::SetReduceOnly { market: None, reduce_only: true });
    assert!(
        matches!(events.last(), Some(Event::ElectionRejected { reason, .. }) if reason.contains("need protocol V56")),
        "{events:?}"
    );
    install_expect(&mut h, ceilings());
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V56 });
    let command = margin(&h, BTC, 1_500);
    let events = pass(&mut h, 1, false, command.clone());
    assert!(
        matches!(events.last(), Some(Event::ElectionRejected { reason, .. }) if reason.contains("no vote contract")),
        "{events:?}"
    );

    let mut h = governed();
    let events = pass(&mut h, 1, false, command.clone());
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { proposal: 1, .. })), "{events:?}");
    let replay = h.log.last().expect("the mirror").cmd.clone();
    let events = h.cmd(replay);
    assert!(
        matches!(events.last(), Some(Event::ElectionRejected { reason, .. }) if reason.contains("already mirrored")),
        "{events:?}"
    );
    // A command that does not hash to the proposal's contradicts the chain, and holds the watcher.
    let wrong = PassedProposal {
        proposal: 2,
        fast: false,
        command_hash: command_hash(&AdminCommand::SetReduceOnly { market: None, reduce_only: true }),
        command: Some(command),
        block: h.log.len() as u64,
        chain_ref: "0xwrong:0".into(),
    };
    let events = h.cmd(Command::PassProposal(Box::new(wrong)));
    assert!(
        matches!(events.last(), Some(Event::ElectionRejected { reason, .. }) if reason.contains("does not hash")),
        "{events:?}"
    );
    assert!(!h.engine.is_staking_credited("0xwrong:0"));
}

#[test]
fn a_normal_change_applies_no_sooner_than_seven_days_after_it_is_logged() {
    let mut h = governed();
    let command = margin(&h, BTC, 1_500);
    let logged = h.ts;
    let events = pass(&mut h, 1, false, command.clone());
    assert_eq!(events.last(), Some(&Event::ProposalScheduled { proposal: 1, applies_at: logged + NOTICE_MS }));
    assert_eq!(h.engine.due_change(logged + NOTICE_MS - 1), None);

    h.advance_to(logged + NOTICE_MS - 1);
    rejected(&apply(&mut h, 1, command.clone()), "applies from");
    assert_eq!(margin_bps(&h, BTC), one_band(1_000), "nothing applies before its time");
    h.advance(1);
    assert_eq!(h.engine.due_change(h.ts).map(|pending| pending.proposal), Some(1));
    let other = margin(&h, BTC, 1_600);
    rejected(&apply(&mut h, 1, other), "waits with another command");
    rejected(&apply(&mut h, 2, command.clone()), "no change waits under proposal 2");
    assert_eq!(apply(&mut h, 1, command.clone()).last(), Some(&Event::AdminApplied));
    assert_eq!(margin_bps(&h, BTC), one_band(1_500));
    // Spent: it applies once.
    rejected(&apply(&mut h, 1, command), "no change waits under proposal 1");
}

#[test]
fn a_voted_rate_change_needs_no_parameter_notice_but_keeps_the_launch_range() {
    let mut h = governed();
    let double = with_params(&h, |p| p.profit_fee_bps = 2_000);
    pass(&mut h, 1, false, double.clone());
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 1, double), "outside ±50% of its launch value 1000");
    let raise = with_params(&h, |p| p.profit_fee_bps = 1_400);
    pass(&mut h, 2, false, raise.clone());
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 2, raise).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.params().unwrap().profit_fee_bps, 1_400);
}

#[test]
fn while_rotation_is_on_the_operators_and_the_nodes_governed_commands_are_refused() {
    let mut h = governed();
    let fee = with_params(&h, |p| p.profit_fee_bps = 1_200);
    let notice = AdminCommand::AnnounceParams(ParamsNotice {
        profit_fee_bps: Some(1_200),
        closeout_level_bps: None,
        stress: None,
        score_tiers: None,
        profit_fee_curve: None,
        effective_at: h.ts + NOTICE_MS,
    });
    for command in [fee.clone(), margin(&h, BTC, 1_500), notice.clone()] {
        rejected(&h.operate(command), "only a passed vote changes this");
    }
    let unsigned = h.cmd(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V56 }));
    rejected(&unsigned, "only a passed vote applies this command");
    // The levers kept with the operator still apply.
    assert_eq!(
        h.operate(AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true }).last(),
        Some(&Event::AdminApplied)
    );
    assert_eq!(
        h.operate(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 }).last(),
        Some(&Event::AdminApplied)
    );

    // With rotation off, the timelock governs and the operator's notices apply again.
    switch_rotation(&mut h, false);
    assert_eq!(h.operate(notice).last(), Some(&Event::AdminApplied));
    h.advance(NOTICE_MS);
    assert_eq!(h.operate(fee).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.params().unwrap().profit_fee_bps, 1_200);
}

#[test]
fn a_proposal_mirrored_with_rotation_off_or_no_canonical_command_is_credited_and_refused() {
    let mut h = governed();
    let passed = PassedProposal {
        proposal: 1,
        fast: false,
        command_hash: Default::default(),
        command: None,
        block: h.log.len() as u64,
        chain_ref: "0xbytes:0".into(),
    };
    refused(&h.cmd(Command::PassProposal(Box::new(passed))), "not an admin command's canonical JSON");
    assert!(h.engine.is_staking_credited("0xbytes:0"));
    switch_rotation(&mut h, false);
    let command = margin(&h, BTC, 1_500);
    refused(&pass(&mut h, 2, false, command), "rotation is off");
    assert!(h.engine.governance().pending.is_empty());
}

#[test]
fn a_normal_vote_takes_only_governed_commands() {
    let mut h = governed();
    let floor = AdminCommand::SetStressFloor { market: Some(BTC), level: 2 };
    refused(&pass(&mut h, 1, false, floor), "a normal vote changes only");
    let rate = AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 400 };
    refused(&pass(&mut h, 2, false, rate), "a normal vote changes only");
    assert!(h.engine.governance().pending.is_empty());
}

#[test]
fn a_fast_path_stress_floor_applies_at_once_survives_the_operator_and_lapses_at_its_lifetime() {
    let mut h = governed();
    let logged = h.ts;
    let events = pass(&mut h, 1, true, AdminCommand::SetStressFloor { market: Some(BTC), level: 2 });
    assert!(events.contains(&Event::RestrictionApplied { proposal: 1, expires_at: logged + TTL }), "{events:?}");
    assert_eq!((level(&h, BTC), level(&h, ETH)), (2, 0));

    // The operator's own floor sits beside it: setting or clearing it leaves level 2 in force.
    for operator_level in [1, 0] {
        let events = h.operate(AdminCommand::SetStressFloor { market: Some(BTC), level: operator_level });
        assert_eq!(events.last(), Some(&Event::AdminApplied));
        assert_eq!(level(&h, BTC), 2);
    }
    rejected(&h.operate(AdminCommand::SetStressFloor { market: Some(BTC), level: 2 }), "at most level 1");

    h.advance_to(logged + TTL - 1);
    h.cmd(Command::Tick);
    assert_eq!(level(&h, BTC), 2, "in force a millisecond before its lifetime ends");
    h.advance(1);
    let events = h.cmd(Command::Tick);
    assert!(events.contains(&Event::RestrictionLapsed { proposal: 1 }), "{events:?}");
    assert_eq!(level(&h, BTC), 0);
    assert!(h.engine.governance().restrictions.is_empty());
}

#[test]
fn reduce_only_by_vote_survives_the_operators_reduce_only_switch() {
    let mut h = governed();
    pass(&mut h, 1, true, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true });
    let reduce_only = |h: &Harness, market| h.engine.is_reduce_only(&h.engine.markets()[&market]);
    assert!(reduce_only(&h, BTC) && !reduce_only(&h, ETH));
    for operator in [true, false] {
        let events = h.operate(AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: operator });
        assert_eq!(events.last(), Some(&Event::AdminApplied));
        assert!(reduce_only(&h, BTC), "the operator's switch leaves the restriction in force");
    }
    for global in [true, false] {
        h.operate(AdminCommand::SetReduceOnly { market: None, reduce_only: global });
    }
    assert!(reduce_only(&h, BTC) && !reduce_only(&h, ETH));
    h.advance(TTL);
    h.cmd(Command::Tick);
    assert!(!reduce_only(&h, BTC));
}

#[test]
fn a_fast_path_proposal_moving_any_field_away_from_lower_risk_is_refused() {
    let mut h = governed();
    let command = margin(&h, BTC, 900);
    refused(&pass(&mut h, 1, true, command), "lowers a margin band or raises a house cap");
    let wider = {
        let AdminCommand::UpsertMarket(spec) = margin(&h, BTC, 1_500) else { unreachable!() };
        AdminCommand::UpsertMarket(MarketSpec { max_slippage_bps: 30, ..spec })
    };
    refused(&pass(&mut h, 2, true, wider), "changes another field");
    let cap = {
        let spec = h.engine.baseline_spec(BTC).unwrap();
        AdminCommand::UpsertMarket(MarketSpec { house_cap_bps: spec.house_cap_bps + 1, ..spec })
    };
    refused(&pass(&mut h, 3, true, cap), "raises a house cap");
    let command = with_params(&h, |p| p.closeout_level_bps = 4_000);
    refused(&pass(&mut h, 4, true, command), "lowers the close-out level");
    let command = with_params(&h, |p| p.profit_fee_bps = 900);
    refused(&pass(&mut h, 5, true, command), "changes another parameter");
    let command = with_params(&h, |p| p.closeout_level_bps = 8_000);
    refused(&pass(&mut h, 6, true, command), "outside ±50%");
    refused(&pass(&mut h, 7, true, AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: false }), "adds risk");
    refused(&pass(&mut h, 8, true, AdminCommand::SetStressFloor { market: Some(BTC), level: 0 }), "level from 1");
    refused(&pass(&mut h, 9, true, AdminCommand::SetStressFloor { market: Some(BTC), level: 3 }), "level from 1");
    let command = margin(&h, BTC, 1_000);
    refused(&pass(&mut h, 10, true, command), "restricts nothing");
    let rate = AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 400 };
    refused(&pass(&mut h, 11, true, rate), "the fast path takes only");
    assert!(h.engine.governance().restrictions.is_empty());
    assert_eq!(margin_bps(&h, BTC), one_band(1_000));
}

#[test]
fn a_fast_path_margin_raise_closes_out_at_once_and_the_baseline_returns_at_its_lapse() {
    let mut h = governed();
    // 9 BTC at 10,000 is 90,000 of notional: 9,000 of margin at 10%, 36,000 at 40%, whose half is
    // more than the account's 15,000 of equity.
    let command = margin(&h, BTC, 4_000);
    let events = pass(&mut h, 1, true, command);
    assert!(events.iter().any(|e| matches!(e, Event::CloseOut { account, .. } if *account == acct(1))), "{events:?}");
    assert_eq!(margin_bps(&h, BTC), one_band(4_000));
    assert_eq!(h.engine.baseline_spec(BTC).unwrap().margin_tiers, one_band(1_000));
    h.advance(TTL);
    h.cmd(Command::Tick);
    assert_eq!(margin_bps(&h, BTC), one_band(1_000));
    assert!(h.engine.governance().market_baselines.is_empty());
}

#[test]
fn a_normal_vote_passed_under_a_restriction_moves_the_baseline_and_survives_the_lapse() {
    let mut h = governed();
    let voted = margin(&h, ETH, 1_100);
    pass(&mut h, 1, false, voted.clone());
    h.advance(NOTICE_MS - TTL / 2);
    let command = margin(&h, ETH, 1_500);
    pass(&mut h, 2, true, command);
    assert_eq!(margin_bps(&h, ETH), one_band(1_500));
    h.advance(TTL / 2);
    assert_eq!(apply(&mut h, 1, voted).last(), Some(&Event::AdminApplied));
    // The baseline moved, but the restriction's stricter band stays in force over it.
    assert_eq!(h.engine.baseline_spec(ETH).unwrap().margin_tiers, one_band(1_100));
    assert_eq!(margin_bps(&h, ETH), one_band(1_500));
    h.advance(TTL / 2);
    h.cmd(Command::Tick);
    assert_eq!(margin_bps(&h, ETH), one_band(1_100), "the voted baseline survives the lapse");
}

#[test]
fn the_values_in_force_are_the_stricter_of_the_baseline_and_every_restriction() {
    let mut h = governed();
    let tiers = |bands: &[(Option<&str>, i64)]| -> Vec<Tier> {
        bands.iter().map(|(up_to, bps)| Tier { up_to: up_to.map(f), bps: *bps }).collect()
    };
    let spec = h.engine.baseline_spec(AAPL).unwrap();
    let banded = |bands| AdminCommand::UpsertMarket(MarketSpec { margin_tiers: tiers(bands), ..spec.clone() });
    pass(&mut h, 1, true, banded(&[(Some("1000000"), 3_000), (None, 2_000)]));
    pass(&mut h, 2, true, banded(&[(Some("500000"), 2_000), (None, 2_500)]));
    assert_eq!(
        margin_bps(&h, AAPL),
        tiers(&[(Some("500000"), 3_000), (Some("1000000"), 3_000), (None, 2_500)]),
        "each slice takes its highest rate, every bound kept"
    );
    let command = with_params(&h, |p| p.closeout_level_bps = 6_000);
    pass(&mut h, 3, true, command);
    let command = with_params(&h, |p| p.house_total_cap_bps = 10_000);
    pass(&mut h, 4, true, command);
    let params = h.engine.params().unwrap();
    assert_eq!((params.closeout_level_bps, params.house_total_cap_bps), (6_000, 10_000));
    assert_eq!(h.engine.baseline_params().unwrap().closeout_level_bps, 5_000);
    // A renewal replaces the identical restriction, moving its end.
    let renewal = with_params(&h, |p| p.closeout_level_bps = 6_000);
    h.advance(TTL / 2);
    pass(&mut h, 5, true, renewal);
    let restrictions = &h.engine.governance().restrictions;
    assert!(!restrictions.contains_key(&3) && restrictions[&5].expires_at == h.ts + TTL);
    assert!(matches!(restrictions[&5].limit, Limit::Params { closeout_level_bps: Some(6_000), .. }));
    h.advance(TTL / 2);
    h.cmd(Command::Tick);
    let params = h.engine.params().unwrap();
    assert_eq!((params.closeout_level_bps, params.house_total_cap_bps), (6_000, 20_000), "the renewal holds");
    h.advance(TTL / 2);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.params().unwrap().closeout_level_bps, 5_000);
    assert!(h.engine.governance().params_baseline.is_none());
}

#[test]
fn a_voted_ceiling_adoption_takes_effect_with_its_vote() {
    let mut h = governed();
    let adoption =
        AdminCommand::AdoptCloseSpreadCeiling { market: BTC, max_close_spread_bps: 18, effective_at: 0, listing: None };
    rejected(&h.operate(adoption.clone()), "only a passed vote changes this");
    pass(&mut h, 1, false, adoption.clone());
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 1, adoption).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.close_spread_ceilings().unwrap()[&BTC], 18);
    let raise =
        AdminCommand::AdoptCloseSpreadCeiling { market: BTC, max_close_spread_bps: 25, effective_at: 0, listing: None };
    pass(&mut h, 2, false, raise.clone());
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 2, raise), "can only be lowered");
}

#[test]
fn canonical_bytes_decode_and_anything_else_does_not() {
    let command = AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true };
    let bytes = serde_json::to_vec(&command).unwrap();
    assert_eq!(decode_command(&bytes), Some(command.clone()));
    let spaced = br#"{"action": "set_reduce_only", "market": 1, "reduce_only": true}"#;
    assert_eq!(decode_command(spaced), None, "a non-canonical encoding of the same command");
    assert_eq!(decode_command(b"not json"), None);
    assert_eq!(command_hash(&command), alloy_primitives::keccak256(&bytes));
}

#[path = "parameter_votes/baseline.rs"]
mod baseline;
#[path = "parameter_votes/holding_fee.rs"]
mod holding_fee;
#[path = "parameter_votes/limits.rs"]
mod limits;
#[path = "parameter_votes/listing.rs"]
mod listing;
#[path = "parameter_votes/toxicity.rs"]
mod toxicity;
