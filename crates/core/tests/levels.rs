//! Take-profit and stop-loss levels from `V79`, through `Engine::apply`: setting them on an order or by
//! request, the minimum distance, the closing price they trigger on, and how each fills. What a fill
//! does to them is in `levels/lifecycle.rs`, the slots and the distance's bounds in
//! `levels/bounds.rs`.

#[path = "levels/bounds.rs"]
mod bounds;
mod common;
#[path = "levels/lifecycle.rs"]
mod lifecycle;
#[path = "levels/tiers.rs"]
mod tiers;

use common::close_spread::trade;
use common::events::only_fill;
use common::levels::{
    closing_price, funded, held, levels, open_with, order_with, refused, removed, set, set_event, triggered,
};
use common::session_keys::funded_at;
use common::{BTC, f};
use engine_core::command::OrderKind;
use engine_core::event::{Event, Reject};
use engine_core::levels::{LevelKind, LevelsRemoved};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

/// The harness's trailing window for fast moves: a price past it leaves BTC at stress level 0.
const MOVE_WINDOW_MS: i64 = 60_000;

#[test]
fn an_order_names_levels_its_position_takes() {
    let (mut h, a, _) = funded();
    let events = open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(Some("102000"), Some("98000")));
    assert!(events.iter().any(|e| matches!(e, Event::OrderAccepted { .. })), "{events:?}");
    assert_eq!(set_event(&events), levels(Some("102000"), Some("98000")));
    assert_eq!(held(&h, &a, BTC), Some(levels(Some("102000"), Some("98000"))));
    // An order naming none leaves them as they are.
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "1", "100000"));
    assert_eq!(held(&h, &a, BTC), Some(levels(Some("102000"), Some("98000"))));
}

#[test]
fn a_request_sets_moves_and_removes_levels() {
    let (mut h, a, _) = funded();
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "1", "100000"));
    assert_eq!(set_event(&set(&mut h, &a, BTC, levels(None, Some("98000")))), levels(None, Some("98000")));
    assert_eq!(
        set_event(&set(&mut h, &a, BTC, levels(Some("103000"), Some("98500")))),
        levels(Some("103000"), Some("98500"))
    );
    assert_eq!(set_event(&set(&mut h, &a, BTC, levels(None, None))), levels(None, None));
    assert_eq!(held(&h, &a, BTC), None);
}

#[test]
fn a_level_inside_the_minimum_distance_or_on_the_wrong_side_is_refused() {
    let (mut h, a, _) = funded();
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "1", "100000"));
    // The long closes at 100000 less its 1bp first band; BTC's distance is its 20bp ceiling.
    let reference = f("99990");
    assert_eq!(closing_price(&h, &a, BTC), reference);
    let too_close = Reject::LevelTooClose { level: f("99800"), min_distance_bps: 20, reference };
    assert_eq!(refused(&set(&mut h, &a, BTC, levels(None, Some("99800")))), &too_close);
    let wrong_side = Reject::LevelTooClose { level: f("100500"), min_distance_bps: 20, reference };
    assert_eq!(refused(&set(&mut h, &a, BTC, levels(None, Some("100500")))), &wrong_side);
    let take_short = Reject::LevelTooClose { level: f("100150"), min_distance_bps: 20, reference };
    assert_eq!(refused(&set(&mut h, &a, BTC, levels(Some("100150"), None))), &take_short);
    // At the distance it is set: 20bp of 99990, rounded up, below it.
    assert_eq!(
        set_event(&set(&mut h, &a, BTC, levels(Some("100189.98"), Some("99790.02")))).stop_loss,
        Some(f("99790.02"))
    );
    // A level left where it is is not measured again, though the price has come near it.
    h.price(BTC, "99900");
    assert_eq!(
        set_event(&set(&mut h, &a, BTC, levels(Some("101000"), Some("99790.02")))).take_profit,
        Some(f("101000"))
    );
}

#[test]
fn a_market_orders_levels_are_measured_from_the_position_it_leaves_and_refuse_the_whole_order() {
    let (mut h, a, _) = funded();
    // Two BTC long close at 100000 less 1bp, so a stop loss at 99800 is inside 20bp of 99990.
    let events = open_with(&mut h, &a, BTC, Side::Buy, "2", "100000", levels(None, Some("99800")));
    let too_close = Reject::LevelTooClose { level: f("99800"), min_distance_bps: 20, reference: f("99990") };
    assert_eq!(refused(&events), &too_close);
    assert!(!events.iter().any(|e| matches!(e, Event::Fill(_))), "{events:?}");
    assert!(h.engine.accounts()[&a.account].positions.is_empty());
}

#[test]
fn a_reduce_only_order_or_an_order_before_v79_naming_levels_is_refused() {
    let (mut h, a, _) = funded();
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "1", "100000"));
    let kind = OrderKind::Market { quoted: f("100000"), max_slippage_bps: None };
    let events = order_with(&mut h, &a, BTC, Side::Sell, "1", kind, true, levels(None, Some("101000")));
    assert!(matches!(refused(&events), Reject::InvalidOrder { .. }), "{events:?}");
    let (mut h, a, _) = funded_at(ProtocolVersion::V74);
    let events = open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(None, Some("98000")));
    assert_eq!(refused(&events), &Reject::LevelsUnsupported);
    assert_eq!(refused(&set(&mut h, &a, BTC, levels(None, Some("98000")))), &Reject::LevelsUnsupported);
}

