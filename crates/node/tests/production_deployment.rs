//! The launch deployment's inputs, `ops/runbooks/production-deployment.toml`, hold the owner's decision
//! of 2026-11-14 on #436 for the contracts' constructors and the operator's two initialisations, the
//! contracts would accept them, and the deploy runbook's commands carry them.

use std::fs;

use chrono::{DateTime, Timelike, Utc};
use engine_core::Fixed;
use engine_core::emissions::{MAX_PERIOD_BUDGET, PERIODS_PER_HALVING, TokenUnits};
use serde::Deserialize;

const INPUTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ops/runbooks/production-deployment.toml");
const RUNBOOK: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ops/runbooks/deploy-and-rollback.md");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inputs {
    start: String,
    timelock: Timelock,
    vault: Vault,
    staking: Staking,
    init_staking: InitStaking,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Timelock {
    min_delay: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Vault {
    period_budget: TokenUnits,
    exit_period: u64,
    claim_window: u64,
    stale_after: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Staking {
    epoch: u64,
    cooldown: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InitStaking {
    liquidity_floor: Fixed,
}

const DAY: u64 = 86_400;

fn text() -> String {
    fs::read_to_string(INPUTS).unwrap()
}

fn inputs() -> Inputs {
    toml::from_str(&text()).unwrap()
}

#[test]
fn every_value_is_the_owners_decision_on_436() {
    let Inputs { timelock, vault, staking, init_staking, .. } = inputs();
    // The initial emission period budget: 100,000 MONKEY a ten-minute period, in 18-decimal base units.
    assert_eq!(vault.period_budget, TokenUnits(100_000 * 10u128.pow(18)));
    assert_eq!(vault.period_budget, TokenUnits(10u128.pow(23)));
    assert_eq!(vault.claim_window, 30 * DAY);
    assert_eq!(vault.stale_after, DAY);
    // The timelock delay and the exit period stand as #360 set them.
    assert_eq!((timelock.min_delay, vault.exit_period), (3 * DAY, 14 * DAY));
    assert_eq!((staking.epoch, staking.cooldown), (DAY, 28 * DAY));
    assert_eq!(init_staking.liquidity_floor, Fixed::from_int(30_500_000));
}

#[test]
fn the_staking_cooldown_covers_at_least_one_epoch() {
    let Staking { epoch, cooldown } = inputs().staking;
    assert!(epoch > 0 && cooldown >= epoch, "Staking's constructor refuses a cooldown below the epoch");
}

#[test]
fn the_contracts_accept_the_values() {
    let Inputs { timelock, vault, staking, .. } = inputs();
    // `Vault`'s constructor: a nonzero timelock delay, an exit period past it, a nonzero claim window
    // and staleness, and staleness within the exit period.
    assert!(timelock.min_delay > 0 && vault.exit_period > timelock.min_delay);
    assert!(vault.claim_window > 0 && vault.stale_after > 0 && vault.stale_after <= vault.exit_period);
    // The budget the engine accepts, whose token cap `2 x PERIODS_PER_HALVING x budget` fits a u128 and
    // gives the budget back, as the node's check reads it from the vault.
    assert!((1..=MAX_PERIOD_BUDGET).contains(&vault.period_budget.0));
    let cap = vault.period_budget.0 * 2 * u128::from(PERIODS_PER_HALVING);
    assert_eq!(cap / (2 * u128::from(PERIODS_PER_HALVING)), vault.period_budget.0);
    assert_eq!(cap, 5_256_000_000 * 10u128.pow(18), "the decided maximum supply");
    // `Staking`'s two values fit the engine's: whole milliseconds the engine takes as epoch and cooldown.
    assert!(staking.epoch.checked_mul(1000).is_some() && staking.cooldown.checked_mul(1000).is_some());
}

#[test]
fn both_initialisations_start_at_midnight_utc_on_the_opening_day_the_owner_names() {
    let text = text();
    let line = text.lines().find(|line| line.starts_with("start = ")).expect("a start");
    assert!(line.contains("# OWNER"), "the opening day is the owner's to set, so its placeholder is marked: {line}");
    let start: DateTime<Utc> = inputs().start.parse().expect("RFC 3339");
    assert_eq!((start.hour(), start.minute(), start.second(), start.nanosecond()), (0, 0, 0, 0));
}

#[test]
fn the_runbooks_commands_carry_the_inputs() {
    let Inputs { start, vault, staking, init_staking, .. } = inputs();
    let runbook = fs::read_to_string(RUNBOOK).unwrap();
    let admin = "$ENGINE admin --config node.toml --node $API --pin $PIN";
    let emissions = format!("{admin} init-emissions --start $START --period-budget {}", vault.period_budget);
    let staking_command = format!(
        "{admin} init-staking --start $START --liquidity-floor {} --epoch-ms {} --cooldown-ms {} --secrets-file secrets.toml",
        init_staking.liquidity_floor,
        staking.epoch * 1000,
        staking.cooldown * 1000
    );
    for command in [&emissions, &staking_command] {
        assert!(runbook.lines().any(|line| line.trim() == command), "the runbook lacks `{command}`");
    }
    assert!(runbook.contains(&format!("START={start} ")), "the runbook's $START is the inputs' start");
}
