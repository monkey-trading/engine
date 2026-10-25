//! Margin after every change: dealing while a market has no fresh price, and close-outs after
//! financing, margin-table and close-out-level changes, end to end through `Engine::apply`.

mod common;

use common::events::{fills, rejection};
use common::{AAPL, BTC, ETH, Harness, acct, f, market, ny, operator, params, us_equities};
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced};
use engine_core::event::{Event, Reject};
use engine_core::params::ProtocolParams;
use engine_core::price::{PriceError, SourceQuote};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Millis, Side};
use engine_core::{Engine, Fixed};

#[test]
fn a_market_with_no_fresh_price_allows_no_dealing_opening_or_closing() {
    let mut h = latest();
    let a = acct(1);
    h.deposit(a, "10000");
    h.price(ETH, "4000");
    h.market_order(a, ETH, Side::Buy, "1", "4000");
    // ETH's quotes age past its 1s limit while BTC stays priced.
    h.advance(1_001);
    h.price(BTC, "100000");

    let stale = Reject::NoPrice { error: PriceError::Stale { fresh: 0, needed: 2 } };
    assert_eq!(rejection(&h.market_order(a, ETH, Side::Buy, "1", "4000")), &stale);
    assert_eq!(rejection(&h.market_order(a, ETH, Side::Sell, "1", "4000")), &stale);
    assert_eq!(rejection(&h.cmd(reduce_only(a, ETH, Side::Sell, "1", "4000"))), &stale);
    assert_eq!(h.engine.accounts()[&a].positions[&ETH].qty, f("1"));
}

#[test]
fn an_account_holding_a_stale_market_trades_others_and_withdraws_at_its_last_mark() {
    let mut h = latest();
    let a = acct(1);
    h.deposit(a, "1000");
    h.price(ETH, "4000");
    h.market_order(a, ETH, Side::Buy, "1", "4000");
    h.price(ETH, "3800");
    h.advance(1_001);
    h.price(BTC, "100000");

    // At ETH's last mark of 3,800, equity is 800 against 380 of margin: 420 is free. At the 4,000
    // entry it would be 600.
    let ev = h.cmd(Command::Withdraw { account: a, amount: f("420.000000001"), auth: None });
    assert_eq!(
        ev,
        [Event::WithdrawalRejected {
            account: a,
            amount: f("420.000000001"),
            reason: Reject::InsufficientFunds { available: f("420") }
        }]
    );
    let ev = h.cmd(Command::Withdraw { account: a, amount: f("120"), auth: None });
    assert_eq!(ev, [Event::WithdrawalApproved { account: a, amount: f("120"), nonce: 1 }]);

    // The 300 left free margins exactly 3,000 of BTC at 10%.
    let ev = h.market_order(a, BTC, Side::Buy, "0.0300001", "100000");
    assert_eq!(rejection(&ev), &Reject::InsufficientMargin { equity: f("680"), required: f("680.001") });
    assert_eq!(fills(&h.market_order(a, BTC, Side::Buy, "0.03", "100000")).len(), 1);
    h.assert_conserved(f("120"));
}

