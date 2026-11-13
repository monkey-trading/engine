//! Every USD conversion rounds toward the house: what a trader is credited, or may use, rounds down,
//! and what a trader is charged, and the exposure that limits the house, rounds up. Checked against
//! the exact result for a market converted through a USD-quote pair (Germany 40 at EUR-USD, times
//! the index) and a USD-base pair (USD-JPY at its own index, over it), long and short.

mod common;

use common::events::{fills, only_fill, rejection};
use common::{DE40, EURUSD, Harness, USDJPY, acct, de40, f, ny, params};
use engine_core::Fixed;
use engine_core::command::{AdminCommand, Command, OrderKind};
use engine_core::event::{Event, Reject};
use engine_core::fixed::Rounding;
use engine_core::market::{MarketSpec, QuoteCurrency, Tier};
use engine_core::params::ProtocolParams;
use engine_core::session::Schedule;
use engine_core::spread_tier::{SpreadTier, spread_staircase};
use engine_core::types::{AssetClass, MarketId, Side};

#[test]
fn every_conversion_that_moves_cash_or_values_an_account_rounds_toward_the_house() {
    for (market, side) in [(DE40, Side::Buy), (DE40, Side::Sell), (USDJPY, Side::Buy), (USDJPY, Side::Sell)] {
        let case = format!("{market} {side:?}");
        let (mut h, a) = (Harness::v7(), acct(1));
        h.list_fx();
        // Margin at 100% of notional, so the requirement is the USD notional itself. A 5% benchmark
        // over a 2.5% markup: longs pay 7.5% a year and shorts receive 2.5%.
        h.admin(AdminCommand::UpsertMarket(listing(market, 10_000)));
        h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 500 });
        h.deposit(a, "10000000");
        let (entry, mark, qty) =
            if market == DE40 { ("20000.5", "20001.37", "3") } else { ("151.37", "151.83", "1000.3") };
        h.price(EURUSD, "1.083712347");
        h.price(market, entry);
        h.market_order(a, market, side, qty, entry);
        h.price(market, mark);

        // Marking: the trader's PnL rounds down; margin, the clients' PnL in house NAV, and gross
        // exposure round up.
        let state = &h.engine.markets()[&market];
        let (mark, fx) = (state.mark.expect("priced"), state.fx_mark.expect("FX marked"));
        let pos = h.engine.accounts()[&a].positions[&market];
        let (equity, margin) = h.engine.equity_and_margin(&h.engine.accounts()[&a], None);
        assert_rounded(
            equity - h.balance(a),
            exact(market, pos.unrealised(mark, h.engine.protocol_version()), fx),
            Rounding::Down,
            &case,
        );
        assert_rounded(margin, exact(market, pos.qty.abs().mul(mark), fx), Rounding::Up, &case);
        let clients_pnl = mark.mul(state.client_qty) - state.client_cost;
        let nav_pnl = h.engine.house_cash() - h.engine.house_nav();
        assert_rounded(nav_pnl, exact(market, clients_pnl, fx), Rounding::Up, &case);
        let exposure = state.client_qty.abs().mul(mark);
        assert_rounded(h.engine.house_gross_exposure(), exact(market, exposure, fx), Rounding::Up, &case);

        // Financing at the 22:00 London rollover rounds up, whether the account pays or receives.
        h.advance_to(ny(2026, 10, 20, 17, 30));
        let annual_bps = if side == Side::Buy { 750 } else { -250 };
        let charged = h
            .cmd(Command::Tick)
            .into_iter()
            .find_map(|e| match e {
                Event::Financing { market: m, amount, .. } if m == market => Some(amount),
                _ => None,
            })
            .expect("financing charged");
        // The day's amount in the quote currency, rounded up at each step as `V5` does.
        let notional_rounding = if annual_bps >= 0 { Rounding::Up } else { Rounding::Down };
        let notional = pos.qty.abs().mul_rounded(mark, notional_rounding);
        let daily = notional.mul_bps_rounded(annual_bps, Rounding::Up).div_int_rounded(365, Rounding::Up);
        assert_rounded(charged, exact(market, daily, fx), Rounding::Up, &case);

        // Realising rounds down, a gain for the long and a loss for the short.
        h.price(EURUSD, "1.083712347");
        h.price(market, mark.to_string().as_str());
        let close = fills(&h.market_order(a, market, side.opposite(), qty, &mark.to_string()))[0].clone();
        let mut closed = pos;
        let realised = closed.apply(-pos.qty, close.price, h.engine.protocol_version());
        let fx = close.fx_index.expect("converted");
        assert_rounded(close.realised_pnl, exact(market, realised, fx), Rounding::Down, &case);
    }
}

