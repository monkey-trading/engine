//! The launch markets' size spreads: #623's calibrated tiers in tenths of a bp, with #614's D6 first
//! bands on FX and indices, each band twice the one before.

use std::path::Path;

use engine_core::Fixed;
use engine_node::NodeConfig;

use crate::{DEV, production_genesis};

/// Each launch market's first band in tenths of a bp: #623's `results/spread_tiers.toml`, and on FX
/// and indices the tenths D6 derives from its 3s moves, pending the owner's confirmation.
const FIRST_BANDS: [(&str, i64); 34] = [
    ("BTC-USD", 20),
    ("ETH-USD", 30),
    ("SOL-USD", 20),
    ("EUR-USD", 4),
    ("GBP-USD", 4),
    ("USD-JPY", 5),
    ("AUD-USD", 5),
    ("US500", 7),
    ("US100", 9),
    ("US30", 7),
    ("JP225", 10),
    ("XAU-USD", 20),
    ("XAG-USD", 20),
    ("WTI", 20),
    ("AAPL", 20),
    ("MSFT", 20),
    ("NVDA", 30),
    ("AMZN", 20),
    ("GOOGL", 20),
    ("META", 20),
    ("TSLA", 30),
    ("AVGO", 30),
    ("BRK.B", 20),
    ("JPM", 20),
    ("V", 20),
    ("MA", 20),
    ("LLY", 30),
    ("JNJ", 20),
    ("XOM", 20),
    ("WMT", 20),
    ("COST", 20),
    ("NFLX", 20),
    ("AMD", 30),
    ("ORCL", 30),
];

#[test]
fn the_launch_markets_carry_the_calibrated_spread_tiers() {
    let production = production_genesis("spreads");
    let dev = NodeConfig::load(Path::new(DEV)).unwrap().genesis;
    for market in &production.markets {
        let symbol = &market.symbol;
        let (_, first) = FIRST_BANDS.iter().find(|(name, _)| name == symbol).expect("a calibrated market");
        let rates: Vec<i64> = market.spread_tiers.iter().map(|tier| tier.tenth_bps()).collect();
        assert_eq!(rates, [*first, first * 2, first * 4], "{symbol}");
        assert!(market.spread_tiers.iter().all(|tier| !tier.is_whole_bps()), "{symbol}");
        // The bands' bounds are a dev market's of the same asset class, and a market dev lists has
        // dev's bands.
        let bounds = |spec: &engine_core::market::MarketSpec| -> Vec<Option<Fixed>> {
            spec.spread_tiers.iter().map(|tier| tier.up_to).collect()
        };
        match dev.markets.iter().find(|d| d.id == market.id) {
            Some(same) => assert_eq!(market.spread_tiers, same.spread_tiers, "{symbol}"),
            None => assert!(
                dev.markets.iter().any(|d| d.asset_class == market.asset_class && bounds(d) == bounds(market)),
                "{symbol}"
            ),
        }
    }
}
