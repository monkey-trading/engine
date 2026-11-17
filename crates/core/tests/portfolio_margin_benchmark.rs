//! What the portfolio requirement costs over 100k holders of BTC-USD against the per-position
//! requirement (`Engine::equity_and_margin`): each account holds a two-market group, long BTC-USD and
//! short ETH-USD, and an ungrouped AAPL position. Fixture construction is outside the timed passes.

mod common;

use std::hint::black_box;
use std::time::Instant;

use common::close_spread::trade;
use common::events::only_fill;
use common::weekend::{client, genesis_with_tiers, globex};
use common::{AAPL, BTC, ETH, f};
use engine_core::account::Account;
use engine_core::portfolio::{GroupMember, OffsetGroup, PortfolioGrid};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, MarketId, Side};
use engine_core::volume_tier::launch_volume_tiers;
use engine_core::{Engine, Fixed};

const HOLDERS: usize = 100_000;
const PASSES: usize = 30;

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn portfolio_requirement_over_100k_holders() {
    let (engine, grid) = (fixture(), grid());
    let accounts: Vec<&Account> = engine.accounts().values().collect();
    assert_eq!(accounts.len(), HOLDERS);
    let one = accounts[0];
    // The group credits its matched exposure, so the timed passes run the whole credit, not an early return.
    assert!(engine.portfolio_margin(one, &grid) < engine.equity_and_margin(one, None).1);
    let pass = |requirement: &dyn Fn(&Account) -> Fixed| {
        let began = Instant::now();
        for acct in &accounts {
            black_box(requirement(black_box(acct)));
        }
        began.elapsed().as_micros()
    };
    let (mut per_position, mut portfolio) = (Vec::with_capacity(PASSES), Vec::with_capacity(PASSES));
    for _ in 0..PASSES {
        per_position.push(pass(&|acct| engine.equity_and_margin(acct, None).1));
        portfolio.push(pass(&|acct| engine.portfolio_margin(acct, &grid)));
    }
    for (name, mut times) in [("equity_and_margin", per_position), ("portfolio_margin", portfolio)] {
        times.sort_unstable();
        let (p50, max) = (times[PASSES / 2], times[PASSES - 1]);
        let per_holder = p50 * 1_000 / HOLDERS as u128;
        println!(
            "LATENCY {name} holders={HOLDERS} passes={PASSES} p50_us={p50} max_us={max} per_holder_ns={per_holder}"
        );
    }
}

/// `HOLDERS` copies of one account holding $100k of BTC long, $100k of ETH short and $20k of AAPL
/// long, at the latest version with the ceiling registry installed.
fn fixture() -> Engine {
    let mut h = genesis_with_tiers(ProtocolVersion::LATEST, globex(vec![]), 0, 0, launch_volume_tiers());
    let a = client(&mut h, 1, "1000000");
    for (market, price) in [(BTC, "100000"), (ETH, "2000"), (AAPL, "200")] {
        h.price(market, price);
    }
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "1", "100000"));
    only_fill(&trade(&mut h, &a, ETH, Side::Sell, "50", "2000"));
    only_fill(&trade(&mut h, &a, AAPL, Side::Buy, "100", "200"));
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    let template = state["accounts"][serde_json::to_value(a.account).unwrap().as_str().unwrap()].clone();
    let accounts: serde_json::Map<_, _> = (0..HOLDERS)
        .map(|index| {
            let mut address = [0; 20];
            address[12..].copy_from_slice(&(index as u64 + 1).to_be_bytes());
            let key = serde_json::to_value(AccountId(address)).unwrap().as_str().unwrap().to_owned();
            (key, template.clone())
        })
        .collect();
    state["accounts"] = accounts.into();
    serde_json::from_value(state).expect("benchmark fixture")
}

/// BTC and ETH as one group, as the dev config's fixture has them.
fn grid() -> PortfolioGrid {
    let member = |market: MarketId, loading: &str| GroupMember {
        market,
        loading: f(loading),
        residual_shock_bps: 20,
        weekend_residual_shock_bps: 20,
    };
    let crypto = OffsetGroup {
        name: "crypto".into(),
        floor_share_bps: 5_000,
        members: vec![member(BTC, "1"), member(ETH, "1.25")],
    };
    let grid = PortfolioGrid { groups: vec![crypto] };
    assert_eq!(grid.validate(), Ok(()));
    grid
}
