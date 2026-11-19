//! The check once a command's own fills are done (#913, D7, D14): every account with a fill but a
//! close-out's is checked for close-out after the command's last fill, whatever markets it still holds
//! and whichever way its requirement moved, and never between the entries of a close-all.

use super::*;
use common::batch::{close_all, closes};
use common::close_spread::{reduce, rest};
use common::portfolio::group;
use common::portfolio::member;
use common::quote;
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::corporate::MAX_CATCH_UP_BOUNDARIES;
use engine_core::types::Millis;

const DAY: Millis = 86_400_000;

/// At a 1% floor share, ETH up 15%: the pair holder has about $60k against the close-out level of a
/// $47k portfolio requirement, but would need half of ETH's $115k of bands alone.
fn hedged_near_its_level() -> (Harness, Client) {
    let mut h = genesis_with_grid(V81, pairs(100), 750, 0);
    let a = client(&mut h, 1, "210000");
    open_pair(&mut h, &a);
    assert!(fills(&h.price(ETH, "4600")).is_empty());
    (h, a)
}

#[test]
fn a_triggered_stop_closing_one_leg_is_followed_by_the_accounts_close_out() {
    let (mut h, a) = hedged_near_its_level();
    assert!(
        rest(&mut h, &a, BTC, Side::Sell, "10", OrderKind::Stop { price: f("99500") })
            .iter()
            .any(|e| matches!(e, Event::OrderAccepted { .. }))
    );
    // BTC at 99,000 fires the stop: the long closes and ETH alone needs its bands. The price's check
    // visits BTC's holders, which the account no longer is; the check after the fills closes ETH.
    let events = h.price(BTC, "99000");
    let closed: Vec<(MarketId, bool)> =
        fills(&events).into_iter().map(|fill| (fill.market, fill.liquidation)).collect();
    assert_eq!(closed, vec![(BTC, false), (ETH, true)], "{events:?}");
    assert!(h.engine.accounts()[&a.account].positions.is_empty());
}

#[test]
fn a_resting_order_filling_as_it_is_placed_is_followed_by_the_accounts_close_out() {
    let (mut h, a) = hedged_near_its_level();
    h.price(ETH, "4650");
    // A sell limit under the mid fills as it rests, leaving ETH alone below its level.
    let events = rest(&mut h, &a, BTC, Side::Sell, "10", OrderKind::Limit { price: f("99000") });
    let closed: Vec<(MarketId, bool)> =
        fills(&events).into_iter().map(|fill| (fill.market, fill.liquidation)).collect();
    assert_eq!(closed, vec![(BTC, false), (ETH, true)], "{events:?}");
}

#[test]
fn a_close_whose_spread_and_fee_take_more_than_it_releases_is_followed_by_the_accounts_close_out() {
    // BTC and ETH in no group, under a grid of oil and gold. BTC bought at 40,000 is worth 100,000: its
    // $600k gain pays a $60k fee when it closes, more than the $50k half of its bands releases.
    let commodities = PortfolioGrid {
        groups: vec![group("commodities", 5_000, vec![member(OIL, "1", 20), member(GOLD, "0.9", 20)])],
    };
    let mut h = genesis_with_grid(V81, commodities, 750, 0);
    let a = client(&mut h, 1, "100000");
    h.price(BTC, "40000");
    h.price(ETH, "4000");
    only_fill(&trade(&mut h, &a, BTC, Side::Buy, "10", "40000"));
    only_fill(&trade(&mut h, &a, ETH, Side::Sell, "100", "4000"));
    h.price(BTC, "100000");
    // ETH rises until the account is $5k above its level: each dollar on ETH costs it $100 of equity and
    // $5 more of level.
    let (equity, level) = standing(&h, &a);
    let eth = f("4000") + (equity - level - f("5000")).div_int(105);
    assert!(fills(&h.price(ETH, &eth.to_string())).is_empty());
    let (equity, level) = standing(&h, &a);
    assert!(equity > level && equity - level < f("10000"), "{equity} against {level}");
    let events = reduce(&mut h, &a, BTC, Side::Sell, "10", "100000");
    let closed: Vec<(MarketId, bool)> =
        fills(&events).into_iter().map(|fill| (fill.market, fill.liquidation)).collect();
    assert_eq!(closed, vec![(BTC, false), (ETH, true)], "{events:?}");
    assert!(fills(&events)[0].profit_fee > f("50000"));
}

