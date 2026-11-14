//! The volume tiers' cost from `V59` at 100k accounts, against `V58` running the same commands: a fill
//! by an account whose volume book is full, and one price triggering 20k stops, each account's book
//! busy. A full book holds the most a book can: 30 days of counted volume, `MAX_OPENING_LOTS` opening
//! lots pending in every launch market and `MAX_CLOSING_LOTS` closing lots within the horizon, so each
//! fill's tier and count read every lot. A busy book holds the 30 days and, in BTC alone,
//! `MAX_OPENING_LOTS` opening and as many closing lots, which keeps 20k books in memory.

use super::common::close_spread::trade;
use super::toxicity::scored_fixture;
use super::*;
use engine_core::Fixed;
use engine_core::event::Event;
use engine_core::volume_tier::{
    AccountVolume, ClosingLot, DayVolume, MAX_CLOSING_LOTS, MAX_OPENING_LOTS, MIN_HOLD_MS, OpeningLot, day_of,
};

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn volume_fill_latency() {
    // Among 100k accounts, a client with a full book opens and closes 0.01 BTC in turn: from `V59`
    // each fill reads the client's tier from its book, and counts in it.
    let mut p50s = Vec::new();
    for version in [ProtocolVersion::V58, ProtocolVersion::V59] {
        let (mut h, client, _) = scored_fixture(version, 100_000, None);
        fill_books(&mut h, [key_of(client.account)].into_iter(), true);
        let mut times = Vec::with_capacity(2 * SAMPLES);
        for sample in 0..SAMPLES {
            for side in [Side::Buy, Side::Sell] {
                let began = Instant::now();
                let events = trade(&mut h, &client, BTC, side, "0.01", "100000");
                times.push(began.elapsed().as_micros());
                assert!(events.iter().any(|e| matches!(e, Event::Fill(_))), "{sample}: {events:?}");
            }
        }
        times.sort_unstable();
        let (p50, p99) = (times[times.len() / 2 - 1], times[times.len() * 99 / 100 - 1]);
        println!("LATENCY volume_fill_{version:?} accounts=100000 p50_us={p50} p99_us={p99}");
        assert!(p99 <= BATCH_BUDGET_US, "a {version:?} fill's p99 {p99}us exceeds {BATCH_BUDGET_US}us");
        p50s.push(p50);
    }
    println!("LATENCY volume_fill_added p50_us={}", p50s[1] as i128 - p50s[0] as i128);
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn volume_trigger_latency() {
    // One price triggers a resting stop buy of 0.01 BTC for each of 20k accounts, each with a busy
    // book: from `V59` each fill reads its account's tier and counts in its book.
    const TRIGGERED: usize = 20_000;
    let mut took = Vec::new();
    for version in [ProtocolVersion::V58, ProtocolVersion::V59] {
        let (mut h, client, fillers) = scored_fixture(version, TRIGGERED, Some("100100"));
        fill_books(&mut h, fillers.iter().cloned().chain([key_of(client.account)]), false);
        h.advance(1);
        let began = Instant::now();
        let events = h.price(BTC, "100100");
        took.push(began.elapsed().as_micros());
        let fills = events.iter().filter(|e| matches!(e, Event::Fill(_))).count();
        assert_eq!(fills, TRIGGERED + 1, "every stop fills");
        println!("LATENCY volume_trigger_{version:?} fills={fills} us={}", took[took.len() - 1]);
    }
    let per_fill = (took[1] as i128 - took[0] as i128) as f64 / (TRIGGERED + 1) as f64;
    println!("LATENCY volume_trigger_added_per_fill us={per_fill:.2}");
    assert!(took[1] <= 2 * took[0] + 10_000, "V59's triggered fills took {}us against V58's {}us", took[1], took[0]);
}

/// Gives each account in `accounts` a book from `V59`: $1M counted on each of the 30 days of the
/// window, `MAX_OPENING_LOTS` opening lots of 0.01 maturing within the next minute in every listed
/// market when `full` and in BTC alone otherwise, and closing lots of BTC at distinct indices, closed
/// now, each with a matured leg of 0.01: `MAX_CLOSING_LOTS` when `full`, and `MAX_OPENING_LOTS` otherwise.
fn fill_books(h: &mut Harness, accounts: impl Iterator<Item = String>, full: bool) {
    if h.engine.protocol_version() < ProtocolVersion::V59 {
        return;
    }
    let today = day_of(h.ts);
    let days = (today - 29..=today).map(|day| DayVolume { day, volume: f("1000000") }).collect();
    let (qty, notional) = (f("0.01"), f("1000"));
    let lots: Vec<OpeningLot> = (0..MAX_OPENING_LOTS as i64)
        .rev()
        .map(|at| OpeningLot { matures_at: h.ts + MIN_HOLD_MS - at, qty, notional })
        .collect();
    let closes = if full { MAX_CLOSING_LOTS } else { MAX_OPENING_LOTS };
    let closing = (0..closes)
        .map(|at| ClosingLot {
            market: BTC,
            side: Side::Sell,
            index: Fixed::from_int(90_000 + at as i64),
            closed_at: h.ts,
            leg: qty,
            matured_leg: qty,
            qty,
            notional,
        })
        .collect();
    let markets: Vec<MarketId> = if full { h.engine.markets().keys().copied().collect() } else { vec![BTC] };
    let opening = markets.into_iter().map(|id| (id, lots.clone())).collect();
    let book = serde_json::to_value(AccountVolume { days, opening, closing }).expect("a book encodes");
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    for key in accounts {
        state["volumes"][key] = book.clone();
    }
    h.engine = serde_json::from_value(state).expect("benchmark fixture");
    h.engine.restore_indexes();
}

fn key_of(account: AccountId) -> String {
    serde_json::to_value(account).unwrap().as_str().unwrap().to_owned()
}
