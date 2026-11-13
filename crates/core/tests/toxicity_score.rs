//! The toxicity score from `V54`, end to end through `Engine::apply`: a markout from the fill's
//! index in units of its market's markout spread, a decayed size-capped edge over the root of its
//! squared weights, tiers only from 20 decayed fills, and close-outs left unmarked.

mod common;

use common::events::fills;
use common::session_keys::funded_at;
use common::toxicity::{BAND_BPS, FLOOR_BPS, another, crypto, fill, floored, mark, relisted_v54, scored, standing};
use common::{AAPL, BTC, ETH, Harness, f, ny, quote};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Fill;
use engine_core::fixed::Rounding;
use engine_core::market::{MarketSpec, PriceRules};
use engine_core::protocol::ProtocolVersion;
use engine_core::toxicity::{GRID_MS, HALF_LIFE_MS, HORIZON_MS, MIN_FILLS};
use engine_core::types::Side;

#[test]
fn a_toxic_account_pays_nothing_until_twenty_fills_score_then_its_tiers_add_on() {
    // Each buy sees the index rise one or two bps a horizon later: about one or two standard errors
    // of the 1 bp floor a fill, so 20 fills score about 4.5 or 8.9, the first or the second tier.
    for (step, add_on) in [("10", 2), ("20", 5)] {
        let (mut h, client) = scored();
        let mut mid = f("100000");
        let mut buys: Vec<(Fill, Fixed)> = Vec::new();
        for _ in 0..25 {
            let buy = fill(&mut h, &client, Side::Buy, "0.1", &mid.to_string());
            let before = mid;
            mid += f(step);
            mark(&mut h, &mid.to_string());
            buys.push((buy, before));
        }
        let paid: Vec<i64> = buys.iter().map(|(buy, _)| buy.spread_bps - BAND_BPS).collect();
        let mut expected = [add_on; 25];
        expected[..MIN_FILLS as usize].fill(0);
        assert_eq!(paid, expected, "a {step} step");
        let now = standing(&h, client.account);
        assert_eq!((now.tier, now.add_on_bps, now.fills), (if add_on == 2 { 1 } else { 2 }, add_on, f("25")));
        // The score as the test computes it from the moves: each buy weighs its notional, and its
        // markout is the move to the next price in bps of its index, over the floor.
        let (edge, squares) = buys.iter().enumerate().fold((Fixed::ZERO, Fixed::ZERO), |(e, w2), (k, (buy, index))| {
            let next = buys.get(k + 1).map_or(mid, |(_, index)| *index);
            let errors =
                (next - *index).mul_div(f("10000"), *index, Rounding::Up).div_rounded(f(FLOOR_BPS), Rounding::Up);
            let weight = buy.qty.mul(buy.price);
            (e + weight.mul_rounded(errors, Rounding::Up), w2 + weight.mul(weight))
        });
        assert_close(now.score, edge, squares);
    }
}

#[test]
fn an_edgeless_account_stays_on_the_best_price() {
    // 25 fills on sides a fixed shuffle picks, while the index steps a bp up or down at random.
    let (mut h, client) = scored();
    let mut mid = f("100000");
    let mut state: u32 = 7;
    let mut next = || {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        (state >> 16) & 1 == 1
    };
    let mut paid = Vec::new();
    for _ in 0..25 {
        let side = if next() { Side::Buy } else { Side::Sell };
        paid.push(fill(&mut h, &client, side, "0.1", &mid.to_string()).spread_bps);
        mid += if next() { f("10") } else { f("-10") };
        mark(&mut h, &mid.to_string());
    }
    assert!(paid.iter().all(|bps| *bps == BAND_BPS), "{paid:?}");
    let now = standing(&h, client.account);
    assert_eq!((now.tier, now.add_on_bps, now.fills), (0, 0, f("25")));
    assert!(now.score < f("4"), "{now:?}");
}

#[test]
fn a_markout_is_measured_from_the_fills_index_so_the_spread_paid_is_no_edge() {
    // $1.5m pays the second size band's spread above $1m; the index holds still, so the markout
    // is zero, not the spread against the fill's price.
    let (mut h, _) = scored();
    let client = another(&mut h, 3, "1000000");
    let buy = fill(&mut h, &client, Side::Buy, "15", "100000");
    assert!(buy.price > buy.index, "{buy:?}");
    mark(&mut h, "100000");
    let now = standing(&h, client.account);
    assert_eq!((now.score, now.fills), (Fixed::ZERO, f("1")));
}

