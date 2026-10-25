//! MONKEY staking and USDC distributions from `V25`, end to end through `Engine::apply`: stake and
//! unstake mirroring, epoch finalisation above and below the liquidity floor, pro-rata rounding,
//! skipped epochs, and distribution proofs against the state root.

mod common;

use common::emissions::{funded, round_trip};
use common::{Harness, acct, f};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::{Event, Reject};
use engine_core::staking::{StakingConfig, UsdcUnits};

const EPOCH_MS: i64 = 600_000;
const COOLDOWN_MS: i64 = 86_400_000;
/// One USDC base unit in `Fixed`'s raw units (9 decimals against USDC's 6).
const USDC: i128 = 1_000;

/// A `V25` harness staking with a 1m floor, the genesis house holding 100m.
fn staking_harness() -> Harness {
    Harness::staking(f("1000000"), EPOCH_MS, COOLDOWN_MS)
}

fn config(h: &Harness) -> StakingConfig {
    h.engine.staking().expect("staking is initialised").config
}

/// Stakes `amount` for `account` under `chain_ref`, which must apply.
fn stake(h: &mut Harness, account: engine_core::types::AccountId, amount: u128, chain_ref: &str) {
    let events = h.cmd(Command::Stake { account, amount: TokenUnits(amount), chain_ref: chain_ref.into() });
    assert!(matches!(events[..], [Event::Staked { .. }]), "stake rejected: {events:?}");
}

fn next_epoch(h: &mut Harness) -> Vec<Event> {
    let open = h.engine.staking().expect("staking is initialised").epoch;
    h.advance_to(config(h).epoch_end(open));
    h.cmd(Command::Tick)
}

#[test]
fn initialisation_needs_v25_and_records_the_config_once() {
    let mut h = Harness::v21();
    let config =
        StakingConfig { liquidity_floor: f("1000000"), epoch_ms: EPOCH_MS, start_ms: h.ts, cooldown_ms: COOLDOWN_MS };
    let events = h.operate(AdminCommand::InitStaking(config));
    assert!(matches!(events[..], [Event::AdminRejected { .. }]), "V21 refuses staking: {events:?}");

    h.admin(AdminCommand::ActivateProtocol { version: engine_core::protocol::ProtocolVersion::V25 });
    h.operator_admin(AdminCommand::InitStaking(config));
    assert!(
        h.operate(AdminCommand::InitStaking(config))
            .contains(&Event::AdminRejected { reason: "staking is already initialised".into() })
    );
}

#[test]
fn stake_and_unstake_mirror_finalised_events_credited_once() {
    let mut h = staking_harness();
    let [a] = funded(&mut h);
    stake(&mut h, a, 1_000, "0xstake:0");
    stake(&mut h, a, 500, "0xstake:1");
    assert_eq!(h.engine.staked(a), TokenUnits(1_500));

    let events = h.cmd(Command::Stake { account: a, amount: TokenUnits(1), chain_ref: "0xstake:0".into() });
    assert!(matches!(events[..], [Event::StakingRejected { reason: Reject::DuplicateReference, .. }]), "{events:?}");
    let events = h.cmd(Command::Stake { account: a, amount: TokenUnits(0), chain_ref: "0xstake:9".into() });
    assert!(matches!(events[..], [Event::StakingRejected { reason: Reject::InvalidAmount, .. }]), "{events:?}");

    let events = h.cmd(Command::Unstake { account: a, amount: TokenUnits(2_000), chain_ref: "0xunstake:0".into() });
    assert!(matches!(events[..], [Event::StakingRejected { reason: Reject::InsufficientStake, .. }]), "{events:?}");
    let events = h.cmd(Command::Unstake { account: a, amount: TokenUnits(1_500), chain_ref: "0xunstake:0".into() });
    assert!(matches!(events[..], [Event::Unstaked { .. }]), "{events:?}");
    assert_eq!(h.engine.staked(a), TokenUnits(0));
    assert!(!h.engine.staked_balances().contains_key(&a));
}

#[test]
fn stake_and_unstake_refuse_before_the_config_is_initialised() {
    let mut h = Harness::v21();
    let a = acct(1);
    let events = h.cmd(Command::Stake { account: a, amount: TokenUnits(1), chain_ref: "0xstake:0".into() });
    assert!(matches!(events[..], [Event::StakingRejected { reason: Reject::StakingNotInitialised, .. }]));
    let events = h.cmd(Command::Unstake { account: a, amount: TokenUnits(1), chain_ref: "0xunstake:0".into() });
    assert!(matches!(events[..], [Event::StakingRejected { reason: Reject::StakingNotInitialised, .. }]));
}

