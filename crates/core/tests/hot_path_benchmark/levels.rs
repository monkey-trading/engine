//! Take-profit and stop-loss levels from `V79` at 100k accounts, each holding a 0.0001 BTC long, small
//! enough that the book's utilisation raises no stress. A price visits only the levels its index reaches
//! (`levels`), so levels within the close-spread ceiling of the index cost a price no more than levels
//! out of reach. Each price is held to the resting case's 200 us p99, and what levels add is printed
//! against the same book without them; the level path alone is timed and held to 200 us p99 in
//! `crates/core/src/levels/tests.rs`. One price firing every stop loss, the first price after a
//! stress level change, which re-keys every level, and the first price after the UTC midnight that drops
//! each account to a lower volume tier, which re-keys every level, are each measured and printed, as is
//! the first price after an hour, when every account holding levels whose toxicity score holds the
//! fills a tier needs has its add-on read again for decay. The index's rebuild from state is printed too.
//!
//! Measured on 18 November 2026 on a Mac17,9 with 15 logical CPUs and 24 GiB RAM, with other workspace
//! builds running, each sample on the book with levels and on the same book without, in turn. With a
//! stop loss inside BTC's 20bp ceiling of the index and out of its closing price's reach, a price took
//! 1 / 5 us p50 / p99, against 1 / 2 us without levels; with both levels out of reach, 1 / 26 us against
//! 1 / 32 us. Pricing each in-band level instead would add about 58 ms, at 0.58 us a level. One price
//! firing all 100k stop losses took 399 ms; the first price after the operator's stress floor moved BTC
//! to level 1, re-keying every level, 684 ms; and the first after the UTC midnight that dropped every
//! account from tier 1 to tier 0, reading each account's tier and re-keying its level, 895 ms. Rebuilding
//! the index from state, as recovery and an activation do, took 0.11 to 1.08 s; a split re-keys only its
//! own market's levels. With 100k accounts whose scores hold 20 decayed fills and sit below every tier,
//! the first price after the hour took 48 ms against 13 us for the price after it.
//!
//! The level path alone took 4 / 5 us p50 / p99 in band, 3 / 6 us out of band, and 4 / 5 us in DE40
//! out of reach while EUR-USD moved. A market converting at another's index keys its levels without that
//! index, so a DE40 stop loss within its 25bp ceiling of the index is priced on every DE40 price: 100k
//! of them took 70 / 74 ms, inside the 250 ms of a price in a market every account holds, and that case
//! is printed, not held.

use super::*;
use crate::common::rerun::{hold, rerun_once};
use engine_core::Fixed;
use engine_core::event::Event;
use engine_core::volume_tier::{DAY_MS, day_of};

const LEVELS: usize = 100_000;

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn levels_price_latency() {
    rerun_once("levels_price_latency", levels_price_latency_measured);
}

