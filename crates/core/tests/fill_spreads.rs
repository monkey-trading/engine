//! `V52`'s size spread on every fill, through `Engine::apply`: the slice that closes a position pays
//! the market's bands as the slice that grows it does, in tenths of a bp, at the stress level's
//! multiplier, whatever closes it, and a pure close clamps to its ceiling before the check that a
//! spread stays under the price. Listings and their notices are in `listing_notice.rs`.

mod common;

use common::batch::{close_all, closes};
use common::close_spread::{attest, fund, reduce, rest, trade};
use common::events::{fills, only_fill};
use common::spreads::funded;
use common::{BTC, Client, f};
use engine_core::command::{AdminCommand, OrderKind};
use engine_core::event::Event;
use engine_core::operator_bounds::{NOTICE_MS, ParamsNotice, StressSettings};
use engine_core::params::ToxicityTier;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

const V51: ProtocolVersion = ProtocolVersion::V51;
const V52: ProtocolVersion = ProtocolVersion::V52;

#[test]
fn a_round_trip_pays_the_bands_on_the_open_and_again_on_the_close() {
    // (version, open price, close price): `V52`'s bands read 1bp to 1m and 2bp to 5m, so $2m pays
    // 100 + 200 = 300, 15 a unit, each way; `V51`'s read 0 and 2bp, and charge only the open.
    for (version, open, close) in [(V52, "100015", "99985"), (V51, "100010", "100000")] {
        let (mut h, client) = funded(version, "1000000");
        let before = h.balance(client.account);
        assert_eq!(only_fill(&trade(&mut h, &client, BTC, Side::Buy, "20", "100000")).price, f(open), "{version:?}");
        assert_eq!(only_fill(&trade(&mut h, &client, BTC, Side::Sell, "20", "100000")).price, f(close), "{version:?}");
        // The round trip's cost is the spread both fills paid, a loss the profit fee does not touch.
        assert_eq!(h.balance(client.account), before - (f(open) - f(close)).mul_int(20), "{version:?}");
        h.assert_conserved(f("0"));
    }
}

#[test]
fn partial_closes_pay_each_band_they_close_through() {
    let (mut h, client) = funded(V52, "5000000");
    // Opening $12m pays 1m at 1bp, 4m at 2bp and 7m at 5bp: 4400 over 120 units, rounded up.
    let open = only_fill(&trade(&mut h, &client, BTC, Side::Buy, "120", "100000")).price;
    assert_eq!(open, f("100036.666666667"));
    // (units sold, price): 12m to 5m at 5bp; 5m to 2m at 2bp; 2m to 0.5m across two bands (200 +
    // 50 over 15); the last 0.5m at 1bp.
    for (qty, price) in [("70", "99950"), ("30", "99980"), ("15", "99983.333333333"), ("5", "99990")] {
        let fill = only_fill(&trade(&mut h, &client, BTC, Side::Sell, qty, "100000")).clone();
        assert_eq!(fill.price, f(price), "selling {qty}: {fill:?}");
    }
    assert!(h.engine.accounts()[&client.account].positions.is_empty());
    h.assert_conserved(f("0"));
}

