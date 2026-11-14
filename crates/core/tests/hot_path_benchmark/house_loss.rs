//! The house loss trigger's cost from `V59` at 100k accounts, against `V58` running the same
//! commands: a price in BTC and a fill, with every launch market listed and holding a position, a
//! loss threshold at level 2 and a full day of slots in the window, and the state root after them.
//! The loss is read from house NAV, which the stress check already sums over the markets, so it adds
//! no scan of the accounts.

use super::common::close_spread::trade;
use super::toxicity::scored_fixture;
use super::*;
use engine_core::Fixed;
use engine_core::event::Event;
use engine_core::house_loss::{SLOT_MS, WINDOW_MS};

/// A `V59` command's p99 may be twice `V58`'s and this much more, room for a noisy runner: the slot's
/// record and a search of at most 98 slots in each market's stress check take a few microseconds, and a
/// scan of the 100k accounts would take milliseconds.
const ADDED_BUDGET_US: u128 = 200;

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn house_loss_latency() {
    let mut p99s = Vec::new();
    for version in [ProtocolVersion::V58, ProtocolVersion::V59] {
        let mut h = fixture(version);
        let client = Client::new(1);
        let (mut prices, mut fills) = (Vec::with_capacity(SAMPLES), Vec::with_capacity(2 * SAMPLES));
        for sample in 0..SAMPLES {
            h.advance(1);
            let mid = if sample % 2 == 0 { "100010" } else { "100000" };
            let began = Instant::now();
            h.price(BTC, mid);
            prices.push(began.elapsed().as_micros());
            for side in [Side::Buy, Side::Sell] {
                let began = Instant::now();
                let events = trade(&mut h, &client, BTC, side, "0.01", mid);
                fills.push(began.elapsed().as_micros());
                assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{sample}: {events:?}");
            }
        }
        let began = Instant::now();
        h.engine.state_root();
        let root = began.elapsed().as_micros();
        let mut row = Vec::new();
        for (name, mut times) in [("price", prices), ("fill", fills)] {
            times.sort_unstable();
            let (p50, p99) = (times[times.len() / 2 - 1], times[times.len() * 99 / 100 - 1]);
            println!("LATENCY house_loss_{name}_{version:?} accounts=100000 p50_us={p50} p99_us={p99}");
            row.push(p99);
        }
        println!("LATENCY house_loss_root_{version:?} accounts=100000 us={root}");
        if version >= ProtocolVersion::V59 {
            let slots = h.engine.house_loss().expect("V59 reads the loss");
            assert!(slots.loss.is_negative() && slots.level == 0, "the fixture's loss stays under zero: {slots:?}");
        }
        p99s.push(row);
    }
    for (index, name) in ["price", "fill"].iter().enumerate() {
        let (before, after) = (p99s[0][index], p99s[1][index]);
        assert!(after <= 2 * before + ADDED_BUDGET_US, "a V59 {name}'s p99 {after}us against V58's {before}us");
    }
}

/// `scored_fixture` at `version` for 100k accounts, each holding 0.01 BTC, with a third stress level
/// and, from `V59`, a loss threshold of zero on it and a window holding a slot for every 15 minutes of
/// the day before, each a million below the house's result now, so the trigger holds off.
fn fixture(version: ProtocolVersion) -> Harness {
    let (mut h, _, keys) = scored_fixture(version, 100_000, None);
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    for key in &keys {
        state["accounts"][key]["positions"] = json!({BTC.to_string(): {"qty": "0.01", "entry": "100000"}});
        state["accounts"][key]["opened_at"] = json!({BTC.to_string(): h.ts});
    }
    let held = f("0.01").mul(f(&keys.len().to_string()));
    let market = &mut state["markets"][BTC.to_string()];
    market["client_qty"] = json!(f(market["client_qty"].as_str().unwrap_or("0")) + held);
    let cost = f(market["client_cost"].as_str().unwrap_or("0")) + held.mul(f("100000"));
    market["client_cost"] = json!(cost);
    let levels = &mut state["params"]["stress_levels"];
    let mut top = levels[1].clone();
    top["min_spread_bps"] = json!(6);
    top["trigger"] = if version >= ProtocolVersion::V59 { json!({"house_loss": "0"}) } else { json!(null) };
    levels.as_array_mut().expect("levels").push(top);
    h.engine = serde_json::from_value(state).expect("benchmark fixture");
    h.engine.restore_indexes();
    if version >= ProtocolVersion::V59 {
        let result = h.engine.house_nav() - Fixed::from_int(1_000_000);
        let now = h.ts.div_euclid(SLOT_MS);
        let slots: Vec<_> = (now - WINDOW_MS / SLOT_MS - 1..now)
            .map(|slot| json!({"slot": slot, "result": result, "high": result}))
            .collect();
        let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
        state["house_loss"] = json!({"slots": slots});
        h.engine = serde_json::from_value(state).expect("benchmark fixture");
        h.engine.restore_indexes();
    }
    h.engine.state_root();
    h
}
