//! What a fill, a close-out, a split and an entry order do to a position's levels.

use crate::common::batch::{cancel_all, close_all, closes};
use crate::common::close_spread::{reduce, trade};
use crate::common::events::only_fill;
use crate::common::levels::{funded, held, levels, open_with, order_with, refused, removed, set, set_event, triggered};
use crate::common::weekend::{OIL, OIL_WEEKEND_BPS, chicago, client, genesis_v60, globex};
use crate::common::{AAPL, BTC, f, ny};
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::corporate::{Adjustment, CorporateAction};
use engine_core::event::{Event, Reject};
use engine_core::levels::{LevelKind, LevelsRemoved};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

#[test]
fn an_add_and_a_partial_close_keep_both_levels_over_the_whole_position() {
    let (mut h, a, _) = funded();
    open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(Some("102000"), Some("98000")));
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "1", "100000"));
    only_fill(&reduce(&mut h, &a, BTC, Side::Sell, "0.5", "100000"));
    assert_eq!(held(&h, &a, BTC), Some(levels(Some("102000"), Some("98000"))));
    // The stop loss closes all 1.5 that is left.
    let events = super::past_the_move_window(&mut h, "97000");
    assert_eq!((only_fill(&events).qty, only_fill(&events).position), (f("1.5"), f("0")));
}

#[test]
fn an_add_keeps_a_level_it_does_not_name_and_measures_only_those_it_moves() {
    let (mut h, a, _) = funded();
    open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(Some("102000"), Some("99790.02")));
    let events = open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(Some("103000"), None));
    assert_eq!(set_event(&events), levels(Some("103000"), Some("99790.02")));
    // Three BTC would close at 99890.01, inside 20bp of the stop loss: named unchanged it is not measured
    // again, while moving it is.
    h.price(BTC, "99900");
    let events = open_with(&mut h, &a, BTC, Side::Buy, "1", "99900", levels(Some("103000"), Some("99790.02")));
    assert_eq!(set_event(&events), levels(Some("103000"), Some("99790.02")));
    let events = open_with(&mut h, &a, BTC, Side::Buy, "1", "99900", levels(None, Some("99800")));
    assert!(matches!(refused(&events), Reject::LevelTooClose { level, .. } if *level == f("99800")), "{events:?}");
    // A reversal's levels are the new position's own, so the long's take profit is measured for the short.
    let events = open_with(&mut h, &a, BTC, Side::Sell, "6", "99900", levels(Some("103000"), None));
    assert!(matches!(refused(&events), Reject::LevelTooClose { level, .. } if *level == f("103000")), "{events:?}");
    assert_eq!(held(&h, &a, BTC), Some(levels(Some("103000"), Some("99790.02"))));
}

#[test]
fn a_reversal_removes_the_levels_unless_its_order_names_the_new_positions() {
    let (mut h, a, _) = funded();
    open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(Some("102000"), Some("98000")));
    let events = trade(&mut h, &a, BTC, Side::Sell, "2", "100000");
    assert_eq!(removed(&events), LevelsRemoved::Reversed);
    assert_eq!(held(&h, &a, BTC), None);
    // Back long, naming levels: the short's none go, and the long takes the order's.
    let events = open_with(&mut h, &a, BTC, Side::Buy, "2", "100000", levels(Some("103000"), Some("97000")));
    assert!(!events.iter().any(|e| matches!(e, Event::PositionLevelsRemoved { .. })), "{events:?}");
    assert_eq!(set_event(&events), levels(Some("103000"), Some("97000")));
    let events = open_with(&mut h, &a, BTC, Side::Sell, "2", "100000", levels(Some("97000"), Some("103000")));
    assert_eq!(removed(&events), LevelsRemoved::Reversed);
    assert_eq!(set_event(&events), levels(Some("97000"), Some("103000")));
    assert!(h.engine.accounts()[&a.account].positions[&BTC].qty.is_negative());
}

#[test]
fn a_client_close_a_close_all_and_a_close_out_each_remove_the_levels_saying_why() {
    let (mut h, a, b) = funded();
    open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(None, Some("98000")));
    assert_eq!(removed(&reduce(&mut h, &a, BTC, Side::Sell, "1", "100000")), LevelsRemoved::Closed);
    open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(None, Some("98000")));
    let nonce = h.engine.next_request_nonce(a.account);
    let events = h.cmd(close_all(&h, &a, a.account, nonce, closes(&[BTC], "100000"), None));
    assert_eq!(removed(&events), LevelsRemoved::CloseAll);
    // Nine BTC on $100k at 10% margin close out near 93.5k; the stop loss is out of reach.
    crate::common::batch::onboard(&mut h, &b, "100000");
    open_with(&mut h, &b, BTC, Side::Buy, "9", "100000", levels(None, Some("50000")));
    let events = super::past_the_move_window(&mut h, "93000");
    assert!(events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    assert_eq!(removed(&events), LevelsRemoved::CloseOut);
    assert_eq!(triggered(&events), None);
}