#[test]
fn an_epoch_above_the_floor_pays_stakers_pro_rata_in_whole_usdc_units() {
    let mut h = staking_harness();
    let [a, b] = funded(&mut h);
    // Losses of 100 and 300 make house NAV 100,000,400; the floor leaves 99,000,400 USDC above it.
    round_trip(&mut h, &[(a, "1"), (b, "3")], "100000", "99900");
    stake(&mut h, a, 1, "0xstake:0");
    stake(&mut h, b, 3, "0xstake:1");

    let cash_before = h.engine.house_cash();
    let events = next_epoch(&mut h);
    let allocated: u128 = [a, b].iter().map(|account| h.engine.distribution(*account).0).sum();
    assert_eq!(h.engine.distribution(a), UsdcUnits(24_750_100_000_000));
    assert_eq!(h.engine.distribution(b), UsdcUnits(74_250_300_000_000));
    // House cash fell by exactly the allocated total; the floor stays as house liquidity.
    assert_eq!(h.engine.house_cash(), cash_before - Fixed::from_raw(allocated as i128 * USDC));
    assert_eq!(h.engine.house_cash(), f("1000000"));
    assert!(events.iter().any(|e| matches!(e, Event::DistributionAllocated { account, epoch: 0, amount }
        if *account == a && *amount == UsdcUnits(24_750_100_000_000))));
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { epoch: 0, allocated, excess, .. }
        if *allocated == UsdcUnits(99_000_400_000_000) && *excess == UsdcUnits(99_000_400_000_000))));
    // Cash is conserved: the allocation moved from house cash to the stakers' claimable distributions.
    h.assert_conserved(Fixed::from_raw(allocated as i128 * USDC));
}

#[test]
fn nav_at_or_below_the_floor_pays_nothing() {
    // A floor above the house's 100m NAV: every epoch closes at or below it.
    let mut h = Harness::staking(f("200000000"), EPOCH_MS, COOLDOWN_MS);
    let [a] = funded(&mut h);
    stake(&mut h, a, 1, "0xstake:0");
    round_trip(&mut h, &[(a, "1")], "100000", "99900");
    let cash = h.engine.house_cash();
    let events = next_epoch(&mut h);
    assert_eq!(h.engine.distribution(a), UsdcUnits(0));
    assert_eq!(h.engine.house_cash(), cash);
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { epoch: 0, excess, allocated, .. }
        if *excess == UsdcUnits(0) && *allocated == UsdcUnits(0))));
}

#[test]
fn epochs_whose_end_no_command_witnessed_are_skipped() {
    let mut h = staking_harness();
    let [a] = funded(&mut h);
    stake(&mut h, a, 1, "0xstake:0");
    h.advance_to(config(&h).epoch_end(3));
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochsSkipped { first: 0, last: 2 })));
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { epoch: 3, .. })));
    // One distribution, for the latest epoch only, at the snapshot now.
    assert_eq!(h.engine.distribution(a), UsdcUnits(99_000_000_000_000));
    assert_eq!(h.engine.staking().unwrap().epoch, 4);
}

#[test]
fn a_later_close_adds_to_the_cumulative_distribution_and_proves_against_the_root() {
    let mut h = staking_harness();
    let [a] = funded(&mut h);
    stake(&mut h, a, 1, "0xstake:0");
    next_epoch(&mut h);
    assert_eq!(h.engine.distribution(a), UsdcUnits(99_000_000_000_000));

    // A paying epoch reduces NAV to the floor; another loss, and the next epoch pays its excess
    // too, cumulatively.
    round_trip(&mut h, &[(a, "1")], "100000", "99900");
    next_epoch(&mut h);
    assert_eq!(h.engine.distribution(a), UsdcUnits(99_000_100_000_000));

    let root = h.engine.state_root();
    let proof = h.engine.distribution_proof(a).expect("a distribution leaf exists");
    assert!(proof.verify(root.into()), "the cumulative distribution proves against the state root");
    assert_eq!(proof.entitled, UsdcUnits(99_000_100_000_000));
}
