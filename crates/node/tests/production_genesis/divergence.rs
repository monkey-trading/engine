//! Every launch market reaches each divergence stress level as its two sources move apart.

use std::collections::BTreeSet;
use std::path::Path;

use engine_core::Engine;
use engine_core::Fixed;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, Sequenced};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::params::ProtocolParams;
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_node::NodeConfig;

use crate::access;
use crate::{DEV, production_genesis};

#[test]
fn every_launch_market_can_reach_each_divergence_level() {
    let dev = NodeConfig::load(Path::new(DEV)).unwrap().genesis;
    let production = production_genesis("divergence-levels");
    for genesis in [&dev, &production] {
        let levels: BTreeSet<usize> = (0..genesis.params.stress_levels.len())
            .filter(|level| {
                let trigger = genesis.params.stress_levels[*level].trigger.as_ref();
                trigger.is_some_and(|t| t.divergence_share_bps.is_some())
            })
            .collect();
        assert_eq!(levels, BTreeSet::from([1, 2]));
        for (symbol, reached) in divergence_levels(&genesis.params, &genesis.markets) {
            assert!(levels.is_subset(&reached), "{symbol}: reaches {reached:?}");
        }
    }
}

// ── Helpers ──

/// The stress levels each of `markets` reaches under the latest protocol version and `params`, all
/// listed 24/7 with house capital and no positions, as two venues diverge around a flat index of 1
/// from zero up to the market's bound, in listing order, so each FX market is priced before the
/// markets that convert at it.
fn divergence_levels(params: &ProtocolParams, markets: &[MarketSpec]) -> Vec<(String, BTreeSet<usize>)> {
    let mut engine = Engine::new();
    let operator = NodeConfig::load(Path::new(DEV)).unwrap().operator().unwrap();
    let activation = [
        Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)),
        Command::Admin(AdminCommand::SetOperator(operator)),
        Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST }),
        Command::Admin(AdminCommand::SetAccessPolicy(access::policy())),
        Command::Admin(AdminCommand::SetParams(params.clone())),
    ];
    let listings = markets.iter().map(|market| {
        Command::Admin(AdminCommand::UpsertMarket(MarketSpec { schedule: Schedule::AlwaysOpen, ..market.clone() }))
    });
    activation.into_iter().chain(listings).for_each(|cmd| apply_at_zero(&mut engine, cmd));
    // House funding is screened like any deposit: clear evidence from the funding payer credits it.
    let capital = access::funding(&engine, None, Some(access::account(1)), Fixed::from_int(10_000_000), "capital");
    apply_at_zero(&mut engine, capital);
    assert_eq!(engine.house_cash(), Fixed::from_int(10_000_000));
    // An index of 1 converts every launch currency, so a market quoted in one has a price.
    let center = Fixed::ONE;
    markets
        .iter()
        .map(|market| {
            // A market priced from futures takes quotes for its first contract at time zero.
            let contract = market.contracts.first().map(|c| c.contract.clone());
            let quote =
                |source: &str, mid| SourceQuote { source: source.into(), mid, ts: 0, contract: contract.clone() };
            let reached = (0..=market.price_rules.max_divergence_bps)
                .map(|bps| {
                    let half = Fixed::from_bps(bps).div_int(2);
                    let quotes = vec![quote("a", center - half), quote("b", center + half)];
                    apply_at_zero(&mut engine, Command::Prices { market: market.id, quotes });
                    engine.markets()[&market.id].stress.level
                })
                .collect();
            (market.symbol.clone(), reached)
        })
        .collect()
}

/// Applies `cmd` as the engine's next seq at time zero; panics if the engine refuses it.
fn apply_at_zero(engine: &Engine, cmd: Command) {
    let seq = engine.last_seq().map_or(0, |seq| seq + 1);
    let events = engine.apply(&Sequenced { seq, ts: 0, cmd }).unwrap();
    let refused = |e: &Event| matches!(e, Event::AdminRejected { .. } | Event::PricesRejected { .. });
    assert!(!events.iter().any(refused), "{events:?}");
}