#[test]
fn the_minimum_notional_and_the_size_spread_round_toward_the_house() {
    let (mut h, a) = (Harness::v7(), acct(1));
    h.list_fx();
    h.deposit(a, "10000000");
    let fx = f("1.083712347");
    let euros = QuoteCurrency::Eur { fx_market: EURUSD };
    // One contract at 20,001.37 is 21,675.775... USD: a minimum just above that refuses it, though
    // the notional rounded up would meet it.
    let notional = f("20001.37");
    let minimum = euros.to_usd(notional, Some(fx), Rounding::Up);
    assert!(euros.to_usd(notional, Some(fx), Rounding::Down) < minimum, "the case must need rounding");
    let mut spec = de40(500);
    spec.min_order_notional = minimum;
    h.admin(AdminCommand::UpsertMarket(spec.clone()));
    h.price(EURUSD, "1.083712347");
    h.price(DE40, "20001.37");
    let refused = h.market_order(a, DE40, Side::Buy, "1", "20001.37");
    assert_eq!(rejection(&refused), &Reject::InvalidOrder { reason: "below minimum order notional".into() });

    // Size spread on two contracts bought one at a time, by stops at the mid, which fill at once with
    // no price guard. The slice of notional a fill adds converts outwards, its low end down and its
    // high end up, and its cost converts back up. Thin 100% bands sit at one and at two contracts'
    // notional, so a unit of rounding at either end of a slice changes its cost.
    let band = |contracts: i64| {
        let usd = euros.to_usd(notional.mul_int(contracts), Some(fx), Rounding::Down);
        let start = Fixed::from_int((usd.raw() / Fixed::ONE.raw()) as i64 - 5);
        [SpreadTier::bps(Some(start), 0), SpreadTier::bps(Some(start + f("10")), 10_000)]
    };
    spec.min_order_notional = Fixed::ZERO;
    spec.spread_tiers = [&band(1)[..], &band(2), &[SpreadTier::bps(None, 0)]].concat();
    h.admin(AdminCommand::UpsertMarket(spec.clone()));
    for held in [0, 1] {
        let events = h.order(a, DE40, Side::Buy, "1", OrderKind::Stop { price: f("20001.37") });
        let fill = fills(&events)[0].clone();
        let cost = |low: Rounding, high: Rounding, back: Rounding| {
            let usd = |contracts: i64, round| euros.to_usd(notional.mul_int(contracts), Some(fx), round);
            let slice = spread_staircase(&spec.spread_tiers, usd(held, low), usd(held + 1, high), Rounding::TowardZero);
            euros.from_usd(slice, Some(fx), back)
        };
        let charged = cost(Rounding::Down, Rounding::Up, Rounding::Up);
        assert_eq!(fill.price - fill.index, charged, "holding {held}");
        // Rounding any one step the other way would charge less; from nothing, the low end is exact.
        let (up, down) = (Rounding::Up, Rounding::Down);
        let mut others = vec![cost(down, down, up), cost(down, up, down)];
        if held > 0 {
            others.push(cost(up, up, up));
        }
        assert!(others.iter().all(|other| *other < charged), "holding {held}: {others:?} vs {charged:?}");
    }
}

