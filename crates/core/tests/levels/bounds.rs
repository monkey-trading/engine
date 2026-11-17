//! The slots levels use and the minimum distance's bounds.

use crate::common::close_spread::{NOTICE_MS, rest};
use crate::common::levels::{funded, held, levels, open_with, refused, set, set_event};
use crate::common::{BTC, CLOSE_SPREAD_CEILINGS, Client, ETH, Harness, f};
use engine_core::command::{AdminCommand, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::MarketSpec;
use engine_core::types::Side;

#[test]
fn a_markets_levels_use_one_order_slot_that_a_first_level_needs_free() {
    let (mut h, a, _) = funded();
    h.price(ETH, "3000");
    open_with(&mut h, &a, BTC, Side::Buy, "0.1", "100000", levels(None, None));
    open_with(&mut h, &a, ETH, Side::Buy, "1", "3000", levels(None, None));
    // Rest orders until one is refused: that refusal is the account's limit.
    let far = OrderKind::Limit { price: f("50000") };
    let limit = loop {
        let events = rest(&mut h, &a, BTC, Side::Buy, "0.001", far);
        if let Some(Event::OrderRejected { reason, .. }) = events.first() {
            break reason.clone();
        }
    };
    assert!(matches!(limit, Reject::TierOrderLimit { .. } | Reject::TooManyOrders { .. }), "{limit:?}");
    // With every slot taken, a first level on a position is refused as a resting order is.
    assert_eq!(refused(&set(&mut h, &a, BTC, levels(None, Some("98000")))), &limit);
    // Once an order leaves, the BTC long's levels take its slot, and the ETH long's find none.
    let order_id = *h.engine.accounts()[&a.account].orders.keys().next().expect("resting");
    let signer = Client { expires_at: h.ts + 60_000, ..Client::new(1) };
    let events = h.cmd(signer.cancel(h.engine.next_request_nonce(a.account), order_id));
    assert!(matches!(events.as_slice(), [Event::OrderCancelled { .. }]), "{events:?}");
    assert_eq!(set_event(&set(&mut h, &a, BTC, levels(None, Some("98000")))), levels(None, Some("98000")));
    assert_eq!(refused(&set(&mut h, &a, ETH, levels(None, Some("2900")))), &limit);
    // Moving or adding to levels a position holds uses its slot; removing them frees it.
    assert_eq!(
        set_event(&set(&mut h, &a, BTC, levels(Some("103000"), Some("97000")))),
        levels(Some("103000"), Some("97000"))
    );
    assert!(matches!(rest(&mut h, &a, BTC, Side::Buy, "0.001", far).as_slice(), [Event::OrderRejected { .. }]));
    assert_eq!(set_event(&set(&mut h, &a, BTC, levels(None, None))), levels(None, None));
    assert!(matches!(rest(&mut h, &a, BTC, Side::Buy, "0.001", far).as_slice(), [Event::OrderAccepted { .. }]));
}

#[test]
fn activation_gives_each_market_its_ceiling_and_every_listing_after_names_a_distance() {
    let (mut h, _, _) = funded();
    for (market, ceiling) in CLOSE_SPREAD_CEILINGS {
        assert_eq!(h.engine.markets()[&market].spec.min_level_distance_bps, Some(ceiling), "market {market}");
    }
    let unnamed = MarketSpec { min_level_distance_bps: None, ..h.engine.markets()[&BTC].spec.clone() };
    let events = h.operate(AdminCommand::UpsertMarket(unnamed));
    assert!(
        matches!(events.as_slice(), [Event::AdminRejected { reason }] if reason.contains("minimum level distance"))
    );
}

#[test]
fn a_distance_moves_only_by_a_notice_from_its_launch_value_to_twice_it_and_levels_set_stay() {
    let (mut h, a, _) = funded();
    open_with(&mut h, &a, BTC, Side::Buy, "1", "100000", levels(None, Some("99700")));
    let relisting = |h: &Harness, bps: i64| {
        AdminCommand::UpsertMarket(MarketSpec {
            min_level_distance_bps: Some(bps),
            ..h.engine.markets()[&BTC].spec.clone()
        })
    };
    let rejected = |events: &[Event]| matches!(events, [Event::AdminRejected { .. }]);
    // BTC launched at its 20bp ceiling: no relisting moves it without a notice.
    assert!(rejected(&h.operate(relisting(&h, 30))));
    let notice = |bps: i64, effective_at| ListingNotice {
        spread_tiers: None,
        margin_tiers: None,
        markup_bps: None,
        weekend_margin_bps: None,
        price_rules: None,
        min_level_distance_bps: Some(bps),
        effective_at,
    };
    let at = h.ts + NOTICE_MS;
    for outside in [19, 41] {
        assert!(rejected(&h.operate(AdminCommand::AnnounceListing { market: BTC, notice: notice(outside, at) })));
    }
    let events = h.operate(AdminCommand::AnnounceListing { market: BTC, notice: notice(40, at) });
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    assert!(rejected(&h.operate(relisting(&h, 40))), "before the notice's time");
    h.advance_to(at);
    let events = h.operate(relisting(&h, 40));
    assert!(events.contains(&Event::AdminApplied), "{events:?}");
    assert_eq!(h.engine.markets()[&BTC].spec.min_level_distance_bps, Some(40));
    // The stop loss set within the new distance stays.
    assert_eq!(held(&h, &a, BTC), Some(levels(None, Some("99700"))));
}
