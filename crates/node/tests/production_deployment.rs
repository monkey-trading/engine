//! The launch deployment's inputs, `ops/runbooks/production-deployment.toml`, hold the owner's decision
//! of 2026-11-14 on #436 for the contracts' constructors and the operator's two initialisations, and of
//! 2026-11-21 on #1175 for the operator schedule's heartbeat rule, the contracts would accept them, and
//! the deploy runbook's commands carry them.

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
    schedule: Schedule,
    parameter_vote: ParameterVote,
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

/// `OperatorSchedule`'s constructor values.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    switch_budget: u64,
    block_millis: u64,
    liveness: Liveness,
    liveness_minimums: Liveness,
    liveness_maximums: Liveness,
    take_window: TakeWindow,
}

/// `OperatorSchedule.Liveness`, in seconds but for `head_blocks`.
#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct Liveness {
    heartbeat_interval: u64,
    head_tolerance: u64,
    heartbeat_misses: u64,
    head_blocks: u64,
}

/// `OperatorSchedule.TakeWindow`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TakeWindow {
    slots: u64,
    minimum: u64,
    maximum: u64,
}

/// `ParameterVote`'s windows, in seconds.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParameterVote {
    normal_window: u64,
    fast_window: u64,
}

const DAY: u64 = 86_400;
/// `OperatorSchedule`'s longest detection, `(heartbeatMisses + 1) x heartbeatInterval + headTolerance`, the
/// most blocks `arbBlockHash` reaches, and the time before a promotion at which the operator's node stops
/// signing (`STOP_MARGIN`).
const MAX_DETECTION: u64 = 600;
const MAX_HEAD_BLOCKS: u64 = 256;
const STOP_MARGIN: u64 = 15;
/// `ParameterVote.MAX_WINDOW`.
const MAX_VOTE_WINDOW: u64 = 30 * DAY;

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

#[test]
fn the_heartbeat_rule_is_the_owners_decision_of_2026_11_21() {
    let Schedule { liveness, take_window, .. } = inputs().schedule;
    let Liveness { heartbeat_interval, head_tolerance, heartbeat_misses, .. } = liveness;
    assert_eq!((heartbeat_interval, head_tolerance, heartbeat_misses, take_window.slots), (140, 40, 3, 2));
    // A promotion passes `heartbeatMisses` to `heartbeatMisses + 1` intervals after the last head's slot:
    // 7 to 9.3 minutes, and at most ten with the tolerance.
    assert_eq!((heartbeat_misses * heartbeat_interval, (heartbeat_misses + 1) * heartbeat_interval), (420, 560));
}

#[test]
fn the_schedule_accepts_the_values() {
    let Schedule { switch_budget, block_millis, liveness, liveness_minimums: lo, liveness_maximums: hi, take_window } =
        inputs().schedule;
    // `_checkLiveness`, on the rule in force: within the bounds, detection within ten minutes, the
    // tolerance under the interval, the held-head window within `arbBlockHash`'s reach and under one
    // interval, and the missed slots longer than the switch budget.
    let field =
        |rule: Liveness| [rule.heartbeat_interval, rule.head_tolerance, rule.heartbeat_misses, rule.head_blocks];
    let (rule, lo, hi) = (field(liveness), field(lo), field(hi));
    assert!((0..4).all(|i| lo[i] <= rule[i] && rule[i] <= hi[i]), "{rule:?} within {lo:?} and {hi:?}");
    let [interval, tolerance, misses, blocks] = rule;
    assert!(misses >= 2 && (misses + 1) * interval + tolerance <= MAX_DETECTION);
    assert!(tolerance < interval && blocks <= MAX_HEAD_BLOCKS && blocks * block_millis < interval * 1000);
    assert!(misses * interval > switch_budget);
    // The constructor's take window: a minimum one slot past which the shortest interval exceeds the switch
    // budget and the longest tolerance, the window within its bounds, and the longest window at most a turn
    // at the shortest interval.
    let TakeWindow { slots, minimum, maximum } = take_window;
    assert!(minimum > 0 && minimum <= slots && slots <= maximum);
    assert!((minimum - 1) * lo[0] > switch_budget + hi[1]);
    assert!(maximum * lo[0] <= DAY);
    // `_skipsFirst`: the first skip's member can take the turn before the untaken-turn promotion.
    assert!(misses > slots && (misses - slots) * interval > switch_budget + tolerance.max(STOP_MARGIN));
    // Every rule the bounds allow keeps a held head's block window under one interval at this block time.
    assert!(hi[3] <= MAX_HEAD_BLOCKS && hi[3] * block_millis < lo[0] * 1000);
}

#[test]
fn the_parameter_vote_accepts_its_windows() {
    let ParameterVote { normal_window, fast_window } = inputs().parameter_vote;
    assert!((1..=MAX_VOTE_WINDOW).contains(&normal_window) && (1..=MAX_VOTE_WINDOW).contains(&fast_window));
}

#[test]
fn the_runbooks_schedule_deployment_carries_the_inputs() {
    let Schedule { switch_budget, block_millis, liveness, liveness_minimums, liveness_maximums, take_window } =
        inputs().schedule;
    let tuple = |rule: Liveness| {
        format!(
            "\"({},{},{},{})\"",
            rule.heartbeat_interval, rule.head_tolerance, rule.heartbeat_misses, rule.head_blocks
        )
    };
    let window = format!("\"({},{},{})\"", take_window.slots, take_window.minimum, take_window.maximum);
    let schedule = format!(
        "--constructor-args $REGISTRY $SEED_RECEIVER $LOG_HEADS {} {} {} {window} {switch_budget} {block_millis}",
        tuple(liveness),
        tuple(liveness_minimums),
        tuple(liveness_maximums)
    );
    let ParameterVote { normal_window, fast_window } = inputs().parameter_vote;
    let vote = format!("--constructor-args $SCHEDULE {normal_window} {fast_window}");
    let runbook = fs::read_to_string(RUNBOOK).unwrap();
    for args in [&schedule, &vote] {
        assert!(runbook.lines().any(|line| line.trim().ends_with(args.as_str())), "the runbook lacks `{args}`");
    }
}