#[test]
fn a_stop_loss_fills_the_whole_position_at_the_closing_price_that_reached_it_gap_included() {
    let (mut h, a, _) = funded();
    open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(Some("102000"), Some("98000")));
    // A price just above the stop's reach leaves it; one well below fills at that price's close.
    assert_eq!(triggered(&h.price(BTC, "98010")), None);
    let events = past_the_move_window(&mut h, "97000");
    let (kind, order_id) = triggered(&events).expect("the stop loss fires");
    let fill = only_fill(&events);
    assert_eq!((kind, fill.order_id, fill.qty, fill.price), (LevelKind::StopLoss, order_id, f("1"), f("96990.3")));
    assert_eq!(fill.position, f("0"));
    assert_eq!(removed(&events), LevelsRemoved::StopLoss);
    assert_eq!(held(&h, &a, BTC), None);
}

#[test]
fn a_take_profit_fills_at_its_level_or_better_once_the_closing_price_reaches_it() {
    let (mut h, a, _) = funded();
    open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(Some("101000"), None));
    // The index at the level closes 1bp short of it.
    assert_eq!(triggered(&h.price(BTC, "101000")), None);
    let events = h.price(BTC, "101011");
    assert_eq!(triggered(&events).map(|(kind, _)| kind), Some(LevelKind::TakeProfit));
    assert!(only_fill(&events).price >= f("101000"), "{events:?}");
    assert_eq!(removed(&events), LevelsRemoved::TakeProfit);
}

#[test]
fn a_shorts_levels_mirror_a_longs() {
    let (mut h, a, _) = funded();
    open_with(&mut h, &a, BTC, Side::Sell, "1", "100000", levels(Some("98000"), Some("102000")));
    assert_eq!(triggered(&h.price(BTC, "101980")), None);
    let events = past_the_move_window(&mut h, "103000");
    assert_eq!(triggered(&events).map(|(kind, _)| kind), Some(LevelKind::StopLoss));
    assert_eq!(only_fill(&events).price, f("103010.3"));
}

#[test]
fn a_raised_stress_level_fires_a_stop_loss_at_the_next_price_with_the_index_unchanged() {
    let (mut h, a, _) = funded();
    open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(None, Some("99500")));
    // At level 0 the close pays 1bp; level 1 adds 5bp and doubles the band, 7bp in all.
    assert_eq!(triggered(&h.price(BTC, "99520")), None);
    let events = h.operate(engine_core::command::AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
    assert_eq!(triggered(&events), None);
    let events = h.price(BTC, "99520");
    assert_eq!(triggered(&events).map(|(kind, _)| kind), Some(LevelKind::StopLoss));
    assert_eq!(only_fill(&events).price, f("99450.336"));
}

#[test]
fn nothing_fires_while_the_market_is_unpriced_and_the_first_price_after_fills_what_it_gapped_through() {
    let (mut h, a, _) = funded();
    open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(None, Some("98000")));
    // The quotes age out: a lone fresh quote is short of the market's two sources.
    h.advance(MOVE_WINDOW_MS + 1);
    let events = h.quotes(BTC, &["90000"]);
    assert_eq!(triggered(&events), None);
    // The market is open, so its last mark measures no level: only a shut market's does.
    let no_price = Reject::InvalidOrder { reason: "no price to measure levels from".into() };
    assert_eq!(refused(&set(&mut h, &a, BTC, levels(Some("110000"), Some("98000")))), &no_price);
    let events = h.price(BTC, "95000");
    assert_eq!(triggered(&events).map(|(kind, _)| kind), Some(LevelKind::StopLoss));
    assert_eq!(only_fill(&events).price, f("94990.5"));
}

#[test]
fn levels_fire_in_a_reduce_only_market_and_before_the_close_out_on_the_same_price() {
    let (mut h, a, _) = funded();
    // 9 BTC on 100000 at 10% margin; close-out at half the margin, about 94.4k.
    open_with(&mut h, &a, BTC, Side::Buy, "9", "100000", levels(None, Some("96000")));
    h.operate(engine_core::command::AdminCommand::SetReduceOnly { market: Some(BTC), reduce_only: true });
    let events = h.price(BTC, "90000");
    assert_eq!(triggered(&events).map(|(kind, _)| kind), Some(LevelKind::StopLoss));
    assert!(!events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    assert!(!only_fill(&events).liquidation);
}

/// BTC priced at `mid` once the fast-move window has passed since the last price, so the move to it
/// raises no stress.
fn past_the_move_window(h: &mut common::Harness, mid: &str) -> Vec<Event> {
    h.advance(MOVE_WINDOW_MS + 1);
    h.price(BTC, mid)
}