#[test]
fn a_close_all_fills_every_entry_before_the_check() {
    let (mut h, a) = hedged_near_its_level();
    h.price(ETH, "4650");
    // Below its level once BTC closes, the account still closes ETH as the second entry.
    let nonce = h.engine.next_request_nonce(a.account);
    let events = h.cmd(close_all(&h, &a, a.account, nonce, closes(&[BTC, ETH], "100000"), Some(10_000)));
    let closed: Vec<(MarketId, bool)> =
        fills(&events).into_iter().map(|fill| (fill.market, fill.liquidation)).collect();
    assert_eq!(closed, vec![(BTC, false), (ETH, false)], "{events:?}");
    assert!(!events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
}

#[test]
fn a_close_all_whose_second_entry_is_refused_is_followed_by_the_accounts_close_out() {
    let (mut h, a) = hedged_near_its_level();
    h.price(ETH, "4650");
    // AAPL holds nothing to close, so its entry is refused; the check after the last entry closes ETH.
    let nonce = h.engine.next_request_nonce(a.account);
    let events = h.cmd(close_all(&h, &a, a.account, nonce, closes(&[BTC, AAPL], "100000"), Some(10_000)));
    let refused = events.iter().position(|e| matches!(e, Event::OrderRejected { client_id: 1, .. }));
    let announced = events.iter().position(|e| matches!(e, Event::CloseOut { .. }));
    assert!(refused.is_some() && refused < announced, "{events:?}");
    let closed: Vec<(MarketId, bool)> =
        fills(&events).into_iter().map(|fill| (fill.market, fill.liquidation)).collect();
    assert_eq!(closed, vec![(BTC, false), (ETH, true)], "{events:?}");
}

#[test]
fn an_admin_commands_own_fill_is_followed_by_the_accounts_close_out_its_outcome_last() {
    let (mut h, a) = hedged_near_its_level();
    h.price(ETH, "4650");
    // BTC's index is the median of three venues, 100,000, which leaves a sell limit at 100,005 resting;
    // every quote is within 10bp of it, so no stress level rises.
    let quotes = |mids: &[(usize, &str)], ts| {
        mids.iter().map(|(venue, mid)| quote(&format!("venue{venue}"), mid, ts)).collect::<Vec<_>>()
    };
    h.cmd(Command::Prices { market: BTC, quotes: quotes(&[(0, "100000"), (1, "99995"), (2, "100090")], h.ts) });
    let events = rest(&mut h, &a, BTC, Side::Sell, "10", OrderKind::Limit { price: f("100005") });
    assert!(fills(&events).is_empty(), "{events:?}");
    h.advance(900);
    h.cmd(Command::Prices { market: BTC, quotes: quotes(&[(1, "99995"), (2, "100090")], h.ts) });
    assert!(fills(&h.price(ETH, "4650")).is_empty());
    // The first venue's quote ages out before the operator's next command, whose markets' processing
    // lifts the index to 100,042.5 and fills the limit; the check after the command's fills closes ETH
    // out, and the command's outcome stays its last event.
    h.advance(200);
    let events = h.operate(AdminCommand::SetReduceOnly { market: Some(AAPL), reduce_only: false });
    let closed: Vec<(MarketId, bool)> =
        fills(&events).into_iter().map(|fill| (fill.market, fill.liquidation)).collect();
    assert_eq!(closed, vec![(BTC, false), (ETH, true)], "{events:?}");
    assert_eq!(events.last(), Some(&Event::AdminApplied), "{events:?}");
}

#[test]
fn a_deferred_catch_ups_own_fill_is_checked_before_the_command_ends() {
    let (mut h, a) = hedged_near_its_level();
    h.price(ETH, "4650");
    // As above, a quote ages out at the next rollover and lifts BTC's index through a resting sell limit.
    let rollover = h.engine.next_rollover().expect("a rollover is scheduled");
    h.advance_to(rollover - 1_100);
    let quotes = |mids: &[(usize, &str)], ts| {
        mids.iter().map(|(venue, mid)| quote(&format!("venue{venue}"), mid, ts)).collect::<Vec<_>>()
    };
    h.cmd(Command::Prices { market: BTC, quotes: quotes(&[(0, "100000"), (1, "99995"), (2, "100090")], h.ts) });
    assert!(fills(&h.price(ETH, "4650")).is_empty());
    let events = rest(&mut h, &a, BTC, Side::Sell, "10", OrderKind::Limit { price: f("100005") });
    assert!(fills(&events).is_empty(), "{events:?}");
    h.advance(900);
    h.cmd(Command::Prices { market: BTC, quotes: quotes(&[(1, "99995"), (2, "100090")], h.ts) });
    assert!(fills(&h.price(ETH, "4650")).is_empty());
    // A halt past `MAX_CATCH_UP_BOUNDARIES` rollovers: the first fills the limit, and the command is deferred
    // after the last it may walk. The accounts its fills touched are checked before it ends, so none is
    // left for the next command, which a node restored from a snapshot taken here could not see.
    h.advance_to(rollover + (MAX_CATCH_UP_BOUNDARIES as i64 + 1) * DAY + 1);
    let events = h.cmd(Command::Tick);
    assert!(events.iter().any(|e| matches!(e, Event::CatchUpDeferred { .. })), "{events:?}");
    let filled: Vec<(MarketId, bool)> = fills(&events)
        .into_iter()
        .filter(|fill| fill.account == a.account)
        .map(|f| (f.market, f.liquidation))
        .collect();
    assert_eq!(filled.first(), Some(&(BTC, false)), "{events:?}");
    h.cmd(Command::Tick);
}