#[test]
fn the_score_and_its_fills_halve_each_half_life_so_an_idle_tier_lapses() {
    let (mut h, client) = scored();
    let mut mid = f("100000");
    for _ in 0..25 {
        fill(&mut h, &client, Side::Buy, "0.1", &mid.to_string());
        mid += f("10");
        mark(&mut h, &mid.to_string());
    }
    let before = standing(&h, client.account);
    assert_eq!(before.tier, 1);
    // A week on, the sums are half: 12.5 fills are too few for a tier, and the score is the old one
    // over the root of two.
    h.advance(HALF_LIFE_MS);
    h.price(BTC, &mid.to_string());
    let after = standing(&h, client.account);
    assert_eq!((after.fills, after.tier, after.add_on_bps), (f("12.5"), 0, 0));
    let ratio = after.score.mul_div(f("1000"), before.score, Rounding::Down);
    assert!((f("706")..=f("708")).contains(&ratio), "{before:?} -> {after:?}");
    assert_eq!(fill(&mut h, &client, Side::Buy, "0.1", &mid.to_string()).spread_bps, BAND_BPS);
}

#[test]
fn a_close_out_is_not_marked() {
    let (mut h, _) = scored();
    // The initial margin of the buy and its 1 bp spread.
    let thin = another(&mut h, 2, "1001");
    fill(&mut h, &thin, Side::Buy, "0.1", "100000");
    // A horizon on, the index has fallen past the close-out level: the buy scores, and the
    // close-out that fills at the same price is not marked.
    h.advance(HORIZON_MS);
    let events = h.price(BTC, "94000");
    assert!(fills(&events).iter().any(|fill| fill.liquidation), "{events:?}");
    mark(&mut h, "94000");
    mark(&mut h, "94000");
    assert_eq!(standing(&h, thin.account).fills, f("1"));
}

#[test]
fn a_markout_due_in_a_venue_gap_scores_at_the_first_index_within_a_horizon_after() {
    let (mut h, client) = scored();
    fill(&mut h, &client, Side::Buy, "0.1", "100000");
    // At the due time the quotes are a horizon old, past their second of freshness: the markout
    // waits, and the first index after it, 2 s later, scores it.
    h.advance(HORIZON_MS);
    h.cmd(Command::Tick);
    assert_eq!(standing(&h, client.account).fills, Fixed::ZERO);
    h.advance(2_000);
    h.price(BTC, "100010");
    let now = standing(&h, client.account);
    assert_eq!(now.fills, f("1"));
    assert!(now.score.is_positive());
    // A gap that outlasts a horizon after the due time drops the markout unscored.
    fill(&mut h, &client, Side::Buy, "0.1", "100010");
    h.advance(2 * HORIZON_MS + 1);
    h.price(BTC, "100020");
    assert_eq!(standing(&h, client.account).fills, f("1"));
}

#[test]
fn the_markout_spread_is_the_root_mean_square_of_the_trailing_hours_grid_changes() {
    // ETH moves as BTC does, under a floor above the moves' root mean square.
    let listings = [floored(crypto(BTC, "BTC-USD"), "0.5"), floored(crypto(ETH, "ETH-USD"), "3")];
    let (mut h, _) = relisted_v54(listings);
    // Two grid points on, the genesis price is stale: the first point here starts the history.
    h.advance_to(h.ts.div_euclid(GRID_MS) * GRID_MS + 2 * GRID_MS);
    // Held at each grid point's mid until the next, alternately 100000 and 100020, but for one
    // interval left unpriced, whose start the last price before it still prices: that change is
    // zero, and the change across the unpriced interval counts nothing.
    let mids = ["100000", "100020", "100000", "100020", "", "100020", "100000", "100020"];
    for mid in mids {
        hold(&mut h, mid);
    }
    let changes = [
        ("100000", "100020"),
        ("100020", "100000"),
        ("100000", "100020"),
        ("100020", "100020"),
        ("100020", "100000"),
        ("100000", "100020"),
    ];
    let (sum, count) = changes.iter().fold((0i128, 0i128), |(sum, count), (a, b)| {
        let bps = (f(b) - f(a)).mul_div(f("10000"), f(a), Rounding::TowardZero);
        (sum + bps.mul(bps).raw(), count + 1)
    });
    let rms = Fixed::from_raw(((sum / count) as u128 * 1_000_000_000).isqrt() as i128);
    assert!(rms > f("1.82") && rms < f("1.83"), "{rms}");
    assert_eq!(h.engine.markout_spread_bps(BTC), Some(rms));
    assert_eq!(h.engine.markout_spread_bps(ETH), Some(f("3")));
    // An hour and a grid point on with no price, the last change, sampled at the last price, has
    // left the trailing hour, and the floor is the spread.
    h.advance(60 * 60 * 1000 + GRID_MS);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.markout_spread_bps(BTC), Some(f("0.5")));
}