#[test]
fn the_margin_cap_and_stress_checks_round_toward_the_house_at_their_boundaries() {
    let one = Fixed::from_raw(1);
    let (yen, euros) = (QuoteCurrency::Jpy { fx_market: USDJPY }, QuoteCurrency::Eur { fx_market: EURUSD });

    // Margin: flipping 1,000 USD-JPY long to 1,000 short at 140 realises -10,000 JPY, -71.4285714...
    // USD, and leaves a short needing 20 USD. With one unit less than that, the loss rounded down
    // leaves the account short of it; rounded up it would just meet it.
    let (mut h, a) = (Harness::v7(), acct(1));
    h.list_fx();
    let loss = yen.to_usd(f("-10000"), Some(f("140")), Rounding::Down);
    assert!(yen.to_usd(f("-10000"), Some(f("140")), Rounding::Up) > loss, "the case must need rounding");
    h.deposit(a, &(f("20") - loss - one).to_string());
    h.price(USDJPY, "150");
    h.market_order(a, USDJPY, Side::Buy, "1000", "150");
    // A minute later, so the move raises no stress.
    h.advance(61_000);
    h.price(USDJPY, "140");
    let ev = h.market_order(a, USDJPY, Side::Sell, "2000", "140");
    assert_eq!(rejection(&ev), &Reject::InsufficientMargin { equity: f("20") - one, required: f("20") });

    // House cap: 2,306 Germany 40 at 20,000.37 is just over 49,980,813 USD. With NAV at twice that
    // rounded down, the cap is the rounded-down exposure, which the exposure rounded up exceeds.
    let (mut h, a) = (Harness::v7(), acct(1));
    h.list_fx();
    h.deposit(a, "100000000");
    let (fx, mid) = (f("1.083712347"), f("20000.37"));
    let exposure = |qty: &str, round| euros.to_usd(f(qty).mul(mid), Some(fx), round);
    assert!(exposure("2306", Rounding::Up) > exposure("2306", Rounding::Down), "the case must need rounding");
    let to_nav = exposure("2306", Rounding::Down).mul_int(2) - h.engine.house_nav();
    h.cmd(Command::HouseCapital { amount: to_nav, chain_ref: "0xcap".into() });
    h.price(EURUSD, "1.083712347");
    h.price(DE40, "20000.37");
    assert_eq!(rejection(&h.market_order(a, DE40, Side::Buy, "2306", "20000.37")), &Reject::HouseCapacity);

    // Stress: 901 contracts are 19,528,857.77... USD, exactly 80% of the cap once rounded up and
    // just under it rounded down. With NAV set to put the cap there, the next price raises stress.
    let up = exposure("901", Rounding::Up);
    assert!(up > exposure("901", Rounding::Down), "the case must need rounding");
    let (mut h, a) = (Harness::v7(), acct(1));
    h.list_fx();
    h.deposit(a, "100000000");
    h.price(EURUSD, "1.083712347");
    h.price(DE40, "20000.37");
    h.market_order(a, DE40, Side::Buy, "901", "20000.37");
    // The cap is half of NAV, so NAV is two and a half times the exposure that is 80% of it.
    let to_nav = up.mul_int(5).div_int(2) - h.engine.house_nav();
    h.cmd(Command::HouseCapital { amount: to_nav, chain_ref: "0xcap".into() });
    assert_eq!(h.engine.house_nav().mul_bps(5_000).mul_bps(8_000), up, "NAV must put 80% of the cap there");
    let ev = h.price(DE40, "20000.37");
    assert!(ev.contains(&Event::StressChanged { market: DE40, level: 1 }), "{ev:?}");
}

