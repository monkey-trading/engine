//! Closing one leg of a hedge (#913, D7, D14): an ordinary order whose fill only reduces is checked as new
//! risk when it leaves the initial requirement higher, and refused when equity falls short of it; one that
//! leaves the requirement lower fills whatever the equity. A close-all entry, a triggered stop or limit, a
//! limit that triggers as it rests, and a level are never refused for margin; the check after the command's
//! fills closes the account out if it is below its close-out level. A triggered order that reverses through
//! zero is checked on the slice that adds risk, as before `V84`.

use super::*;
use common::batch::{close_all, closes};
use common::close_spread::{reduce, rest};
use common::levels::{levels, open_with};
use engine_core::command::OrderKind;
use engine_core::event::CancelReason;
use engine_core::levels::LevelKind;
use engine_core::types::MarketId;

#[test]
fn an_order_closing_one_leg_into_a_requirement_it_cannot_meet_is_refused() {
    // On about $60k, ETH's short alone would need its $100k of bands.
    let (mut h, a) = hedged_on("60000");
    for events in
        [reduce(&mut h, &a, BTC, Side::Sell, "10", "100000"), trade(&mut h, &a, BTC, Side::Sell, "10", "100000")]
    {
        let Reject::InsufficientMargin { equity, required } = rejection(&events) else { panic!("{events:?}") };
        assert_eq!(*required, f("100000"), "{events:?}");
        assert!(*equity < *required);
    }
    assert_eq!(h.engine.accounts()[&a.account].position(BTC).qty, f("10"));
    // At `V83` such a close fills: BTC down 19% leaves the pair on about $60k.
    let (mut h, a) = pair(V83, pairs(100), "250000");
    assert!(fills(&h.price(BTC, "81000")).is_empty());
    let (equity, _) = h.equity_and_margin(a.account);
    assert!(equity < f("70000"), "{equity}");
    only_fill(&reduce(&mut h, &a, BTC, Side::Sell, "10", "81000"));
}

#[test]
fn a_close_that_lowers_the_requirement_fills_below_the_initial_requirement() {
    // BTC falls 3.5%: the account is below its initial requirement, above its close-out level.
    let (mut h, a) = hedged_on("60000");
    assert!(fills(&h.price(BTC, "96500")).is_empty());
    let (equity, initial) = equity_and_initial(&h, &a);
    assert!(equity < initial, "{equity} against {initial}");
    // Trimming ETH's larger side to BTC's loading-weighted size lowers the requirement, and fills.
    let before = initial;
    only_fill(&reduce(&mut h, &a, ETH, Side::Buy, "50", "4000"));
    let (_, after) = equity_and_initial(&h, &a);
    assert!(after < before, "{after} against {before}");
    // Closing BTC raises it, and is refused.
    let events = reduce(&mut h, &a, BTC, Side::Sell, "10", "96500");
    assert!(matches!(rejection(&events), Reject::InsufficientMargin { .. }), "{events:?}");
}

#[test]
fn while_a_member_cannot_trade_an_order_closing_the_other_leg_alone_is_refused() {
    // ETH's quotes go stale. Its group takes no initial credit, but closing BTC by an order would still leave
    // ETH alone, $100k of bands on about $60k, which no close-out can close until ETH prices again.
    let (mut h, a) = hedged_on("60000");
    h.advance(2_000);
    assert!(fills(&h.price(BTC, "100000")).is_empty());
    let preview = h.engine.preview_order(a.account, BTC, Side::Sell, f("10"), true).expect("credited");
    let events = reduce(&mut h, &a, BTC, Side::Sell, "10", "100000");
    let Reject::InsufficientMargin { required, .. } = rejection(&events) else { panic!("{events:?}") };
    assert_eq!(*required, f("100000"));
    assert_eq!(preview.refused.as_ref(), Some(rejection(&events)));
    // A close-all of BTC is never refused for margin.
    let nonce = h.engine.next_request_nonce(a.account);
    let events = h.cmd(close_all(&h, &a, a.account, nonce, closes(&[BTC], "100000"), Some(10_000)));
    assert_eq!(markets_filled(&events), vec![(BTC, false)], "{events:?}");
}

#[test]
fn a_one_entry_close_all_closes_a_lone_leg_an_order_could_not() {
    let (mut h, a) = hedged_on("60000");
    let nonce = h.engine.next_request_nonce(a.account);
    let events = h.cmd(close_all(&h, &a, a.account, nonce, closes(&[BTC], "100000"), Some(10_000)));
    assert_eq!(markets_filled(&events), vec![(BTC, false)], "{events:?}");
    // Above its close-out level, the account is not closed out; below its initial requirement, it may add nothing.
    let (equity, initial) = equity_and_initial(&h, &a);
    assert!(equity < initial, "{equity} against {initial}");
    assert_eq!(h.engine.accounts()[&a.account].position(ETH).qty, f("-250"));
}

