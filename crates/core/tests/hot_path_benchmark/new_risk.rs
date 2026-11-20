//! New risk and withdrawals under portfolio margin from `V84` at 100k accounts (#916), on `portfolio`'s
//! book of 100k holders of a BTC-USD/ETH-USD pair and AAPL at a 50% floor share, between their levels, with
//! one signed client holding the same: its fills each adding to one leg of its pair, which from `V84` value
//! its book under the portfolio requirement; its withdrawals, which read it; and, at `V84` alone, an
//! ordinary order closing its BTC-USD leg alone, refused for the requirement it would raise, beside the
//! cost of that order's preview read on the engine. Each signed command's p99 is printed against
//! `BATCH_BUDGET_US` and its p50 held by `hold_fill`, a `V84` one to twice `V83`'s and `ADDED_P50_US`.

use std::collections::BTreeMap;

use super::common::close_spread::{reduce, trade};
use super::common::portfolio::{genesis_with_grid, pairs};
use super::common::weekend::withdraw;
use super::portfolio::{COUNT, Standing, address, build, crypto};
use super::*;
use engine_core::Fixed;
use engine_core::event::{Event, Reject};

const V83: ProtocolVersion = ProtocolVersion::V83;
const V84: ProtocolVersion = ProtocolVersion::V84;

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn portfolio_new_risk_latency() {
    rerun_once("portfolio_new_risk_latency", portfolio_new_risk_latency_measured);
}

fn portfolio_new_risk_latency_measured() {
    let mut earlier: BTreeMap<&str, u128> = BTreeMap::new();
    for version in [V83, V84] {
        let (mut h, client) = with_client(version, 5_000, f("1000000"));
        let mut fills = Vec::with_capacity(2 * SAMPLES);
        for _ in 0..SAMPLES {
            for (market, side, qty, price) in [(BTC, Side::Buy, "0.01", "100000"), (ETH, Side::Sell, "0.25", "4000")] {
                let began = Instant::now();
                let events = trade(&mut h, &client, market, side, qty, price);
                fills.push(began.elapsed().as_micros());
                assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
            }
        }
        held("portfolio_hedged_fill", version, fills, &mut earlier);
        let mut withdrawals = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let began = Instant::now();
            let events = withdraw(&mut h, &client, "10");
            withdrawals.push(began.elapsed().as_micros());
            assert!(events.iter().any(|e| matches!(e, Event::WithdrawalQueued { .. })), "{events:?}");
        }
        held("portfolio_withdrawal", version, withdrawals, &mut earlier);
    }
    // At a 1% floor share the pair and AAPL need about $7k; on $11k, BTC-USD's close would leave ETH-USD and
    // AAPL at their $14k of bands.
    let (mut h, client) = with_client(V84, 100, f("11000"));
    let mut refusals = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let began = Instant::now();
        let events = reduce(&mut h, &client, BTC, Side::Sell, "1", "100000");
        refusals.push(began.elapsed().as_micros());
        assert!(matches!(events.last(), Some(Event::OrderRejected { reason: Reject::InsufficientMargin { .. }, .. })));
    }
    held("portfolio_refused_lone_close", V84, refusals, &mut earlier);
    let mut previews: Vec<u128> = (0..SAMPLES)
        .map(|_| {
            let began = Instant::now();
            let preview = h.engine.preview_order(client.account, BTC, Side::Sell, f("1"), true);
            let took = began.elapsed().as_micros();
            assert!(preview.is_some_and(|preview| preview.refused.is_some()));
            took
        })
        .collect();
    previews.sort_unstable();
    let (p50, p99) = (previews[SAMPLES / 2 - 1], previews[SAMPLES * 99 / 100 - 1]);
    println!("LATENCY portfolio_order_preview_V84 accounts={COUNT} p50_us={p50} p99_us={p99}");
}

/// Prints `name`'s samples at `version` and holds them by `hold_fill`, against the earlier version's p50 in
/// `earlier` where it has one.
fn held(name: &'static str, version: ProtocolVersion, mut times: Vec<u128>, earlier: &mut BTreeMap<&str, u128>) {
    times.sort_unstable();
    let (p50, p99) = (times[times.len() / 2 - 1], times[times.len() * 99 / 100 - 1]);
    println!("LATENCY {name}_{version:?} accounts={COUNT} p50_us={p50} p99_us={p99}");
    hold_fill(&format!("{name}_{version:?}"), p50, p99, earlier.get(name).copied());
    earlier.insert(name, p50);
}

/// `portfolio`'s book of `COUNT` accounts between their levels at `version` and a floor share of `floor_bps`,
/// with signed client 1 holding the same positions on `equity`, and a harness at the book's time.
fn with_client(version: ProtocolVersion, floor_bps: i64, equity: Fixed) -> (Harness, Client) {
    let spec = crypto(version, floor_bps);
    let (engine, _) = build(&spec, COUNT, Standing::Between);
    let template: AccountId = serde_json::from_value(json!(address(0))).expect("an address");
    let (held, _) = engine.equity_and_margin(&engine.accounts()[&template], None);
    let client = Client::new(1);
    let mut state = serde_json::to_value(&engine).expect("fixture encoding");
    drop(engine);
    let key = serde_json::to_value(client.account).unwrap().as_str().unwrap().to_owned();
    let mut copy = state["accounts"][address(0)].clone();
    let balance: Fixed = serde_json::from_value(copy["balance"].clone()).expect("a balance");
    copy["balance"] = json!(balance + equity - held);
    for (market, ..) in &spec.holdings {
        let held: Position = serde_json::from_value(copy["positions"][market.to_string()].clone()).unwrap();
        let aggregates = &mut state["markets"][market.to_string()];
        let qty: Fixed = serde_json::from_value(aggregates["client_qty"].clone()).unwrap();
        let cost: Fixed = serde_json::from_value(aggregates["client_cost"].clone()).unwrap();
        aggregates["client_qty"] = json!(qty + held.qty);
        aggregates["client_cost"] = json!(cost + held.cost());
    }
    state["accounts"][&key] = copy;
    state["eligibility"][&key] = state["eligibility"][address(0)].clone();
    let mut h = genesis_with_grid(version, pairs(floor_bps), 0, 0);
    h.engine = serde_json::from_value(state).expect("benchmark fixture");
    h.engine.restore_indexes();
    h.ts = h.engine.now();
    // The harness numbers a command by its log's length: stand in for the commands the book was built by.
    let next = usize::try_from(h.engine.last_seq().map_or(0, |seq| seq + 1)).expect("in range");
    let filler = h.log[0].clone();
    h.log.resize(next, filler);
    (h, client)
}
