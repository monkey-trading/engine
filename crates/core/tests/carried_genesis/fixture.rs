//! The source log a carried genesis takes its state from, and the new log whose genesis carries it.

use alloy_primitives::B256;
use engine_core::Fixed;
use engine_core::access::AccessPolicy;
use engine_core::awards::{AwardApproval, Severity};
use engine_core::carry::CarryPart;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::event::Event;
use engine_core::governance::{GovernanceConfig, PassedProposal, VotedChange, command_hash};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::protocol::ProtocolVersion;
use engine_core::staking::StakingConfig;
use engine_core::turns::{RegistryRotationSet, SetMember};
use engine_core::types::{AccountId, Millis, Side};

use crate::common::close_spread::{attest, fund, source_key, trade};
use crate::common::events::only_fill;
use crate::common::genesis::node_genesis;
use crate::common::{BTC, Client, HOUSE_CAPITAL, Harness};

pub const DAY_MS: Millis = 24 * 60 * 60 * 1000;
/// The source's period budget, in MONKEY base units.
pub const PERIOD_BUDGET: u128 = 1_000_000_000_000_000;
/// The vote contract the source records.
const VOTE: AccountId = AccountId([0x70; 20]);
/// The researcher the source's award is credited to.
pub const RESEARCHER: AccountId = AccountId([0x7e; 20]);
/// The source's stakers and what each stakes; the first later unstakes part of it.
pub const STAKERS: [(AccountId, u128); 3] =
    [(AccountId([0xa1; 20]), 300_000), (AccountId([0xa2; 20]), 100_000), (AccountId([0xa3; 20]), 50_000)];
/// What the first staker unstakes in the source.
pub const UNSTAKED: u128 = 20_000;
/// The seeds of the traders whose losses earned the source's entitlements.
pub const TRADER_SEEDS: [u8; 2] = [0x91, 0x92];
/// The house's excess over the staking floor in each log, in USD.
pub const EXCESS: i64 = 2_000_000;

/// The staking both logs initialise from `start`: a floor `EXCESS` below the house capital, one-day epochs
/// and a three-day cooldown.
pub fn staking_config(start: Millis) -> StakingConfig {
    StakingConfig {
        liquidity_floor: Fixed::from_int(HOUSE_CAPITAL - EXCESS),
        epoch_ms: DAY_MS,
        start_ms: start,
        cooldown_ms: 3 * DAY_MS,
    }
}

/// `node_genesis` at the harness start.
pub fn genesis(version: ProtocolVersion) -> Harness {
    node_genesis(version, crate::common::start())
}

/// The trader from `seed`, signing requests that expire a minute after `h`'s time.
pub fn trader(h: &Harness, seed: u8) -> Client {
    Client { expires_at: h.ts + 60_000, ..Client::new(seed) }
}

/// A source log at `V90` under EIP-712 client auth: house capital, emissions and staking initialised, two
/// traders whose losses earned entitlements in two periods, three stakers with part of one stake unstaked, a
/// Critical award voted in while a rotation set governed and paid in part by an epoch close, then rotation
/// switched off, the staking mirror's completed frontier past every block it mirrored, and the open emission
/// period with no results. Its root is computed at its last command.
pub fn source() -> Harness {
    let mut h = genesis(ProtocolVersion::V90);
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 30 * DAY_MS }));
    h.cmd(Command::HouseCapital { amount: Fixed::from_int(HOUSE_CAPITAL), chain_ref: "genesis".into() });
    let schedule = EmissionSchedule { start_ms: h.ts, period_budget: TokenUnits(PERIOD_BUDGET) };
    h.operator_admin(AdminCommand::InitEmissions(schedule));
    h.operator_admin(AdminCommand::InitStaking(staking_config(h.ts)));
    for seed in TRADER_SEEDS {
        let client = trader(&h, seed);
        attest(&mut h, &client);
        fund(&mut h, &client, "1000000");
    }
    // Each trader loses in a period of its own, which the next command closes.
    for seed in TRADER_SEEDS {
        lose(&mut h, seed);
        next_period(&mut h);
    }
    for (block, (account, amount)) in (1..).zip(STAKERS) {
        stake(&mut h, account, amount, block);
    }
    let unstake = Command::Unstake {
        account: STAKERS[0].0,
        amount: TokenUnits(UNSTAKED),
        block: 4,
        chain_ref: "0xunstake:4".into(),
    };
    assert!(matches!(h.cmd(unstake)[..], [Event::Unstaked { .. }]));
    vote_award(&mut h);
    switch_rotation(&mut h, false, 6);
    // The mirror passes every epoch now due, whose first close pays the award in part.
    let config = h.engine.staking().expect("staking is initialised").config;
    let end = config.epoch_end(config.epochs_elapsed(h.ts) - 1);
    let block_hash = B256::repeat_byte(7);
    let progress = Command::StakingProgress { block: 7, block_hash, block_time: Some(end), named_turn: None };
    assert!(!h.cmd(progress).iter().any(|event| matches!(event, Event::AdminRejected { .. })));
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|event| matches!(event, Event::AwardPaid(_))), "{events:?}");
    h.engine.state_root();
    h
}