#[test]
fn a_triggered_stop_on_one_leg_fills_and_leaves_the_account_below_its_initial_requirement() {
    // On about $70k, BTC's 1% fall through the stop leaves about $60k against ETH's $100k of bands.
    let (mut h, a) = hedged_on("70000");
    assert!(accepted(&rest(&mut h, &a, BTC, Side::Sell, "10", OrderKind::Stop { price: f("99500") })));
    let events = h.price(BTC, "99000");
    assert_eq!(markets_filled(&events), vec![(BTC, false)], "{events:?}");
    let (equity, initial) = equity_and_initial(&h, &a);
    assert!(equity < initial, "{equity} against {initial}");
    assert_eq!(h.engine.accounts()[&a.account].position(ETH).qty, f("-250"));
}

#[test]
fn a_take_profit_limit_on_one_leg_fills_and_the_account_is_closed_out_after_it() {
    // On about $40k, BTC alone would be $10k short of half its bands.
    let (mut h, a) = hedged_on("40000");
    assert!(accepted(&rest(&mut h, &a, ETH, Side::Buy, "250", OrderKind::Limit { price: f("3990") })));
    let events = h.price(ETH, "3980");
    assert_eq!(markets_filled(&events), vec![(ETH, false), (BTC, true)], "{events:?}");
    assert!(h.engine.accounts()[&a.account].positions.is_empty());
}

#[test]
fn a_limit_filling_as_it_rests_closes_one_leg_an_order_could_not() {
    let (mut h, a) = hedged_on("60000");
    let events = rest(&mut h, &a, BTC, Side::Sell, "10", OrderKind::Limit { price: f("99000") });
    assert_eq!(markets_filled(&events), vec![(BTC, false)], "{events:?}");
}

#[test]
fn a_stop_loss_level_firing_on_one_leg_fills() {
    let mut h = genesis_with_grid(V84, pairs(100), 750, 0);
    let a = client(&mut h, 1, "150000");
    h.price(BTC, "100000");
    h.price(ETH, "4000");
    only_fill(&open_with(&mut h, &a, BTC, Side::Buy, "10", "100000", levels(None, Some("99500"))));
    only_fill(&trade(&mut h, &a, ETH, Side::Sell, "250", "4000"));
    withdraw_to(&mut h, &a, "60000");
    let events = reduce(&mut h, &a, BTC, Side::Sell, "10", "100000");
    assert!(matches!(rejection(&events), Reject::InsufficientMargin { .. }), "{events:?}");
    let events = h.price(BTC, "99400");
    assert_eq!(markets_filled(&events), vec![(BTC, false)], "{events:?}");
    let fired = events
        .iter()
        .any(|e| matches!(e, Event::PositionLevelTriggered { market: BTC, level: LevelKind::StopLoss, .. }));
    assert!(fired, "{events:?}");
    assert_eq!(h.engine.accounts()[&a.account].position(ETH).qty, f("-250"));
}

#[test]
fn a_triggered_reversal_is_checked_on_the_slice_that_adds_risk() {
    // A stop selling 20 BTC turns the long into a short beside ETH's, which the account cannot carry.
    let (mut h, a) = hedged_on("60000");
    assert!(accepted(&rest(&mut h, &a, BTC, Side::Sell, "20", OrderKind::Stop { price: f("99500") })));
    let events = h.price(BTC, "99000");
    assert!(fills(&events).is_empty(), "{events:?}");
    let cancelled = events.iter().any(|e| matches!(e, Event::OrderCancelled { reason: CancelReason::Unfillable, .. }));
    assert!(cancelled, "{events:?}");
    assert_eq!(h.engine.accounts()[&a.account].position(BTC).qty, f("10"));
}

// ── Helpers ──

/// The pair at `V84` under a 1% floor share, its equity brought to about `equity` by a withdrawal.
pub(crate) fn hedged_on(equity: &str) -> (Harness, Client) {
    let (mut h, a) = pair(V84, pairs(100), "150000");
    withdraw_to(&mut h, &a, equity);
    (h, a)
}

/// Withdraws whole dollars from `client` until its equity is about `equity`.
pub(crate) fn withdraw_to(h: &mut Harness, client: &Client, equity: &str) {
    let (now, _) = h.equity_and_margin(client.account);
    let out = i64::try_from((now - f(equity)).raw() / Fixed::ONE.raw()).expect("in range");
    let events = withdraw(h, client, &out.to_string());
    assert!(events.iter().any(|e| matches!(e, Event::WithdrawalQueued { .. })), "{events:?}");
}

/// Whether `events` accept an order.
pub(crate) fn accepted(events: &[Event]) -> bool {
    events.iter().any(|e| matches!(e, Event::OrderAccepted { .. }))
}

/// The markets `events` fill, in order, each with whether a close-out filled it.
pub(crate) fn markets_filled(events: &[Event]) -> Vec<(MarketId, bool)> {
    fills(events).into_iter().map(|fill| (fill.market, fill.liquidation)).collect()
}