#[test]
fn a_weekend_close_out_removes_the_levels_and_a_stop_loss_gapped_over_the_weekend_fills_at_the_reopening() {
    let mut h = genesis_v60(globex(vec![]), OIL_WEEKEND_BPS, 0);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V79 });
    let (thin, held_over) = (client(&mut h, 2, "50000"), client(&mut h, 3, "200000"));
    h.price(OIL, "70");
    // $700k of oil needs 7.5% over the weekend: $52.5k, more than the thin account holds.
    open_with(&mut h, &thin, OIL, Side::Buy, "10000", "70", levels(None, Some("60")));
    open_with(&mut h, &held_over, OIL, Side::Buy, "10000", "70", levels(None, Some("66")));
    let close = chicago(2026, 10, 23, 16, 0);
    h.advance_to(close + 60 * 60 * 1000);
    let events = h.cmd(Command::Tick);
    assert_eq!(removed(&events), LevelsRemoved::WeekendCloseOut);
    assert_eq!(held(&h, &held_over, OIL), Some(levels(None, Some("66"))));
    // While the market is shut a level is measured from the position's close at the last mark.
    assert_eq!(
        set_event(&set(&mut h, &held_over, OIL, levels(Some("75"), Some("66")))),
        levels(Some("75"), Some("66"))
    );
    // Nothing fires while the market is shut; the first price after the reopening fills the stop loss.
    h.advance_to(chicago(2026, 10, 25, 17, 0));
    let events = h.price(OIL, "65");
    assert_eq!(triggered(&events).map(|(kind, _)| kind), Some(LevelKind::StopLoss));
    assert!(only_fill(&events).price < f("65"), "{events:?}");
}

#[test]
fn a_split_scales_levels_as_it_scales_working_orders_and_removes_those_of_a_position_it_rounds_to_zero() {
    let (mut h, a, b) = funded();
    crate::common::batch::onboard(&mut h, &b, "100000");
    h.price(AAPL, "80");
    open_with(&mut h, &a, AAPL, Side::Buy, "3", "80", levels(Some("90.000000001"), Some("70.000000001")));
    // A long of one raw unit, which the split's three for four rounds to nothing, keeps its levels.
    open_with(&mut h, &b, AAPL, Side::Buy, "1", "80", levels(Some("90"), Some("70")));
    only_fill(&reduce(&mut h, &b, AAPL, Side::Sell, "0.999999999", "80"));
    assert_eq!(held(&h, &b, AAPL), Some(levels(Some("90"), Some("70"))));
    // From `V76` a split needs scheduling at least seven days before its previous close.
    let split = CorporateAction {
        id: "levels".into(),
        market: AAPL,
        previous_close: ny(2026, 10, 28, 16, 0),
        ex_open: ny(2026, 10, 29, 9, 30),
        adjustment: Adjustment::Split { new_units: 3, old_units: 4 },
    };
    let events = h.operate(AdminCommand::ScheduleCorporateAction(split));
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    h.advance_to(ny(2026, 10, 29, 9, 30));
    let events = h.price(AAPL, "106.666666667");
    // Each price times 4/3, rounded as a sell limit (up) and a sell stop (down) would be.
    assert_eq!(held(&h, &a, AAPL), Some(levels(Some("120.000000002"), Some("93.333333334"))));
    assert_eq!(removed(&events), LevelsRemoved::Split);
    assert_eq!(held(&h, &b, AAPL), None);
}

#[test]
fn a_resting_orders_levels_wait_with_it_and_attach_to_the_position_its_fill_leaves_on_its_side() {
    let (mut h, a, _) = funded();
    let limit = OrderKind::Limit { price: f("99000") };
    let events = order_with(&mut h, &a, BTC, Side::Buy, "1", limit, false, levels(Some("101000"), Some("97000")));
    assert!(!events.iter().any(|e| matches!(e, Event::PositionLevelsSet { .. })), "{events:?}");
    // A cancel-all takes the order and its levels with it.
    let nonce = h.engine.next_request_nonce(a.account);
    let events = h.cmd(cancel_all(&a, a.account, nonce));
    assert!(matches!(events.as_slice(), [Event::OrderCancelled { .. }]), "{events:?}");
    order_with(&mut h, &a, BTC, Side::Buy, "1", limit, false, levels(Some("101000"), Some("97000")));
    let events = h.price(BTC, "98900");
    assert_eq!(set_event(&events), levels(Some("101000"), Some("97000")));
    assert_eq!(held(&h, &a, BTC), Some(levels(Some("101000"), Some("97000"))));
    // A sell limit naming levels that only reduces the long drops them, and the long keeps its own.
    let sell = OrderKind::Limit { price: f("100000") };
    order_with(&mut h, &a, BTC, Side::Sell, "0.5", sell, false, levels(Some("99000"), Some("101000")));
    let events = h.price(BTC, "100100");
    assert_eq!(removed(&events), LevelsRemoved::NotOpened);
    assert_eq!(held(&h, &a, BTC), Some(levels(Some("101000"), Some("97000"))));
}

#[test]
fn levels_an_entrys_fill_attaches_fire_at_once_when_its_closing_price_is_past_them() {
    let (mut h, a, _) = funded();
    let limit = OrderKind::Limit { price: f("100000") };
    order_with(&mut h, &a, BTC, Side::Buy, "1", limit, false, levels(None, Some("99700")));
    // A price gapping to 99000 fills the buy limit well within it, and the long's closing price is
    // already past its stop loss.
    let events = super::past_the_move_window(&mut h, "99000");
    assert_eq!(set_event(&events), levels(None, Some("99700")));
    assert_eq!(triggered(&events).map(|(kind, _)| kind), Some(LevelKind::StopLoss));
    assert_eq!(removed(&events), LevelsRemoved::StopLoss);
    assert!(h.engine.accounts()[&a.account].positions.is_empty());
}
