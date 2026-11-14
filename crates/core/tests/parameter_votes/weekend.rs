//! Parameter votes over a market's weekend margin, from `V59`: a voted relisting moves it with no
//! listing notice, to none or to a rate its listing admits; the operator's notice of one waits for
//! rotation to end; the fast path cannot move it, and may raise a margin band past it.

use super::*;
use engine_core::listing_bounds::ListingNotice;

/// `governed`, with `V59` activated by a vote, as rotation requires.
fn weekend_governed() -> Harness {
    let mut h = governed();
    let activate = AdminCommand::ActivateProtocol { version: ProtocolVersion::V59 };
    pass(&mut h, 90, false, activate.clone());
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 90, activate).last(), Some(&Event::AdminApplied));
    h
}

/// AAPL's baseline listing with a weekend margin of `bps`.
fn weekend(h: &Harness, bps: i64) -> AdminCommand {
    let spec = h.engine.baseline_spec(AAPL).expect("listed");
    AdminCommand::UpsertMarket(MarketSpec { weekend_margin_bps: bps, ..spec })
}

#[test]
fn a_voted_relisting_moves_a_weekend_margin_with_no_listing_notice() {
    let mut h = weekend_governed();
    let (set, over) = (weekend(&h, 3_000), weekend(&h, 10_001));
    pass(&mut h, 1, false, set.clone());
    pass(&mut h, 2, false, over.clone());
    h.advance(NOTICE_MS);
    rejected(&apply(&mut h, 2, over), "weekend_margin_bps must be in 0..=10000");
    assert_eq!(apply(&mut h, 1, set).last(), Some(&Event::AdminApplied));
    let aapl = &h.engine.markets()[&AAPL];
    assert_eq!((aapl.spec.weekend_margin_bps, aapl.weekend.is_some()), (3_000, true));
    assert!(h.engine.operator_bounds().markups.is_empty(), "no listing notice was named or spent");
    // A vote takes it away again, and the market keeps no weekend close.
    let none = weekend(&h, 0);
    pass(&mut h, 3, false, none.clone());
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 3, none).last(), Some(&Event::AdminApplied));
    assert_eq!(h.engine.markets()[&AAPL].weekend, None);
}

#[test]
fn the_operators_weekend_notice_waits_for_rotation_to_end_and_the_fast_path_cannot_move_it() {
    let mut h = weekend_governed();
    let notice = ListingNotice {
        spread_tiers: None,
        margin_tiers: None,
        markup_bps: None,
        weekend_margin_bps: Some(3_000),
        effective_at: h.ts + NOTICE_MS,
    };
    let announce = AdminCommand::AnnounceListing { market: AAPL, notice };
    rejected(&h.operate(announce.clone()), "only a passed vote changes this");
    let fast = weekend(&h, 3_000);
    refused(&pass(&mut h, 1, true, fast), "changes another field");
    switch_rotation(&mut h, false);
    assert_eq!(h.operate(announce).last(), Some(&Event::AdminApplied));
}

#[test]
fn a_fast_path_margin_raise_may_pass_the_weekend_rate() {
    let mut h = weekend_governed();
    let set = weekend(&h, 3_000);
    pass(&mut h, 1, false, set.clone());
    h.advance(NOTICE_MS);
    assert_eq!(apply(&mut h, 1, set).last(), Some(&Event::AdminApplied));
    // AAPL's one 20% band raised to 40%, past its 30% weekend rate, as a restriction.
    let raise = margin(&h, AAPL, 4_000);
    let events = pass(&mut h, 2, true, raise);
    assert!(events.iter().any(|event| matches!(event, Event::RestrictionApplied { .. })), "{events:?}");
    assert_eq!(margin_bps(&h, AAPL), one_band(4_000));
}
