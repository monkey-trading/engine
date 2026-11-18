//! Limits out of reach at 100k accounts from `V74`, against `V73` running the same prices: each account
//! rests a limit buy of 0.01 BTC at 100,005, which every price at 100,000 or 100,001 passes but BTC's
//! 1bp first band keeps out of reach. `V73` prices every one of them on every price; from `V74` a price
//! visits none, so it holds the resting case's budget (`command_latency`).
//!
//! Measured on 17 November 2026 on a Mac17,9 with 15 logical CPUs and 24 GiB RAM, with other workspace
//! builds running: a price took 48,436 / 51,335 us p50 / p99 under `V73` and 0 / 2 us under `V74`.

use super::common::close_spread::rest;
use super::common::session_keys::funded_at;
use super::*;
use engine_core::event::Event;

const LIMITS: usize = 100_000;

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn out_of_reach_limits_latency() {
    for version in [ProtocolVersion::V73, ProtocolVersion::V74] {
        let mut h = out_of_reach(version);
        // The first price after the restore, outside the samples.
        h.advance(1);
        h.price(BTC, "100000");
        let mut times = Vec::with_capacity(SAMPLES);
        for sample in 0..SAMPLES {
            h.advance(1);
            let mid = if sample % 2 == 0 { "100001" } else { "100000" };
            let began = Instant::now();
            let events = h.price(BTC, mid);
            times.push(began.elapsed().as_micros());
            let left = events.iter().any(|e| matches!(e, Event::Fill(_) | Event::OrderCancelled { .. }));
            assert!(!left, "every limit stays resting: {events:?}");
        }
        times.sort_unstable();
        let (p50, p99) = (times[49], times[98]);
        println!("LATENCY out_of_reach_limits_{version:?} accounts={LIMITS} p50_us={p50} p99_us={p99}");
        assert_eq!(h.engine.markets()[&BTC].resting.len(), LIMITS + 1);
        if version >= ProtocolVersion::V74 {
            assert!(p99 <= 200, "{version:?} p99 {p99}us exceeds 200us");
        }
    }
}

/// `funded_at(version)` with its client resting a limit buy of 0.01 BTC at 100,005 against BTC at
/// 100,000, and `LIMITS` copies of the client, each resting the same limit.
fn out_of_reach(version: ProtocolVersion) -> Harness {
    let (mut h, client, _) = funded_at(version);
    let events = rest(&mut h, &client, BTC, Side::Buy, "0.01", OrderKind::Limit { price: f("100005") });
    assert!(events.iter().any(|e| matches!(e, Event::OrderAccepted { .. })), "{events:?}");
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    let key_of = |account: AccountId| serde_json::to_value(account).unwrap().as_str().unwrap().to_owned();
    let template = state["accounts"][key_of(client.account)].clone();
    let order = template["orders"].as_object().and_then(|orders| orders.values().next().cloned());
    let order = order.expect("the client's limit rests");
    let mut next_order = state["next_order_id"].as_u64().expect("an order id");
    for index in 0..LIMITS as u64 {
        let mut address = [0xcc; 20];
        address[12..].copy_from_slice(&(index + 1).to_be_bytes());
        let key = key_of(AccountId(address));
        let mut copy = template.clone();
        let mut order = order.clone();
        order["order_id"] = json!(next_order);
        copy["orders"] = json!({ next_order.to_string(): order });
        state["markets"][BTC.to_string()]["resting"][next_order.to_string()] = json!(key);
        state["accounts"][&key] = copy;
        next_order += 1;
    }
    state["next_order_id"] = json!(next_order);
    h.engine = serde_json::from_value(state).expect("benchmark fixture");
    h.engine.restore_indexes();
    h
}
