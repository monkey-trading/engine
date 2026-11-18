//! `config/production-genesis.toml` as the node sequences it, on a harness at the latest protocol
//! version, with the market hours and futures contracts a price at launch reads.

use engine_core::Fixed;
use engine_core::auth::ClientAuth;
use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate};
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::params::ProtocolParams;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::staking::StakingConfig;
use engine_core::types::{MarketId, Millis};
use serde::Deserialize;

use crate::common::launch_book::mix::Origin;
use crate::common::{CHAIN_ID, Harness, VAULT, operator};

const PRODUCTION_GENESIS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/production-genesis.toml");
const DEPLOYMENT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ops/runbooks/production-deployment.toml");

/// Mon 16 Nov 2026, 15:00 UTC: the US shares, CME's futures, Cboe FX and Osaka's night session all
/// trade, and four daily rollovers (22:00 UTC) fall before the weekend's gap.
pub const BOOK_TIME: Millis = 1_794_841_200_000;

/// The `[genesis]` section of a node config, as far as the engine reads it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    genesis: Genesis,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Genesis {
    house_capital: Fixed,
    params: ProtocolParams,
    markets: Vec<MarketSpec>,
    close_spread_ceilings: Vec<CloseSpreadCeiling>,
}

/// The launch deployment's one-time inputs the node's two initialisations carry
/// (`ops/runbooks/production-deployment.toml`).
#[derive(Deserialize)]
struct Deployment {
    start: chrono::DateTime<chrono::Utc>,
    vault: Vault,
    staking: Staking,
    init_staking: InitStaking,
}

#[derive(Deserialize)]
struct Vault {
    period_budget: String,
}

#[derive(Deserialize)]
struct Staking {
    epoch: i64,
    cooldown: i64,
}

#[derive(Deserialize)]
struct InitStaking {
    liquidity_floor: Fixed,
}

fn load() -> File {
    let text = std::fs::read_to_string(PRODUCTION_GENESIS).expect("config/production-genesis.toml");
    toml::from_str(&text).expect("the production genesis parses as the engine's types")
}

/// An engine at `ProtocolVersion::LATEST` with the production genesis sequenced as the node does:
/// signed requests and the harness operator recorded, then the version activated, then the genesis's
/// parameters, markets and close-spread ceilings and its zero house capital, then the operator's two
/// initialisations at the launch deployment's values, whose time origin is still ahead. The clock is
/// at `BOOK_TIME`.
pub fn production_harness() -> Harness {
    production_harness_at(ProtocolVersion::LATEST, None, BOOK_TIME, false)
}

/// `production_harness` at `version`, with emissions' period 0 and staking's epoch 0 at `origin` (its
/// start after the book's clock and its epoch length) in place of the deployment's, the clock at `at`,
/// and with `always_open` every market listed as trading around the clock, with no weekend margin.
pub fn production_harness_at(
    version: ProtocolVersion,
    origin: Option<Origin>,
    at: Millis,
    always_open: bool,
) -> Harness {
    let genesis = load().genesis;
    let mut h = Harness::empty();
    h.ts = at;
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
    h.admin(AdminCommand::SetOperator(operator()));
    h.admin(AdminCommand::ActivateProtocol { version });
    h.admin(AdminCommand::SetParams(genesis.params));
    for market in genesis.markets {
        // A market open around the clock has no weekend gap to hold a margin over.
        let market = match always_open {
            true => MarketSpec { schedule: Schedule::AlwaysOpen, weekend_margin_bps: 0, ..market },
            false => market,
        };
        h.admin(AdminCommand::UpsertMarket(market));
    }
    h.admin(AdminCommand::SetCloseSpreadCeilings { ceilings: genesis.close_spread_ceilings });
    let events = h.cmd(Command::HouseCapital { amount: genesis.house_capital, chain_ref: "genesis".into() });
    let text = std::fs::read_to_string(DEPLOYMENT).expect("ops/runbooks/production-deployment.toml");
    let deployment: Deployment = toml::from_str(&text).expect("the deployment's inputs parse");
    let start_ms = origin.map_or(deployment.start.timestamp_millis(), |origin| at + origin.start_after);
    let period_budget = deployment.vault.period_budget.parse().expect("a budget in base units");
    h.operator_admin(AdminCommand::InitEmissions(EmissionSchedule {
        start_ms,
        period_budget: TokenUnits(period_budget),
    }));
    h.operator_admin(AdminCommand::InitStaking(StakingConfig {
        liquidity_floor: deployment.init_staking.liquidity_floor,
        epoch_ms: origin.map_or(deployment.staking.epoch * 1_000, |origin| origin.epoch_ms),
        start_ms,
        cooldown_ms: deployment.staking.cooldown * 1_000,
    }));
    assert!(!events.is_empty() || genesis.house_capital.is_zero());
    h
}

/// The batch of a 4.3% rate, effective today, for each benchmark the listings name, so a rollover
/// charges financing.
pub fn benchmark_batch(h: &Harness) -> AdminCommand {
    let today = h.engine.benchmark_date(h.ts);
    let names: std::collections::BTreeSet<String> =
        h.engine.markets().values().map(|m| m.spec.financing.benchmark.clone()).collect();
    let rates = names
        .iter()
        .map(|benchmark| BenchmarkRate {
            benchmark: benchmark.clone(),
            effective_date: today,
            annual_rate: "0.043".parse().unwrap(),
        })
        .collect();
    AdminCommand::SetBenchmarkRates(BenchmarkBatch { as_of: today, checked: names, rates })
}

/// Publishes `benchmark_batch`.
pub fn publish_benchmarks(h: &mut Harness) {
    let events = h.operate(benchmark_batch(h));
    assert!(events.contains(&Event::AdminApplied), "benchmark publication: {events:?}");
}

/// The command quoting `market` from two venues at `mid`, stamped `ts` and naming the front contract
/// of a market priced from futures.
pub fn price_command(h: &Harness, market: MarketId, mid: &str, ts: Millis) -> Command {
    let contract = h.engine.markets()[&market].spec.contracts.iter().rfind(|c| c.from.timestamp_millis() <= ts);
    let contract = contract.map(|c| c.contract.clone());
    let quotes = (0..2)
        .map(|venue| SourceQuote {
            source: format!("venue{venue}"),
            mid: mid.parse().unwrap(),
            ts,
            contract: contract.clone(),
        })
        .collect();
    Command::Prices { market, quotes }
}

/// Quotes `market` at `mid` now.
pub fn price_market(h: &mut Harness, market: MarketId, mid: &str) -> Vec<Event> {
    let cmd = price_command(h, market, mid, h.ts);
    h.cmd(cmd)
}
