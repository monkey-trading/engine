//! `config/production-genesis.toml` as the node sequences it, on a harness at the latest protocol
//! version, with the market hours and futures contracts a price at launch reads.

use engine_core::Fixed;
use engine_core::auth::ClientAuth;
use engine_core::benchmark::{BenchmarkBatch, BenchmarkRate};
use engine_core::close_spread::CloseSpreadCeiling;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::params::ProtocolParams;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Millis};
use serde::Deserialize;

use crate::common::{CHAIN_ID, Harness, VAULT, operator};

const PRODUCTION_GENESIS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/production-genesis.toml");

/// The `[genesis]` section of a node config, as far as the engine reads it.
#[derive(Deserialize)]
struct File {
    genesis: Genesis,
}

#[derive(Deserialize)]
struct Genesis {
    house_capital: Fixed,
    params: ProtocolParams,
    markets: Vec<MarketSpec>,
    close_spread_ceilings: Vec<CloseSpreadCeiling>,
}

/// The production genesis's markets, in listing order.
pub fn production_markets() -> Vec<MarketSpec> {
    load().genesis.markets
}

fn load() -> File {
    let text = std::fs::read_to_string(PRODUCTION_GENESIS).expect("config/production-genesis.toml");
    toml::from_str(&text).expect("the production genesis parses as the engine's types")
}

/// Mon 16 Nov 2026, 15:00 UTC: the US shares, CME's futures, Cboe FX and Osaka's night session all
/// trade, and four daily rollovers (22:00 UTC) fall before the weekend's gap.
pub const BOOK_TIME: Millis = 1_794_841_200_000;

/// An engine at `ProtocolVersion::LATEST` with the production genesis sequenced as the node does:
/// signed requests and the harness operator recorded, then the version activated, then the genesis's
/// parameters, markets and close-spread ceilings and its zero house capital. The clock is at
/// `BOOK_TIME`.
pub fn production_harness() -> Harness {
    let genesis = load().genesis;
    let mut h = Harness::empty();
    h.ts = BOOK_TIME;
    h.admin(AdminCommand::SetClientAuth(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT }));
    h.admin(AdminCommand::SetOperator(operator()));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST });
    h.admin(AdminCommand::SetParams(genesis.params));
    for market in genesis.markets {
        h.admin(AdminCommand::UpsertMarket(market));
    }
    h.admin(AdminCommand::SetCloseSpreadCeilings { ceilings: genesis.close_spread_ceilings });
    let events = h.cmd(Command::HouseCapital { amount: genesis.house_capital, chain_ref: "genesis".into() });
    assert!(!events.is_empty() || genesis.house_capital.is_zero());
    h
}

/// The batch of a 4% rate, effective today, for each benchmark the listings name, so a rollover
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
            annual_rate: "0.04".parse().unwrap(),
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