#[test]
fn financing_that_takes_an_account_below_the_close_out_level_closes_it_out() {
    let a = acct(1);
    // 0.1 BTC bought at 100,000 and marked at 94,740: equity 474 against a close-out level of 473.7
    // (50% of 947.4). A day's financing at 250bps a year on 9,474 is 0.64890411, rounded up.
    let setup = |h: &mut Harness| {
        h.deposit(a, "1000");
        h.price(BTC, "100000");
        h.market_order(a, BTC, Side::Buy, "0.1", "100000");
        h.price(BTC, "94740");
        // A fresh price half a second before the 22:00 London rollover, then a tick just after it.
        h.advance_to(ny(2026, 10, 20, 16, 59) + 59_500);
        h.price(BTC, "94740");
        h.advance(600);
        h.cmd(Command::Tick)
    };

    let mut h = latest();
    let ev = setup(&mut h);
    let financing = Fixed::from_raw(648_904_110);
    assert!(ev.contains(&Event::Financing { account: a, market: BTC, amount: financing }), "{ev:?}");
    let equity = f("474") - financing;
    assert!(ev.contains(&Event::CloseOut { account: a, equity, margin: f("947.4") }), "{ev:?}");
    assert_eq!(liquidations(&ev), [(BTC, f("0.1"))]);
    assert!(h.engine.accounts()[&a].positions.is_empty());
    h.assert_conserved(Fixed::ZERO);

    // Before `V6` the rollover charges the same financing but closes nothing until BTC next prices.
    let mut h = Harness::v5();
    let ev = setup(&mut h);
    assert!(ev.contains(&Event::Financing { account: a, market: BTC, amount: financing }), "{ev:?}");
    assert!(!ev.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{ev:?}");
    assert_eq!(liquidations(&h.price(BTC, "94740")), [(BTC, f("0.1"))]);
}

#[test]
fn a_margin_table_change_closes_tradeable_losers_largest_first_and_leaves_a_closed_market() {
    let a = acct(1);
    // AAPL, ETH and BTC longs. At 16:30 New York AAPL has closed with its loser marked at entry, ETH
    // is down 100 and BTC down 10: equity 5,890 against 5,089 of margin.
    let setup = |h: &mut Harness| {
        h.deposit(a, "6000");
        h.price(AAPL, "230");
        h.price(ETH, "4000");
        h.price(BTC, "100000");
        h.market_order(a, AAPL, Side::Buy, "100", "230");
        h.market_order(a, ETH, Side::Buy, "1", "4000");
        h.market_order(a, BTC, Side::Buy, "0.01", "100000");
        h.advance_to(ny(2026, 10, 20, 16, 30));
        h.price(ETH, "3900");
        h.price(BTC, "99000");
        // AAPL's margin rises from 20% to 100%: 23,489 of margin.
        let spec = market(AAPL, "AAPL", AssetClass::Equity, us_equities(), 10_000);
        h.cmd(Command::Admin(AdminCommand::UpsertMarket(spec)))
    };

    let mut h = latest();
    let ev = setup(&mut h);
    assert!(ev.contains(&Event::CloseOut { account: a, equity: f("5890"), margin: f("23489") }), "{ev:?}");
    // The larger loser first; still below the level, but AAPL cannot trade while closed.
    assert_eq!(liquidations(&ev), [(ETH, f("1")), (BTC, f("0.01"))]);
    assert!(ev.contains(&Event::AdminApplied));
    assert_eq!(h.engine.accounts()[&a].positions[&AAPL].qty, f("100"));
    // AAPL's first price after the open closes it out.
    h.advance_to(ny(2026, 10, 21, 9, 30));
    assert_eq!(liquidations(&h.price(AAPL, "230")), [(AAPL, f("100"))]);
    assert!(h.engine.accounts()[&a].positions.is_empty());
    h.assert_conserved(Fixed::ZERO);

    // Before `V6` a listing change closes nothing.
    let mut h = Harness::v5();
    let ev = setup(&mut h);
    assert!(liquidations(&ev).is_empty(), "{ev:?}");
    assert_eq!(h.engine.accounts()[&a].positions.len(), 3);
}

#[test]
fn a_raised_close_out_level_closes_fresh_markets_and_leaves_a_stale_one_until_it_prices() {
    let a = acct(1);
    // ETH down 2,000 and stale, BTC down 10 and fresh: equity 2,990 against 3,899 of margin, above
    // the 50% level and below a 100% one.
    let setup = |h: &mut Harness| {
        h.deposit(a, "5000");
        h.price(ETH, "4000");
        h.price(BTC, "100000");
        h.market_order(a, ETH, Side::Buy, "10", "4000");
        h.market_order(a, BTC, Side::Buy, "0.01", "100000");
        h.price(ETH, "3800");
        h.advance(1_001);
        h.price(BTC, "99000");
        h.cmd(Command::Admin(AdminCommand::SetParams(ProtocolParams { closeout_level_bps: 10_000, ..params() })))
    };

    let mut h = latest();
    let ev = setup(&mut h);
    assert!(ev.contains(&Event::CloseOut { account: a, equity: f("2990"), margin: f("3899") }), "{ev:?}");
    assert_eq!(liquidations(&ev), [(BTC, f("0.01"))]);
    assert_eq!(h.engine.accounts()[&a].positions[&ETH].qty, f("10"));
    // ETH's next price closes it out.
    assert_eq!(liquidations(&h.price(ETH, "3800")), [(ETH, f("10"))]);
    h.assert_conserved(Fixed::ZERO);

    // Before `V6` a parameter change closes nothing.
    let mut h = Harness::v5();
    assert!(liquidations(&setup(&mut h)).is_empty());
    assert_eq!(h.engine.accounts()[&a].positions.len(), 2);
}

#[test]
fn a_close_out_never_fills_a_market_after_its_session_closes() {
    let a = acct(1);
    // AAPL is bought and marked down to a 1,500 loss just before its 16:00 close; 50ms after the
    // close, BTC's fall to 88,000 takes the account below its close-out level, with AAPL's quotes
    // still fresh and its open flag not yet refreshed.
    let setup = |h: &mut Harness| {
        h.deposit(a, "4000");
        h.price(BTC, "100000");
        h.market_order(a, BTC, Side::Buy, "0.1", "100000");
        h.advance_to(ny(2026, 10, 20, 15, 59) + 59_800);
        h.price(AAPL, "230");
        h.market_order(a, AAPL, Side::Buy, "50", "230");
        h.advance(100);
        h.price(AAPL, "200");
        h.advance(150);
        h.price(BTC, "88000")
    };

    // From `V6` the largest loser, AAPL, waits for its next session; BTC is closed instead.
    let mut h = latest();
    let ev = setup(&mut h);
    assert!(ev.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{ev:?}");
    assert_eq!(liquidations(&ev), [(BTC, f("0.1"))]);
    assert_eq!(h.engine.accounts()[&a].positions[&AAPL].qty, f("50"));

    // Before `V6` the close-out filled AAPL after its close.
    let mut h = Harness::v5();
    assert_eq!(liquidations(&setup(&mut h)), [(AAPL, f("50"))]);
}

#[test]
fn a_close_out_judges_and_ranks_an_account_at_the_index_it_fills_at() {
    let a = acct(1);
    // The financing test's position, held just above the level at its 94,740 mark. Quotes stamped
    // ahead of the clock are stored without moving the mark, and count once their stamps pass.
    let held = |h: &mut Harness| {
        h.deposit(a, "1000");
        h.price(BTC, "100000");
        h.market_order(a, BTC, Side::Buy, "0.1", "100000");
        h.price(BTC, "94740");
    };

    // At the rollover the mark, less a day's financing, is below the level, but the index of
    // 96,000 the close-out would fill at leaves equity of 599.35 against 480: nothing closes.
    let mut h = latest();
    held(&mut h);
    h.advance_to(ny(2026, 10, 20, 16, 59) + 59_500);
    h.cmd(quotes_ahead(BTC, "96000", h.ts, 300));
    h.advance(600);
    let ev = h.cmd(Command::Tick);
    assert!(ev.iter().any(|e| matches!(e, Event::Financing { .. })), "{ev:?}");
    assert!(liquidations(&ev).is_empty(), "{ev:?}");

    // The mark is above the level, but at the index of 93,000 equity is 300 against 465: new
    // parameters close the account out.
    let mut h = latest();
    held(&mut h);
    h.cmd(quotes_ahead(BTC, "93000", h.ts, 300));
    h.advance(400);
    let ev = h.cmd(Command::Admin(AdminCommand::SetParams(params())));
    assert!(ev.contains(&Event::CloseOut { account: a, equity: f("300"), margin: f("930") }), "{ev:?}");
    assert_eq!(liquidations(&ev), [(BTC, f("0.1"))]);

    // At the marks ETH is the larger loser, at the indexes BTC is: BTC closes first, and enough.
    let mut h = latest();
    h.deposit(a, "2000");
    h.price(BTC, "100000");
    h.price(ETH, "4000");
    h.market_order(a, BTC, Side::Buy, "0.1", "100000");
    h.market_order(a, ETH, Side::Buy, "2", "4000");
    h.price(BTC, "99000");
    h.price(ETH, "3900");
    h.cmd(quotes_ahead(BTC, "96000", h.ts, 300));
    h.advance(400);
    let ev = h.cmd(Command::Admin(AdminCommand::SetParams(ProtocolParams { closeout_level_bps: 10_000, ..params() })));
    assert_eq!(liquidations(&ev), [(BTC, f("0.1"))]);
    h.assert_conserved(Fixed::ZERO);
}

#[test]
fn a_listing_before_any_parameters_is_applied_without_a_close_out_check() {
    let mut engine = Engine::new();
    let btc = market(BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    let cmds = [
        AdminCommand::SetClientAuth(ClientAuth::Unsigned),
        AdminCommand::SetOperator(operator()),
        AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST },
        AdminCommand::UpsertMarket(btc),
    ];
    for (seq, cmd) in (0..).zip(cmds) {
        let ev = engine.apply(&Sequenced { seq, ts: 0, cmd: Command::Admin(cmd) }).unwrap();
        assert!(ev.contains(&Event::AdminApplied), "{ev:?}");
    }
    assert!(engine.markets().contains_key(&BTC));
}

// ── Helpers ──

/// Genesis under `V6`, with unsigned requests.
fn latest() -> Harness {
    Harness::v6()
}

/// The market and qty of each liquidation fill among `events`, in order.
fn liquidations(events: &[Event]) -> Vec<(MarketId, Fixed)> {
    fills(events).iter().filter(|fill| fill.liquidation).map(|fill| (fill.market, fill.qty)).collect()
}

/// Two venues quoting `mid` for `market`, stamped `lead_ms` after `now`.
fn quotes_ahead(market: MarketId, mid: &str, now: Millis, lead_ms: Millis) -> Command {
    let quote = |source: &str| SourceQuote { source: source.into(), mid: f(mid), ts: now + lead_ms, contract: None };
    Command::Prices { market, quotes: vec![quote("venue0"), quote("venue1")] }
}

/// A reduce-only market order quoted at `quoted`.
fn reduce_only(account: AccountId, market: MarketId, side: Side, qty: &str, quoted: &str) -> Command {
    Command::PlaceOrder(OrderRequest {
        account,
        client_id: 1,
        market,
        side,
        qty: f(qty),
        kind: OrderKind::Market { quoted: f(quoted), max_slippage_bps: None },
        reduce_only: true,
        auth: None,
    })
}