#[test]
fn a_change_across_a_moment_the_market_was_unpriced_counts_nothing() {
    let (mut h, _) = relisted_v54([floored(crypto(BTC, "BTC-USD"), "0.5"), floored(crypto(ETH, "ETH-USD"), "0.5")]);
    h.advance_to(h.ts.div_euclid(GRID_MS) * GRID_MS + 2 * GRID_MS);
    let start = h.ts;
    // Venues 2 and 3 quote BTC alone.
    let far =
        |ts, mid| Command::Prices { market: BTC, quotes: vec![quote("venue2", mid, ts), quote("venue3", mid, ts)] };
    h.price(BTC, "100000");
    h.price(ETH, "100000");
    h.advance(500);
    h.price(ETH, "100000");
    // At 0.8 s BTC's venues 2 and 3 quote 60 bps over venues 0 and 1, past its divergence bound,
    // until 0 and 1 go stale and 2 and 3 alone price it: the next price command finds it priced.
    h.advance(300);
    h.cmd(far(h.ts, "100600"));
    // ETH's quotes go stale at 1.5 s, until a price at 2.9 s.
    for offset in [1_100, 2_000, 2_900, 3_800, 4_700] {
        h.advance_to(start + offset);
        h.cmd(far(h.ts, "100600"));
        if offset > 2_000 {
            h.price(ETH, "100000");
        }
    }
    // Each has an index at both grid points and was open throughout: the change still counts
    // nothing, and the spread is the floor.
    h.advance_to(start + GRID_MS);
    h.cmd(far(h.ts, "100620"));
    h.price(ETH, "100020");
    h.advance(100);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.markout_spread_bps(BTC), Some(f("0.5")));
    assert_eq!(h.engine.markout_spread_bps(ETH), Some(f("0.5")));
}

#[test]
fn a_change_across_a_relisting_that_unpriced_the_market_counts_nothing() {
    let listing = floored(crypto(BTC, "BTC-USD"), "0.5");
    let (mut h, _) = relisted_v54([listing.clone()]);
    h.advance_to(h.ts.div_euclid(GRID_MS) * GRID_MS + 2 * GRID_MS);
    let start = h.ts;
    h.price(BTC, "100000");
    // Between two grid points, a relisting asks three venues of BTC's two and the next takes it
    // back: no price command finds it unpriced.
    h.advance(500);
    for min_sources in [3, 2] {
        let price_rules = PriceRules { min_sources, ..listing.price_rules };
        h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { price_rules, ..listing.clone() }));
    }
    while h.ts < start + GRID_MS {
        h.price(BTC, "100000");
        h.advance((start + GRID_MS - h.ts).min(900));
    }
    h.price(BTC, "100020");
    h.advance(100);
    h.cmd(Command::Tick);
    assert_eq!(h.engine.markout_spread_bps(BTC), Some(f("0.5")));
}

#[test]
fn a_grid_point_with_the_market_closed_counts_no_change() {
    // AAPL keeps its quotes fresh for a minute, past the close at 16:00.
    let listed = funded_at(ProtocolVersion::V51).0.engine.markets()[&AAPL].spec.clone();
    let price_rules = PriceRules { max_age_ms: 60_000, ..listed.price_rules };
    let (mut h, _) = relisted_v54([MarketSpec { price_rules, ..floored(listed, "0.01") }]);
    h.advance_to(ny(2026, 10, 20, 15, 59) + 50_000);
    h.price(AAPL, "200");
    h.advance(GRID_MS);
    h.price(AAPL, "200.02");
    h.advance(2 * GRID_MS);
    h.cmd(Command::Tick);
    // One change of 1 bp, from 15:59:50 to 15:59:55: 16:00:00 is closed, so the quote still fresh
    // there adds no zero change.
    assert_eq!(h.engine.markout_spread_bps(AAPL), Some(f("1")));
}

/// Prices BTC and ETH at `mid` every 900 ms from now to the next grid point, so neither is unpriced
/// at any command, and moves to it; an empty `mid` leaves both unpriced.
fn hold(h: &mut Harness, mid: &str) {
    let next = (h.ts.div_euclid(GRID_MS) + 1) * GRID_MS;
    while h.ts < next {
        if !mid.is_empty() {
            h.price(BTC, mid);
            h.price(ETH, mid);
        }
        h.advance((next - h.ts).min(900));
    }
}

/// Checks `score` is `edge / sqrt(squares)` to within a millionth: the engine's square root and
/// division round down.
fn assert_close(score: Fixed, edge: Fixed, squares: Fixed) {
    let tolerance = f("0.000001");
    let at = |s: Fixed| s.mul(s).mul(squares);
    let target = edge.mul(edge);
    assert!(at(score - tolerance) <= target && target <= at(score + tolerance), "{score} vs {edge} / sqrt({squares})");
}