#[test]
fn a_reversal_pays_its_close_through_the_bands_and_then_its_open() {
    let (mut h, client) = funded(V52, "1000000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "20", "100000"));
    // Closing $2m pays 300 and opening $1m short pays 100: 400 over 30 units, rounded up.
    let fill = only_fill(&trade(&mut h, &client, BTC, Side::Sell, "30", "100000")).clone();
    assert_eq!((fill.price, fill.position), (f("99986.666666666"), f("-10")));
}

#[test]
fn the_stress_multiplier_scales_the_closing_slice_too() {
    let (mut h, client) = funded(V52, "1000000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "10", "100000"));
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
    // Level 1 doubles the $1m close's 100 to 200, 20 a unit, and adds its 5bp minimum.
    assert_eq!(only_fill(&trade(&mut h, &client, BTC, Side::Sell, "10", "100000")).price, f("99930"));
}

#[test]
fn a_reduce_only_order_and_a_close_all_pay_the_closing_spread() {
    let (mut h, client) = funded(V52, "1000000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "10", "100000"));
    // Each close of $0.5m pays 1bp of the mid a unit.
    assert_eq!(only_fill(&reduce(&mut h, &client, BTC, Side::Sell, "5", "100000")).price, f("99990"));
    let nonce = h.engine.next_request_nonce(client.account);
    let request = close_all(&h, &client, client.account, nonce, closes(&[BTC], "100000"), None);
    assert_eq!(only_fill(&h.cmd(request)).price, f("99990"));
    assert!(h.engine.accounts()[&client.account].positions.is_empty());
}

#[test]
fn a_closing_limit_fills_only_once_its_price_with_the_spread_reaches_the_limit() {
    let (mut h, client) = funded(V52, "1000000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "5", "100000"));
    rest(&mut h, &client, BTC, Side::Sell, "5", OrderKind::Limit { price: f("100005") });
    // At 100005 and 100015 the mid reaches the limit but the price, 1bp of the mid below it, does
    // not: the order rests.
    for mid in ["100005", "100015"] {
        let events = h.price(BTC, mid);
        assert!(fills(&events).is_empty() && !events.iter().any(|e| matches!(e, Event::OrderCancelled { .. })));
    }
    let fill = only_fill(&h.price(BTC, "100016")).clone();
    assert_eq!(fill.price, f("100005.9984"));
    assert!(fill.price >= f("100005"));
}

#[test]
fn a_triggered_stop_pays_the_closing_spread() {
    let (mut h, client) = funded(V52, "1000000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "5", "100000"));
    rest(&mut h, &client, BTC, Side::Sell, "5", OrderKind::Stop { price: f("99990") });
    assert_eq!(only_fill(&h.price(BTC, "99990")).price, f("99980.001"));
}

#[test]
fn a_close_out_pays_the_size_spread_and_no_toxicity_add_on() {
    // (version, close-out price): `V52` charges $0.837m of BTC 1bp, 9.3 a unit; `V51` nothing.
    for (version, closed_at) in [(V52, "92990.7"), (V51, "93000")] {
        let (mut h, client) = funded(version, "10");
        let mut params = h.engine.params().unwrap().clone();
        // Level 1's move trigger out of reach, so the 7% drop leaves BTC at level 0.
        params.stress_levels[1].trigger.as_mut().unwrap().move_bps = Some(10_000);
        announce_and_apply(&mut h, params.clone());
        // Every account pays a 3bp toxicity add-on, which a close-out waives.
        params.toxicity.tiers = vec![ToxicityTier { min_score_bps: 0, extra_spread_bps: 3 }];
        h.operator_admin(AdminCommand::SetParams(params));
        h.price(BTC, "100000");
        only_fill(&trade(&mut h, &client, BTC, Side::Buy, "9", "100000"));
        let events = h.price(BTC, "93000");
        let fill = only_fill(&events);
        assert!(fill.liquidation, "{events:?}");
        assert_eq!(fill.price, f(closed_at), "{version:?}: {events:?}");
    }
}

#[test]
fn a_pure_close_prices_at_the_widest_admitted_parameters() {
    let (mut h, client) = funded(V52, "1000000");
    let other = Client::new(2);
    attest(&mut h, &other);
    fund(&mut h, &other, "100000");
    // The widest stress level the bounds admit beside a 3bp add-on: a 9996bp minimum at ten times
    // the size spread, then a 1bp add-on for every account, so every close's spread reaches the price.
    let mut params = h.engine.params().unwrap().clone();
    params.stress_levels[1].size_spread_multiplier_bps = 100_000;
    params.stress_levels[1].min_spread_bps = 9_996;
    announce_and_apply(&mut h, params.clone());
    params.toxicity.tiers = vec![ToxicityTier { min_score_bps: 0, extra_spread_bps: 1 }];
    h.operator_admin(AdminCommand::SetParams(params));
    h.price(BTC, "100000");
    only_fill(&trade(&mut h, &client, BTC, Side::Buy, "10", "100000"));
    only_fill(&trade(&mut h, &other, BTC, Side::Buy, "9", "100000"));
    h.operator_admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });

    // A reduce-only close, a close-all and a close-out each clamp to BTC's 20bp ceiling.
    assert_eq!(only_fill(&reduce(&mut h, &client, BTC, Side::Sell, "4", "99800")).price, f("99800"));
    let nonce = h.engine.next_request_nonce(client.account);
    let request = common::batch::close_all_expiring(
        &h,
        &client,
        client.account,
        nonce,
        closes(&[BTC], "99800"),
        None,
        h.ts + 60_000,
    );
    assert_eq!(only_fill(&h.cmd(request)).price, f("99800"));
    let events = h.price(BTC, "93000");
    let fill = only_fill(&events);
    assert!(fill.liquidation && fill.account == other.account, "{events:?}");
    assert_eq!(fill.price, f("92814"), "{events:?}");
}

/// Applies `params`, whose stress parameters differ, through a notice and the seven days it waits.
fn announce_and_apply(h: &mut common::Harness, params: engine_core::params::ProtocolParams) {
    let stress = Some(StressSettings::of(&params));
    let due = h.ts + NOTICE_MS;
    let notice = ParamsNotice { profit_fee_bps: None, closeout_level_bps: None, stress, effective_at: due };
    h.operator_admin(AdminCommand::AnnounceParams(notice));
    h.advance_to(due);
    h.operator_admin(AdminCommand::SetParams(params));
}