#[test]
fn a_position_built_from_many_orders_converts_without_overflow() {
    // At a 1bp margin, caps of ten times NAV, and EUR-USD at 0.000000001, 175 of the largest orders
    // at the largest index build 1.75e20 EUR of notional, whose scaled product with the index is
    // past `i128` from the 171st order. EUR-USD then moves to 10, where that notional is 1.75e21
    // USD: in range, though its scaled product with the index is not.
    let (mut h, a) = (Harness::fx(), acct(1));
    h.admin(AdminCommand::SetParams(ProtocolParams { house_total_cap_bps: 100_000, ..params() }));
    let flat = vec![SpreadTier::bps(None, 0)];
    let spec = MarketSpec {
        margin_tiers: vec![Tier { up_to: None, bps: 1 }],
        spread_tiers: flat,
        house_cap_bps: 100_000,
        ..de40(1)
    };
    h.admin(AdminCommand::UpsertMarket(spec));
    for n in 0..18 {
        h.cmd(Command::HouseCapital { amount: Fixed::MAX_INPUT, chain_ref: format!("0xcap{n}") });
    }
    h.deposit(a, "100000000");
    h.price(EURUSD, "0.000000001");
    h.price(DE40, "1000000000");
    for n in 0..175 {
        let fills = fills(&h.market_order(a, DE40, Side::Buy, "1000000000", "1000000000")).len();
        assert_eq!(fills, 1, "order {n}");
    }
    // `Harness::cmd` panics if the engine does; here the account is closed out at the new value.
    let ev = h.price(EURUSD, "10");
    assert!(ev.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{ev:?}");
    assert_eq!(h.engine.markets()[&DE40].fx_mark, Some(f("10")));
}

#[test]
fn conversion_composes_with_the_quote_currency_rounding_toward_the_house() {
    // `V5` rounds quote-currency PnL down and a financing amount up, and conversion rounds each the
    // same way again. Against the exact amounts the prices give, before either rounding, the trader
    // is never credited more, nor charged less, through either pair kind, long or short.
    for (market, side) in [(DE40, Side::Buy), (DE40, Side::Sell), (USDJPY, Side::Buy), (USDJPY, Side::Sell)] {
        let case = format!("{market} {side:?}");
        let (mut h, a) = (Harness::fx(), acct(1));
        // Longs pay 7.5% a year and shorts receive 2.5%.
        h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 500 });
        h.deposit(a, "10000000");
        let (entry, exit, qty) = if market == DE40 {
            ("20000.5", "20001.87", "0.333333333")
        } else {
            ("151.37", "151.83", "1000.333333333")
        };
        let (entry, exit, qty) = (f(entry), f(exit), f(qty));
        h.price(EURUSD, "1.083712347");
        h.price(market, &entry.to_string());
        let entry = only_fill(&h.market_order(a, market, side, &qty.to_string(), &entry.to_string())).price;

        // A day's financing at the entry: qty * entry * rate / 365, then converted.
        h.advance_to(ny(2026, 10, 20, 17, 30));
        let charged = h
            .cmd(Command::Tick)
            .into_iter()
            .find_map(|e| match e {
                Event::Financing { market: m, amount, .. } if m == market => Some(amount),
                _ => None,
            })
            .expect("financing charged");
        let annual_bps: i128 = if side == Side::Buy { 750 } else { -250 };
        let fx = if market == DE40 { f("1.083712347") } else { entry };
        let daily = (qty.raw() * entry.raw() * annual_bps, 1_000_000_000 * 10_000 * 365);
        let exact_charge = exact_usd(market, daily, fx);
        assert!(at_least(charged, exact_charge), "{case}: charged {charged:?}, exact {exact_charge:?}");

        // Closing realises (exit - entry) * qty for a long, the reverse for a short, converted at
        // the FX index then; a gain pays the 10% fee.
        h.price(EURUSD, "1.083712347");
        h.price(market, &exit.to_string());
        let before = h.balance(a);
        let close = only_fill(&h.market_order(a, market, side.opposite(), &qty.to_string(), &exit.to_string())).clone();
        let credited = h.balance(a) - before;
        let sign = if side == Side::Buy { 1 } else { -1 };
        let quote = (sign * (close.price.raw() - entry.raw()) * qty.raw(), 1_000_000_000);
        let (num, den) = exact_usd(market, quote, close.fx_index.expect("converted"));
        let exact_net = if num > 0 { (num * 9, den * 10) } else { (num, den) };
        assert!(num % den != 0, "{case}: the PnL must need rounding");
        assert!(credited.raw() * exact_net.1 <= exact_net.0, "{case}: credited {credited:?}, exact {exact_net:?}");
    }
}

// ── Helpers ──

/// The market listed 24/7 with one margin rate: Germany 40 in euros, or USD-JPY in yen.
fn listing(market: MarketId, margin_bps: i64) -> MarketSpec {
    if market == DE40 {
        return de40(margin_bps);
    }
    let spec = common::market(USDJPY, "USD-JPY", AssetClass::Fx, Schedule::AlwaysOpen, margin_bps);
    MarketSpec { quote_currency: QuoteCurrency::Jpy { fx_market: USDJPY }, ..spec }
}

/// `amount` in `market`'s quote currency, in USD at `fx`, exactly: a numerator and a positive
/// denominator in units of `Fixed`'s smallest step.
fn exact(market: MarketId, amount: Fixed, fx: Fixed) -> (i128, i128) {
    let one = Fixed::ONE.raw();
    if market == DE40 { (amount.raw() * fx.raw(), one) } else { (amount.raw() * one, fx.raw()) }
}

/// Fails unless `got` is the exact value `num / den` rounded `round`; an exact value must match.
fn assert_rounded(got: Fixed, (num, den): (i128, i128), rounding: Rounding, case: &str) {
    let floor = num.div_euclid(den);
    let want = if rounding == Rounding::Up && num.rem_euclid(den) != 0 { floor + 1 } else { floor };
    assert_eq!(got, Fixed::from_raw(want), "{case}: {num}/{den} rounded {rounding:?}");
}

/// A quote-currency amount `num / den` (in units of `Fixed`'s smallest step), in USD at `fx` for
/// `market`, exactly: a numerator and a positive denominator.
fn exact_usd(market: MarketId, (num, den): (i128, i128), fx: Fixed) -> (i128, i128) {
    if market == DE40 { (num * fx.raw(), den * Fixed::ONE.raw()) } else { (num * Fixed::ONE.raw(), den * fx.raw()) }
}

/// Whether `got` is at least the exact value `num / den`.
fn at_least(got: Fixed, (num, den): (i128, i128)) -> bool {
    got.raw() * den >= num
}