/// The trader from `seed` buys a BTC at 100,000 and sells it at 99,000.
pub fn lose(h: &mut Harness, seed: u8) {
    let client = trader(h, seed);
    for (price, side) in [("100000", Side::Buy), ("99000", Side::Sell)] {
        h.price(BTC, price);
        only_fill(&trade(h, &client, BTC, side, "1", price));
    }
}

/// Ticks at the end of the open emission period, closing it.
pub fn next_period(h: &mut Harness) -> Vec<Event> {
    let emissions = h.engine.emissions().expect("emissions are initialised");
    h.advance_to(emissions.schedule.period_end(emissions.period));
    h.cmd(Command::Tick)
}

/// Mirrors `account` staking `amount` in `block`.
pub fn stake(h: &mut Harness, account: AccountId, amount: u128, block: u64) -> Vec<Event> {
    let chain_ref = format!("0xstake:{block}");
    h.cmd(Command::Stake { account, amount: TokenUnits(amount), block, chain_ref })
}

/// Records the vote contract, a one-member rotation set and rotation on, then passes and applies a Critical
/// award to `RESEARCHER` seven days later.
fn vote_award(h: &mut Harness) {
    h.admin(AdminCommand::SetGovernance(GovernanceConfig { vote: VOTE, restriction_ttl_ms: DAY_MS }));
    let member =
        SetMember { candidate: AccountId([0xc1; 20]), admin_key: Client::operator().account, weight: TokenUnits(1) };
    let adoption = RegistryRotationSet {
        set: 1,
        epoch: 1,
        effective_at: h.ts,
        quorum: true,
        members: vec![member],
        block: 5,
        chain_ref: "0xset:5".into(),
    };
    assert!(h.cmd(Command::AdoptRotationSet(adoption)).iter().any(|e| matches!(e, Event::RotationSetAdopted { .. })));
    switch_rotation(h, true, 5);
    let award = AdminCommand::ApproveAward(AwardApproval {
        recipient: RESEARCHER,
        severity: Severity::Critical,
        advisory: "GHSA-c4rr-y0v3-r9x1".into(),
        amount: Severity::Critical.amount(),
    });
    let passed = PassedProposal {
        proposal: 1,
        fast: false,
        command_hash: command_hash(&award),
        command: Some(award.clone()),
        block: 5,
        chain_ref: "0xvote:5".into(),
    };
    let events = h.cmd(Command::PassProposal(Box::new(passed)));
    assert!(matches!(events.last(), Some(Event::ProposalScheduled { .. })), "{events:?}");
    h.advance(NOTICE_MS);
    let events = h.cmd(Command::ApplyProposal(Box::new(VotedChange { proposal: 1, command: award })));
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
}

/// Mirrors the registry turning rotation on or off in `block`.
fn switch_rotation(h: &mut Harness, enabled: bool, block: u64) {
    let chain_ref = format!("0xrotation:{enabled}");
    let events = h.cmd(Command::SwitchRotation { enabled, election_cancelled: false, block, chain_ref });
    assert!(events.iter().any(|e| matches!(e, Event::RotationSwitched { .. })), "{events:?}");
}

/// A new log's genesis before its carry, a day after `source`'s last command: client auth, the operator, `V90`,
/// the parameters and markets, the access policy, and staking initialised.
pub fn destination(source: &Harness) -> Harness {
    destination_at(source, ProtocolVersion::V90)
}

/// `destination` under `version`.
pub fn destination_at(source: &Harness, version: ProtocolVersion) -> Harness {
    let mut h = node_genesis(version, source.ts + DAY_MS);
    h.admin(AdminCommand::SetAccessPolicy(AccessPolicy { source: source_key(), max_age_ms: 30 * DAY_MS }));
    h.admin(AdminCommand::InitStaking(staking_config(h.ts)));
    h
}

/// Sequences `part` as the node does, returning its events.
pub fn part(h: &mut Harness, part: CarryPart) -> Vec<Event> {
    h.cmd(Command::Admin(AdminCommand::CarryState(part)))
}

/// Sequences every part, each of which must apply, and returns the last's events.
pub fn carry(h: &mut Harness, parts: &[CarryPart]) -> Vec<Event> {
    let mut events = Vec::new();
    for carried in parts {
        events = part(h, carried.clone());
        assert_eq!(events.last(), Some(&Event::AdminApplied), "{carried:?}: {events:?}");
    }
    events
}

/// The reason the last event refused a part with.
pub fn refusal(events: &[Event]) -> &str {
    match events.last() {
        Some(Event::AdminRejected { reason }) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    }
}