fn levels_price_latency_measured() {
    // BTC's ceiling is 20bp. At 99,950 a long closes at 99,940; its stop loss at 99,790 is inside the
    // ceiling's reach of the index, and out of the closing price's. Each sample is taken on the book with
    // levels and on the same book without, in turn, and what levels add is the difference.
    for (name, mids, stop, take) in [
        ("levels_in_band", ["99950", "99951"], "99790", None),
        ("levels_out_of_band", ["100000", "100001"], "99000", Some("101000")),
    ] {
        let mut books = [held(Some(stop), take, false, false), held(None, None, false, false)];
        for h in &mut books {
            h.advance(1);
            h.price(BTC, mids[0]);
        }
        let mut times = [Vec::with_capacity(SAMPLES), Vec::with_capacity(SAMPLES)];
        for sample in 0..SAMPLES {
            for (h, times) in books.iter_mut().zip(&mut times) {
                h.advance(1);
                let began = Instant::now();
                let events = h.price(BTC, mids[sample % 2]);
                times.push(began.elapsed().as_micros());
                assert!(!events.iter().any(|e| matches!(e, Event::Fill(_))), "no level fires: {events:?}");
            }
        }
        times.iter_mut().for_each(|times| times.sort_unstable());
        let [with, without] = &times;
        let added = with[49].saturating_sub(without[49]);
        println!(
            "LATENCY {name} accounts={LEVELS} p50_us={} p99_us={} without_levels_p50_us={} without_levels_p99_us={} \
             added_p50_us={added} (budgets: 200us p99, price_wait_budget 1000us median)",
            with[49], with[98], without[49], without[98]
        );
        hold(name, with[98], 200);
    }
}

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn levels_mass_fire_and_rekey_latency() {
    // One price fires every stop loss: each closes its whole long.
    let mut h = held(Some("99000"), None, false, false);
    h.advance(MOVE_WINDOW_MS);
    let (events, us) = timed(&mut h, "98000");
    let fired = events.iter().filter(|e| matches!(e, Event::PositionLevelTriggered { .. })).count();
    assert_eq!(fired, LEVELS + 2);
    println!("LATENCY levels_mass_fire accounts={LEVELS} us={us}");

    // A stress floor the operator raises re-keys every level at the next price, the index unchanged.
    let mut h = held(Some("99000"), Some("101000"), false, false);
    h.advance(1);
    h.price(BTC, "100000");
    assert_eq!(h.engine.markets()[&BTC].stress.level, 0);
    let events = h.operate(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    h.advance(1);
    let (events, us) = timed(&mut h, "100000");
    assert!(!events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
    println!("LATENCY levels_stress_rekey accounts={LEVELS} us={us}");

    // At UTC midnight each account's first counted day leaves its window, dropping it from tier 1 to
    // tier 0: the next price reads every account's tier again and re-keys every level.
    let mut h = held(Some("99000"), Some("101000"), true, false);
    let midnight = (day_of(h.ts) + 1) * DAY_MS;
    h.advance_to(midnight - 1_000);
    h.price(BTC, "100000");
    let tier = |h: &Harness| h.engine.volume_tier(acct(9));
    assert_eq!(tier(&h), 1);
    h.advance_to(midnight + 1);
    let (_, us) = timed(&mut h, "100000");
    assert_eq!(tier(&h), 0);
    println!("LATENCY levels_tier_roll_rekey accounts={LEVELS} us={us}");

    // Every hour each account holding levels whose score holds 20 decayed fills has its add-on read again:
    // the first price after the hour reads all of them, none moving tier, and the next price reads none.
    let mut h = held(Some("99000"), Some("101000"), false, true);
    let hour = (h.ts.div_euclid(HOUR_MS) + 2) * HOUR_MS;
    h.advance_to(hour - 1_000);
    h.price(BTC, "100000");
    h.advance_to(hour + 1);
    let (events, us) = timed(&mut h, "100000");
    assert!(!events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
    h.advance(1);
    let (_, next_us) = timed(&mut h, "100000");
    println!("LATENCY levels_decay_read accounts={LEVELS} us={us} next_price_us={next_us}");
}

/// The price of BTC at `mid` now, and how long it took.
fn timed(h: &mut Harness, mid: &str) -> (Vec<Event>, u128) {
    let began = Instant::now();
    let events = h.price(BTC, mid);
    (events, began.elapsed().as_micros())
}

/// The `V79` harness's client holding a 0.0001 BTC long at 100,000 with any stop loss at `stop` and take
/// profit at `take`, and `LEVELS` copies of it, each with 30 days of tier-1 volume whose first day
/// leaves the window at the next UTC midnight when `tiered`, and a toxicity score of 30 fills and no
/// edge, below every tier, when `scored`. Prints how long the level index takes to rebuild from that
/// state.
fn held(stop: Option<&str>, take: Option<&str>, tiered: bool, scored: bool) -> Harness {
    let (mut h, client, _) = super::common::session_keys::funded_at(ProtocolVersion::V79);
    // Past the day's rollover, so the measured prices cross no financing.
    h.advance_to((day_of(h.ts) + 1) * DAY_MS - 60_000);
    h.price(BTC, "100000");
    let named = super::common::levels::levels(take, stop);
    let events = super::common::levels::open_with(&mut h, &client, BTC, Side::Buy, "0.0001", "100000", named);
    let set = events.iter().any(|e| matches!(e, Event::PositionLevelsSet { .. }));
    assert_eq!(set, !named.is_empty(), "{events:?}");
    let mut state = serde_json::to_value(&h.engine).expect("fixture encoding");
    let key_of = |account: AccountId| serde_json::to_value(account).unwrap().as_str().unwrap().to_owned();
    let template = state["accounts"][key_of(client.account)].clone();
    let today = day_of(h.ts);
    let score = json!({
        "edge": "0", "weight_squares": "1", "fills": "30", "notional": "1000", "step": h.ts.div_euclid(HOUR_MS),
    });
    let book = json!({ "days": [
        { "day": today - 29, "volume": "500000" },
        { "day": today - 1, "volume": "500000" },
    ] });
    for index in 0..LEVELS as u64 {
        let mut address = [0xdd; 20];
        address[12..].copy_from_slice(&(index + 1).to_be_bytes());
        let key = key_of(AccountId(address));
        state["accounts"][&key] = template.clone();
        if tiered {
            state["volumes"][&key] = book.clone();
        }
        if scored {
            state["toxicity_scores"][&key] = score.clone();
        }
    }
    // The tier the copies are read at is the first copy's.
    let first = key_of(AccountId({
        let mut address = [0xdd; 20];
        address[12..].copy_from_slice(&1u64.to_be_bytes());
        address
    }));
    state["accounts"][key_of(acct(9))] = state["accounts"][&first].clone();
    if tiered {
        state["volumes"][key_of(acct(9))] = book;
    }
    if scored {
        state["toxicity_scores"][key_of(acct(9))] = score;
    }
    let btc = &mut state["markets"][BTC.to_string()];
    let copies = Fixed::from_int(LEVELS as i64 + 2);
    for field in ["client_qty", "client_cost"] {
        let one: Fixed = serde_json::from_value(btc[field].clone()).expect("an amount");
        btc[field] = serde_json::to_value(one.mul(copies)).expect("an amount");
    }
    h.engine = serde_json::from_value(state).expect("benchmark fixture");
    let began = Instant::now();
    h.engine.restore_indexes();
    println!("LATENCY levels_index_rebuild accounts={LEVELS} us={}", began.elapsed().as_micros());
    h
}

/// The harness's trailing window for fast moves: a price past it raises no stress from the last one.
const MOVE_WINDOW_MS: i64 = 60_001;
/// The toxicity score's decay step.
const HOUR_MS: i64 = 60 * 60 * 1000;
