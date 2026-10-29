//! `Engine::quote`: the price a market order would get now, from the pricing and risk checks a fill
//! uses, changing nothing.

mod common;

use common::events::{only_fill, rejection};
use common::stress::{BOUNDED, diverge, divergence_only, with_divergence_shares};
use common::{BTC, Harness, UNLISTED, acct, f};
use engine_core::Fixed;
use engine_core::command::AdminCommand;
use engine_core::event::Reject;
use engine_core::params::{ProtocolParams, ToxicityParams, ToxicityTier};
use engine_core::types::{AccountId, MarketId, Side};

/// Quotes, then fills `qty` at that price with no slippage allowed, and returns the fill's price.
fn fill_at_quote(h: &mut Harness, account: AccountId, market: MarketId, side: Side, qty: &str) -> Fixed {
    let quote = h.engine.quote(account, market, side, f(qty)).expect("a quote");
    let events = h.order(
        account,
        market,
        side,
        qty,
        engine_core::command::OrderKind::Market { quoted: quote.price, max_slippage_bps: Some(0) },
    );
    let fill = only_fill(&events);
    assert_eq!((fill.price, fill.spread_bps), (quote.price, quote.spread_bps), "{qty} {side:?}");
    fill.price
}

#[test]
fn a_quote_is_the_price_a_fill_gets_at_every_size_and_side() {
    // Sizes at 100,000 a coin: inside the free band, across the 1m and 5m bands, and beyond.
    for (n, side) in [Side::Buy, Side::Sell].into_iter().enumerate() {
        for (m, qty) in ["0.5", "10", "30", "49", "80"].into_iter().enumerate() {
            let mut h = Harness::v21();
            let a = acct((n * 10 + m + 1) as u8);
            h.deposit(a, "100000000");
            h.price(BTC, "100000");
            fill_at_quote(&mut h, a, BTC, side, qty);
        }
    }
}

#[test]
fn a_quote_includes_stress_and_the_accounts_position() {
    let mut h = divergence_only(5_000, 50);
    let a = acct(1);
    h.deposit(a, "100000000");
    diverge(&mut h, 0);
    let calm = h.engine.quote(a, BOUNDED, Side::Buy, f("1")).unwrap();
    diverge(&mut h, 30);
    assert_eq!(h.engine.markets()[&BOUNDED].stress.level, 1);
    let stressed = h.engine.quote(a, BOUNDED, Side::Buy, f("1")).unwrap();
    assert!(stressed.spread_bps > calm.spread_bps, "{calm:?} {stressed:?}");
    fill_at_quote(&mut h, a, BOUNDED, Side::Buy, "1");
    // Selling out of the long pays no size spread, buying more pays it from the position on.
    let close = h.engine.quote(a, BOUNDED, Side::Sell, f("1")).unwrap();
    assert!(!close.increases);
    assert!(h.engine.quote(a, BOUNDED, Side::Buy, f("1")).unwrap().increases);
    fill_at_quote(&mut h, a, BOUNDED, Side::Sell, "1");
}

#[test]
fn a_quote_includes_the_accounts_toxicity() {
    let mut h = Harness::v21();
    let toxicity = ToxicityParams {
        ewma_weight_bps: 10_000,
        tiers: vec![ToxicityTier { min_score_bps: 20, extra_spread_bps: 3 }],
        ..common::params().toxicity
    };
    h.operator_admin(AdminCommand::SetParams(ProtocolParams { toxicity, ..with_divergence_shares(common::params()) }));
    let (toxic, clean) = (acct(1), acct(2));
    for a in [toxic, clean] {
        h.deposit(a, "10000000");
    }
    h.price(BTC, "100000");
    assert_eq!(h.engine.quote(toxic, BTC, Side::Buy, f("1")).unwrap().spread_bps, 0);
    // A fill the market then moves 1% in the account's favour a horizon later scores far above the tier.
    h.market_order(toxic, BTC, Side::Buy, "1", "100000");
    h.advance(1_000);
    h.price(BTC, "101000");
    let quote = h.engine.quote(toxic, BTC, Side::Buy, f("1")).unwrap();
    assert_eq!(quote.spread_bps, 3);
    assert_eq!(h.engine.quote(clean, BTC, Side::Buy, f("1")).unwrap().spread_bps, 0);
    fill_at_quote(&mut h, toxic, BTC, Side::Buy, "1");
}

