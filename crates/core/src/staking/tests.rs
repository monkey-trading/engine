//! Tests for MONKEY staking and USDC distributions.

use super::*;
use crate::event::Reject;

fn engine(now: Millis) -> Engine {
    Engine { protocol_version: ProtocolVersion::V25, now, ..Engine::default() }
}

fn config(now: Millis) -> StakingConfig {
    StakingConfig { liquidity_floor: Fixed::from_int(100), epoch_ms: 600_000, start_ms: now, cooldown_ms: 86_400_000 }
}

#[test]
fn initialisation_records_the_config_once_under_v25() {
    let mut engine = engine(1_000);
    engine.init_staking(&config(1_000)).expect("initialises");
    assert_eq!(engine.staking().unwrap().epoch, 0);
    assert_eq!(engine.init_staking(&config(1_000)), Err("staking is already initialised".into()));
}

#[test]
fn initialisation_needs_v25_and_a_config_valid_at_the_sequenced_time() {
    let mut before = Engine { protocol_version: ProtocolVersion::V21, now: 1_000, ..Engine::default() };
    assert_eq!(before.init_staking(&config(1_000)), Err("staking needs protocol V25".into()));

    let mut engine = engine(1_000);
    let late = StakingConfig { start_ms: 999, ..config(1_000) };
    assert!(engine.init_staking(&late).unwrap_err().contains("before the sequenced time"));
    let zero_epoch = StakingConfig { epoch_ms: 0, ..config(1_000) };
    assert!(engine.init_staking(&zero_epoch).unwrap_err().contains("epoch length"));
    let zero_floor = StakingConfig { liquidity_floor: Fixed::ZERO, ..config(1_000) };
    assert!(engine.init_staking(&zero_floor).unwrap_err().contains("liquidity floor"));
}

#[test]
fn stake_and_unstake_mirror_events_once_with_balance_checks() {
    let mut engine = engine(1_000);
    engine.init_staking(&config(1_000)).unwrap();
    let mut events = Vec::new();
    engine.stake(AccountId([1; 20]), TokenUnits(500), "0xstake:0", &mut events);
    engine.stake(AccountId([1; 20]), TokenUnits(300), "0xstake:1", &mut events);
    assert_eq!(engine.staked(AccountId([1; 20])), TokenUnits(800));
    engine.stake(AccountId([1; 20]), TokenUnits(1), "0xstake:0", &mut events);
    assert!(matches!(events.last(), Some(Event::StakingRejected { reason: Reject::DuplicateReference, .. })));
    engine.unstake(AccountId([1; 20]), TokenUnits(900), "0xunstake:0", &mut events);
    assert!(matches!(events.last(), Some(Event::StakingRejected { reason: Reject::InsufficientStake, .. })));
    engine.unstake(AccountId([1; 20]), TokenUnits(800), "0xunstake:0", &mut events);
    assert_eq!(engine.staked(AccountId([1; 20])), TokenUnits(0));
    assert!(!engine.staked_balances().contains_key(&AccountId([1; 20])));
}

#[test]
fn stake_and_unstake_refuse_until_initialised() {
    let mut engine = engine(1_000);
    let mut events = Vec::new();
    engine.stake(AccountId([1; 20]), TokenUnits(500), "0xstake:0", &mut events);
    assert!(matches!(events.last(), Some(Event::StakingRejected { reason: Reject::StakingNotInitialised, .. })));
    engine.unstake(AccountId([1; 20]), TokenUnits(500), "0xunstake:0", &mut events);
    assert!(matches!(events.last(), Some(Event::StakingRejected { reason: Reject::StakingNotInitialised, .. })));
}

#[test]
fn an_epoch_above_the_floor_pays_stakers_pro_rata_and_reduces_house_cash() {
    let mut engine = engine(1_000);
    engine.init_staking(&config(1_000)).unwrap();
    engine.house_cash = Fixed::from_int(1_000);
    let mut events = Vec::new();
    engine.stake(AccountId([1; 20]), TokenUnits(1), "0xa:0", &mut events);
    engine.stake(AccountId([2; 20]), TokenUnits(3), "0xa:1", &mut events);
    engine.now = config(1_000).epoch_end(0);
    engine.finalise_staking(&mut events);
    // Excess 900 USDC splits 25/75; house cash falls by exactly the allocated total.
    assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(225_000_000));
    assert_eq!(engine.distribution(AccountId([2; 20])), UsdcUnits(675_000_000));
    assert_eq!(engine.house_cash, Fixed::from_int(1_000) - Fixed::from_int(900));
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { epoch: 0, allocated, excess, .. }
            if *allocated == UsdcUnits(900_000_000) && *excess == UsdcUnits(900_000_000))));
    assert_eq!(engine.staking().unwrap().epoch, 1);
}

