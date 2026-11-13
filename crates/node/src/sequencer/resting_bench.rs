//! What resting orders add to a price at 100k accounts (#700). A price visits every resting order
//! its mid has reached (`hot_indexes`, `triggered`), and a limit the spread keeps out of reach stays
//! resting, so the same orders are visited, and priced, on every price. Each case loads that many
//! such limits into market 1, tier 0's 25 an account, and times the price command's stamp, which
//! every later price waits behind (`wait_bench::price_wait_budget` holds a batch's median wait to
//! 1 ms). The last case is every account at tier 0's limit; a second test holds that many limits
//! away from the price, which no price visits. Run in a quiet CPU window:
//! `cargo test -p engine-node --lib sequencer::resting_bench -- --ignored --nocapture --test-threads=1`.

use engine_core::Fixed;
use engine_core::command::{Command, OrderKind, OrderRequest};
use engine_core::event::Event;
use engine_core::types::Side;

use super::wait_bench::{ACCOUNTS, Rig, account, report, time};
use crate::test_support::dev_config;

/// Tier 0's resting-order limit (#614, decision D11), and tier 3's, the most any account holds.
const PER_ACCOUNT: u32 = 25;
const MOST: usize = 250;
/// Out-of-reach limits in market 1, per case.
const LOADS: [u32; 6] = [0, 1_000, 10_000, 100_000, 1_000_000, PER_ACCOUNT * ACCOUNTS];

/// A limit buy at 100,005: every price (`Rig::price`, 100,000 or 100,001) reaches it, and market 1's
/// 2 bps first band (the development genesis's BTC-USD, #688) puts a fill at 100,020 or more, past
/// the limit, so it stays resting.
const OUT_OF_REACH: i64 = 100_005;
/// A limit buy at 90,000, below every price, which no price reaches.
const AWAY: i64 = 90_000;

/// The `index`th account's limit buy of 0.001 at `price` in market 1.
fn resting(index: u32, client_id: u64, price: i64) -> Command {
    Command::PlaceOrder(OrderRequest {
        account: account(index),
        client_id,
        market: 1,
        side: Side::Buy,
        qty: "0.001".parse().unwrap(),
        kind: OrderKind::Limit { price: Fixed::from_int(price) },
        reduce_only: false,
        auth: None,
    })
}

/// A 100k-account rig on the development genesis that admits `MOST` resting orders an account.
fn rig(name: &str) -> Rig {
    let mut genesis = dev_config().genesis;
    genesis.params.max_resting_orders = MOST;
    Rig::with_genesis(name, &genesis)
}

/// Places resting limits at `price` until `placed` reaches `load`, `PER_ACCOUNT` an account.
fn place(rig: &mut Rig, placed: &mut u32, load: u32, price: i64) {
    while *placed < load {
        let events = rig.seq.sequence(resting(*placed / PER_ACCOUNT, u64::from(*placed), price)).unwrap().unwrap();
        assert!(events.iter().any(|e| matches!(e, Event::OrderAccepted { .. })), "order {placed} rests: {events:?}");
        *placed += 1;
        if placed.is_multiple_of(1024) {
            rig.seq.flush().unwrap();
        }
    }
    rig.seq.flush().unwrap();
}

/// Times `samples` prices from `step` on, each leaving every resting order resting.
fn stamps(rig: &mut Rig, step: &mut u32, samples: usize) -> Vec<u128> {
    let mut out = Vec::with_capacity(samples);
    for _ in 0..samples {
        let cmd = rig.price(*step);
        *step += 1;
        let (us, events) = time(|| rig.seq.sequence(cmd).unwrap().unwrap());
        assert!(
            !events.iter().any(|e| matches!(e, Event::Fill(_) | Event::OrderCancelled { .. })),
            "every order stays resting"
        );
        out.push(us);
        rig.seq.flush().unwrap();
    }
    out
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn resting_orders_price_stamp() {
    let mut rig = rig("resting-orders");
    let (mut placed, mut step) = (0u32, 0u32);
    for load in LOADS {
        place(&mut rig, &mut placed, load, OUT_OF_REACH);
        let samples = if load >= 1_000_000 { 20 } else { 200 };
        report(&format!("price_stamp_rechecking_{load}_resting_orders"), stamps(&mut rig, &mut step, samples));
    }
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn resting_orders_away_from_the_price() {
    let mut rig = rig("resting-orders-away");
    let (mut placed, mut step) = (0u32, 0u32);
    let load = PER_ACCOUNT * ACCOUNTS;
    place(&mut rig, &mut placed, load, AWAY);
    report(&format!("price_stamp_with_{load}_resting_orders_away"), stamps(&mut rig, &mut step, 200));
}