#[test]
fn a_quote_at_a_larger_size_never_pays_less_per_unit() {
    let mut h = Harness::v21();
    let a = acct(1);
    h.deposit(a, "100000000");
    h.price(BTC, "100000");
    for side in [Side::Buy, Side::Sell] {
        let prices: Vec<Fixed> =
            (1..=120).map(|n| h.engine.quote(a, BTC, side, Fixed::from_int(n).div_int(2)).unwrap().price).collect();
        // The price worsens for the trader as the size grows: up for a buy, down for a sell.
        assert!(prices.windows(2).all(|w| (w[1] - w[0]).mul_int(side.sign()) >= Fixed::ZERO), "{side:?}");
        assert_ne!(prices[0], prices[119]);
    }
}

#[test]
fn a_quote_changes_nothing() {
    let mut h = Harness::v21();
    let a = acct(1);
    h.deposit(a, "100000000");
    h.price(BTC, "100000");
    let (root, seq, now) = (h.engine.state_root(), h.engine.last_seq(), h.engine.now());
    for qty in ["1", "30", "9999"] {
        let _ = h.engine.quote(a, BTC, Side::Buy, f(qty));
    }
    assert_eq!((h.engine.state_root(), h.engine.last_seq(), h.engine.now()), (root, seq, now));
}

#[test]
fn a_quote_reports_the_margin_and_house_capacity_a_fill_would_hit() {
    let mut h = Harness::v21();
    let (small, whale) = (acct(1), acct(2));
    h.deposit(small, "1000");
    h.deposit(whale, "100000000");
    h.price(BTC, "100000");

    // 0.11 BTC needs 1,100 of margin against 1,000 of equity.
    let quote = h.engine.quote(small, BTC, Side::Buy, f("0.11")).unwrap();
    let declined = quote.declined.expect("declined");
    assert!(matches!(declined, Reject::InsufficientMargin { .. }));
    let events = h.market_order(small, BTC, Side::Buy, "0.11", &quote.price.to_string());
    assert_eq!(rejection(&events), &declined);
    assert_eq!(h.engine.quote(small, BTC, Side::Buy, f("0.1")).unwrap().declined, None);

    // 60m of BTC exceeds the market's 50m house cap.
    let quote = h.engine.quote(whale, BTC, Side::Buy, f("600")).unwrap();
    assert_eq!(quote.declined, Some(Reject::HouseCapacity));
    assert_eq!(h.engine.quote(whale, BTC, Side::Buy, f("400")).unwrap().declined, None);
}

#[test]
fn a_quote_refuses_what_a_fill_would_refuse_before_pricing() {
    let mut h = Harness::v21();
    let a = acct(1);
    h.deposit(a, "100000");
    assert!(matches!(h.engine.quote(a, BTC, Side::Buy, f("1")), Err(Reject::NoPrice { .. })));
    h.price(BTC, "100000");
    assert_eq!(h.engine.quote(a, UNLISTED, Side::Buy, f("1")), Err(Reject::UnknownMarket));
    assert_eq!(
        h.engine.quote(acct(9), BTC, Side::Buy, f("1")),
        Err(Reject::InsufficientFunds { available: Fixed::ZERO })
    );
    for qty in [Fixed::ZERO, f("-1")] {
        assert!(matches!(h.engine.quote(a, BTC, Side::Buy, qty), Err(Reject::InvalidOrder { .. })));
    }
    assert!(matches!(
        h.engine.quote(a, BTC, Side::Buy, Fixed::MAX_INPUT + Fixed::from_raw(1)),
        Err(Reject::InvalidOrder { .. })
    ));
    // US equities are closed on the weekend after the harness start.
    h.price(common::AAPL, "200");
    h.advance(4 * 86_400_000);
    h.cmd(engine_core::command::Command::Tick);
    assert_eq!(h.engine.quote(a, common::AAPL, Side::Buy, f("1")), Err(Reject::MarketClosed));
}

#[test]
fn a_quoter_prices_many_orders_as_single_quotes_do() {
    let mut h = Harness::v21();
    let (a, b) = (acct(1), acct(2));
    h.deposit(a, "1000");
    h.deposit(b, "100000000");
    h.price(BTC, "100000");
    h.market_order(b, BTC, Side::Buy, "300", "100000");
    let quoter = h.engine.quoter();
    for account in [a, b] {
        for (side, qty) in [(Side::Buy, "0.05"), (Side::Buy, "0.11"), (Side::Sell, "5"), (Side::Buy, "400")] {
            assert_eq!(quoter.quote(account, BTC, side, f(qty)), h.engine.quote(account, BTC, side, f(qty)), "{qty}");
        }
    }
}