#[test]
fn an_epoch_at_or_below_the_floor_pays_nothing_and_keeps_house_cash() {
    let mut engine = engine(1_000);
    engine.init_staking(&config(1_000)).unwrap();
    engine.house_cash = Fixed::from_int(100);
    let mut events = Vec::new();
    engine.stake(AccountId([1; 20]), TokenUnits(1), "0xa:0", &mut events);
    engine.now = config(1_000).epoch_end(0);
    engine.finalise_staking(&mut events);
    assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(0));
    assert_eq!(engine.house_cash, Fixed::from_int(100));
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { epoch: 0, excess, allocated, .. }
            if *excess == UsdcUnits(0) && *allocated == UsdcUnits(0))));
}

#[test]
fn an_epoch_with_no_stakers_pays_nothing_and_keeps_house_cash() {
    let mut engine = engine(1_000);
    engine.init_staking(&config(1_000)).unwrap();
    engine.house_cash = Fixed::from_int(1_000);
    engine.now = config(1_000).epoch_end(0);
    let mut events = Vec::new();
    engine.finalise_staking(&mut events);
    assert_eq!(engine.house_cash, Fixed::from_int(1_000));
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { excess, allocated, .. }
            if *excess == UsdcUnits(900_000_000) && *allocated == UsdcUnits(0))));
}

#[test]
fn unwitnessed_epochs_are_skipped_and_only_the_latest_closes() {
    let mut engine = engine(1_000);
    engine.init_staking(&config(1_000)).unwrap();
    engine.house_cash = Fixed::from_int(1_000);
    let mut events = Vec::new();
    engine.stake(AccountId([1; 20]), TokenUnits(1), "0xa:0", &mut events);
    // Jump straight past the ends of epochs 0, 1, and 2.
    engine.now = config(1_000).epoch_end(2);
    engine.finalise_staking(&mut events);
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochsSkipped { first: 0, last: 1 })));
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { epoch: 2, .. })));
    // One distribution, for the latest epoch only, not one per elapsed epoch.
    assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(900_000_000));
    assert_eq!(engine.house_cash, Fixed::from_int(100));
    assert_eq!(engine.staking().unwrap().epoch, 3);
}

#[test]
fn sub_usdc_excess_allocates_nothing() {
    let mut engine = engine(1_000);
    engine.init_staking(&config(1_000)).unwrap();
    engine.house_cash = Fixed::from_int(100) + Fixed::from_raw(999);
    let mut events = Vec::new();
    engine.stake(AccountId([1; 20]), TokenUnits(1), "0xa:0", &mut events);
    engine.now = config(1_000).epoch_end(0);
    engine.finalise_staking(&mut events);
    // The excess is 999 raw: less than one USDC base unit, so nothing is allocated.
    assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(0));
    assert_eq!(engine.house_cash, Fixed::from_int(100) + Fixed::from_raw(999));
}

#[test]
fn per_staker_flooring_leaves_the_dust_with_the_house() {
    let mut engine = engine(1_000);
    engine.init_staking(&config(1_000)).unwrap();
    // 1,000.002999 in the house: a 900.002999 excess, 900_000_002 whole USDC units.
    engine.house_cash = Fixed::from_int(1_000) + Fixed::from_raw(2_999);
    let mut events = Vec::new();
    engine.stake(AccountId([1; 20]), TokenUnits(1), "0:a", &mut events);
    engine.stake(AccountId([2; 20]), TokenUnits(2), "0:b", &mut events);
    engine.now = config(1_000).epoch_end(0);
    engine.finalise_staking(&mut events);
    // A third and two thirds of 900_000_002 floor to 300_000_000 and 600_000_001; the last unit
    // stays with the house.
    assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(300_000_000));
    assert_eq!(engine.distribution(AccountId([2; 20])), UsdcUnits(600_000_001));
    assert_eq!(engine.house_cash(), Fixed::from_int(100) + Fixed::from_raw(1_999));
}

