//! An order's preview (`Engine::preview_order`) and the quotes (`Engine::quote`) under `V84`: the same
//! pricing and checks a fill runs, so each agrees with an order of its size, refused or filled; under
//! portfolio margin, admission can depend on size either way, a small hedging add refused where a larger one
//! fills.

use super::closes::{hedged_on, markets_filled};
use super::*;
use common::batch::{close_all, closes};
use common::close_spread::reduce;

#[test]
fn a_preview_names_the_fill_and_the_requirements_it_leaves_and_agrees_with_the_order() {
    let (mut h, a) = hedged_on("60000");
    // Closing BTC alone leaves ETH's $100k of bands, which the account cannot meet.
    let preview = h.engine.preview_order(a.account, BTC, Side::Sell, f("10"), true).expect("credited");
    let fill = preview.fill.as_ref().expect("the close prices");
    assert_eq!((fill.qty, fill.increases), (f("10"), false));
    assert_eq!(preview.initial_margin, f("100000"));
    assert_eq!(preview.maintenance_margin, f("50000"));
    let refused = preview.refused.clone().expect("refused for margin");
    let events = reduce(&mut h, &a, BTC, Side::Sell, "10", "100000");
    assert_eq!(rejection(&events), &refused);
    // Trimming ETH lowers the requirement: the preview admits it, and the fill is the preview's.
    let preview = h.engine.preview_order(a.account, ETH, Side::Buy, f("50"), false).expect("credited");
    assert_eq!(preview.refused, None);
    let price = preview.fill.as_ref().expect("prices").price;
    let fill = only_fill(&reduce(&mut h, &a, ETH, Side::Buy, "50", "4000")).clone();
    assert_eq!(fill.price, price);
    let (equity, initial) = equity_and_initial(&h, &a);
    assert_eq!((equity, initial), (preview.equity, preview.initial_margin));
}

#[test]
fn a_preview_of_a_close_whose_fee_takes_more_than_the_equity_shows_the_write_off() {
    // BTC bought at 40,000 is worth 100,000 and ETH shorted at 4,000 is at 10,410: about $59k of equity. A
    // close-all closes ETH; closing BTC then realises $600k and pays its $60k fee, which leaves the account
    // flat with a negative balance the house writes off, at no equity.
    let mut h = genesis_with_grid(V84, pairs(100), 750, 0);
    let a = client(&mut h, 1, "100000");
    h.price(BTC, "40000");
    h.price(ETH, "4000");
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "10", "40000"));
    only_fill(&trade(&mut h, &a, ETH, Side::Sell, "100", "4000"));
    assert!(fills(&h.price(BTC, "100000")).is_empty());
    assert!(fills(&h.price(ETH, "10410")).is_empty());
    let nonce = h.engine.next_request_nonce(a.account);
    let events = h.cmd(close_all(&h, &a, a.account, nonce, closes(&[ETH], "10410"), Some(10_000)));
    assert_eq!(markets_filled(&events), vec![(ETH, false)], "{events:?}");
    let preview = h.engine.preview_order(a.account, BTC, Side::Sell, f("10"), true).expect("credited");
    assert_eq!((preview.refused.clone(), preview.equity), (None, Fixed::ZERO), "{preview:?}");
    let events = reduce(&mut h, &a, BTC, Side::Sell, "10", "100000");
    assert!(events.iter().any(|e| matches!(e, Event::BadDebt { .. })), "{events:?}");
    let (equity, initial) = equity_and_initial(&h, &a);
    assert_eq!((equity, initial), (preview.equity, preview.initial_margin));
}

#[test]
fn a_reduce_only_preview_fills_at_most_the_position() {
    let (h, a) = hedged_on("60000");
    let preview = h.engine.preview_order(a.account, ETH, Side::Buy, f("400"), true).expect("credited");
    assert_eq!(preview.fill.expect("prices").qty, f("250"));
}

#[test]
fn a_preview_that_does_not_price_leaves_the_account_as_it_is() {
    let (h, a) = hedged_on("60000");
    let preview = h.engine.preview_order(a.account, BTC, Side::Buy, Fixed::ZERO, false).expect("credited");
    assert!(preview.fill.is_none());
    assert!(matches!(preview.refused, Some(Reject::InvalidOrder { .. })), "{preview:?}");
    let (equity, initial) = equity_and_initial(&h, &a);
    assert_eq!((preview.equity, preview.initial_margin), (equity, initial));
    assert_eq!(h.engine.preview_order(common::acct(9), BTC, Side::Buy, f("1"), false), None);
}

#[test]
fn a_small_hedging_add_is_refused_and_a_larger_one_fills_each_as_previewed() {
    // BTC alone, down 7%: about $80k against $93k of initial requirement, above its close-out level. A short
    // of ETH offsets BTC as it grows to BTC's loading-weighted size, 186 ETH, and past it adds risk of its
    // own: about $93k less $438 an ETH below that size, and $400 an ETH less $63k above it.
    let verdicts: Vec<(&str, bool)> = ["10", "50", "186", "300", "400", "1000"]
        .into_iter()
        .map(|qty| {
            let (mut h, a) = btc_below_its_initial_requirement();
            let preview = h.engine.preview_order(a.account, ETH, Side::Sell, f(qty), false).expect("credited");
            let quote = h.engine.quote(a.account, ETH, Side::Sell, f(qty)).expect("prices");
            assert_eq!(quote.declined, preview.refused, "{qty}");
            let events = trade(&mut h, &a, ETH, Side::Sell, qty, "4000");
            match &preview.refused {
                Some(reason) => assert_eq!(rejection(&events), reason, "{qty}"),
                None => {
                    assert_eq!(markets_filled(&events), vec![(ETH, false)], "{qty}");
                    assert_eq!(only_fill(&events).price, preview.fill.as_ref().expect("prices").price);
                }
            }
            (qty, preview.refused.is_none())
        })
        .collect();
    let expected = [("10", false), ("50", true), ("186", true), ("300", true), ("400", false), ("1000", false)];
    assert_eq!(verdicts, expected);
}

#[test]
fn the_quote_declines_a_close_an_order_could_not_make() {
    let (h, a) = hedged_on("60000");
    let quote = h.engine.quote(a.account, BTC, Side::Sell, f("10")).expect("prices");
    assert!(!quote.increases);
    assert!(matches!(quote.declined, Some(Reject::InsufficientMargin { .. })), "{quote:?}");
    let trimmed = h.engine.quote(a.account, ETH, Side::Buy, f("50")).expect("prices");
    assert_eq!(trimmed.declined, None);
}

// ── Helpers ──

/// $1M of BTC long on $150k, BTC down to 93,000: below its initial requirement, above its close-out level.
fn btc_below_its_initial_requirement() -> (Harness, Client) {
    let (mut h, a) = long_btc(V84, pairs(100), "150000");
    assert!(fills(&h.price(BTC, "93000")).is_empty());
    let (equity, initial) = equity_and_initial(&h, &a);
    assert!(equity < initial, "{equity} against {initial}");
    (h, a)
}