#[test]
fn distribution_keeps_the_floor_as_house_liquidity() {
    let mut engine = engine(1_000);
    engine.init_staking(&config(1_000)).unwrap();
    // A client deep underwater on an open position: NAV counts the unrealised loss as house
    // gain, far above the house cash the vault pays from.
    let spec: crate::market::MarketSpec = serde_json::from_value(serde_json::json!({
        "id": 1, "symbol": "BIG", "asset_class": "crypto", "schedule": { "kind": "always_open" },
        "margin_tiers": [{ "bps": 1_000 }], "spread_tiers": [{ "bps": 0 }],
        "price_rules": { "max_age_ms": 1_000, "max_divergence_bps": 50, "min_sources": 2 },
        "max_slippage_bps": 20, "house_cap_bps": 5_000,
        "financing": { "benchmark": "SOFR", "markup_bps": 0, "day_basis": 365 },
    }))
    .expect("a valid listing");
    let mut market = crate::engine::MarketState::new(spec);
    market.mark = Some(Fixed::from_int(1));
    market.client_qty = Fixed::ONE;
    market.client_cost = Fixed::from_int(10_000);
    engine.markets.insert(1, market);
    engine.house_cash = Fixed::from_int(1_000);
    engine.staked.insert(AccountId([1; 20]), TokenUnits(1));
    assert!(engine.house_nav() > engine.house_cash + Fixed::from_int(9_000));

    let mut events = Vec::new();
    engine.now = config(1_000).epoch_end(0);
    engine.finalise_staking(&mut events);
    // Only the excess house cash actually holds above the floor pays: 1,000 − 100, even
    // though NAV is far higher.
    assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(900_000_000));
    assert_eq!(engine.house_cash, Fixed::from_int(100));
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { excess, allocated, .. }
            if *excess == UsdcUnits(900_000_000) && *allocated == UsdcUnits(900_000_000))));
}

#[test]
fn unfinalised_house_principal_stays_locked_from_distributions() {
    let mut engine = engine(1_000);
    engine.init_staking(&config(1_000)).unwrap();
    // 900 of the house cash is principal credited at sequencer confirmation, still waiting
    // for Ethereum finality: `house_capital` refuses to move it, and so does an epoch close.
    engine.house_cash = Fixed::from_int(1_000);
    engine.unfinalised_house_deposits = Fixed::from_int(900);
    engine.staked.insert(AccountId([1; 20]), TokenUnits(1));
    let mut events = Vec::new();
    engine.now = config(1_000).epoch_end(0);
    engine.finalise_staking(&mut events);
    assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(0));
    assert_eq!(engine.house_cash, Fixed::from_int(1_000));
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { excess, allocated, .. }
            if *excess == UsdcUnits(0) && *allocated == UsdcUnits(0))));
}

#[test]
fn a_stake_sum_past_u128_is_refused_without_mutation() {
    let mut engine = engine(1_000);
    engine.init_staking(&config(1_000)).unwrap();
    let mut events = Vec::new();
    let account = AccountId([1; 20]);
    engine.stake(account, TokenUnits(u128::MAX - 1), "0xmax:0", &mut events);
    engine.stake(account, TokenUnits(2), "0xmax:1", &mut events);
    assert!(matches!(events.last(), Some(Event::StakingRejected { reason: Reject::InvalidAmount, .. })));
    assert_eq!(engine.staked(account), TokenUnits(u128::MAX - 1));
    assert!(!engine.is_credited("0xmax:1"));
}

#[test]
fn gains_on_unfinalised_trader_deposits_stay_undistributed() {
    let mut engine = engine(1_000);
    engine.init_staking(&config(1_000)).unwrap();
    engine.house_cash = Fixed::from_int(1_000);
    engine.staked.insert(AccountId([1; 20]), TokenUnits(1));
    // A trader holds an unfinalised deposit whose principal can still reverse; its realised gains
    // in house cash are not distributable until the deposit finalises.
    let account = crate::account::Account {
        balance: Fixed::from_int(500),
        unfinalised_deposits: Fixed::from_int(900),
        ..Default::default()
    };
    engine.accounts.insert(AccountId([2; 20]), account);
    let mut events = Vec::new();
    engine.now = config(1_000).epoch_end(0);
    engine.finalise_staking(&mut events);
    assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(0));
    assert_eq!(engine.house_cash, Fixed::from_int(1_000));
    assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { excess, allocated, .. }
        if *excess == UsdcUnits(0) && *allocated == UsdcUnits(0))));
}
