//! Rounding in the house's favour, end to end through `Engine::apply`. From `V5` dust adds cannot
//! move an entry in the holder's favour, and closing never realises more than the exact PnL; from
//! `V10` every spread charge and margin requirement rounds up for buyers and sellers alike. Positions
//! valued at the input ceiling stay in range.

mod common;

use common::events::{fills, rejection};
use common::{AAPL, BTC, ETH, Harness, acct, f, market, ny, params};
use engine_core::Fixed;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::{Event, Fill, Reject};
use engine_core::market::MarketSpec;
use engine_core::params::{ProtocolParams, StressLevel};
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::spread_tier::SpreadTier;
use engine_core::types::{AccountId, AssetClass, MarketId, Side};

/// An FX market with 1% margin and no size spread, so every fill is at the mid.
const EURUSD: MarketId = 4;
/// A market whose longs are bought at one unit of precision and then marked at the input ceiling.
const DUSTY: MarketId = 5;
const DUST: &str = "0.000000001";
/// A market with one spread band and a slippage bound of 100%, so a fill takes any spread short of
/// the price.
const WIDE: MarketId = 7;

#[test]
fn dust_buys_on_a_long_at_its_entry_price_realise_nothing_on_close() {
    let (mut h, a) = eurusd(Harness::v5(), "1.1");
    h.market_order(a, EURUSD, Side::Buy, "4500000", "1.1");
    (0..1_000).for_each(|_| assert_eq!(fill(h.market_order(a, EURUSD, Side::Buy, DUST, "1.1")).price, f("1.1")));
    assert_eq!(entry(&h, a), f("1.1"));
    let house = h.engine.house_cash();

    let close = fill(h.market_order(a, EURUSD, Side::Sell, "4500000.000001", "1.1"));
    assert_eq!((close.position, close.realised_pnl), (Fixed::ZERO, Fixed::ZERO));
    assert_eq!((h.balance(a), h.engine.house_cash()), (f("1000000"), house));
}

#[test]
fn dust_sells_on_a_short_at_its_entry_price_realise_nothing_on_close() {
    let (mut h, a) = eurusd(Harness::v5(), "1.1");
    h.market_order(a, EURUSD, Side::Sell, "4500000", "1.1");
    (0..1_000).for_each(|_| assert_eq!(fill(h.market_order(a, EURUSD, Side::Sell, DUST, "1.1")).price, f("1.1")));
    assert_eq!(entry(&h, a), f("1.1"));
    let house = h.engine.house_cash();

    let close = fill(h.market_order(a, EURUSD, Side::Buy, "4500000.000001", "1.1"));
    assert_eq!((close.position, close.realised_pnl), (Fixed::ZERO, Fixed::ZERO));
    assert_eq!((h.balance(a), h.engine.house_cash()), (f("1000000"), house));
}

#[test]
fn dust_adds_at_a_moved_price_never_beat_closing_without_them() {
    // The long is 100 units of precision above the new price and the short 100 below it, so each
    // exact average lands just inside the entry and rounds back onto it.
    for (side, moved) in [(Side::Buy, "1.0999999"), (Side::Sell, "1.1000001")] {
        let (mut h, adder) = eurusd(Harness::v5(), "1.1");
        let holder = acct(2);
        h.deposit(holder, "1000000");
        h.market_order(adder, EURUSD, side, "4500000", "1.1");
        h.market_order(holder, EURUSD, side, "4500000", "1.1");
        h.price(EURUSD, moved);
        (0..1_000).for_each(|_| {
            h.market_order(adder, EURUSD, side, DUST, moved);
        });
        assert_eq!(entry(&h, adder), f("1.1"), "{side:?}");

        let back = if side == Side::Buy { Side::Sell } else { Side::Buy };
        let with_adds = fill(h.market_order(adder, EURUSD, back, "4500000.000001", moved)).realised_pnl;
        let without = fill(h.market_order(holder, EURUSD, back, "4500000", moved)).realised_pnl;
        // Closing 4.5m at 0.0000001 against the holder loses 0.45; the dust adds lose a little more.
        assert_eq!((with_adds, without), (f("-0.450000001"), f("-0.45")), "{side:?}");
    }
}

#[test]
fn an_add_then_a_round_trip_at_one_price_realises_nothing() {
    let mut h = Harness::v5();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000.123456789");
    let price = "100000.123456789";
    // 1.5 and 2.5 at this price are not whole units of precision, so a truncated average would
    // land one unit below the price.
    h.market_order(a, BTC, Side::Buy, "1.5", price);
    h.market_order(a, BTC, Side::Buy, "2.5", price);
    assert_eq!(entry(&h, a), f(price));
    // A reduction, a flip through the position, and the close of the new short.
    let realised: Vec<Fixed> = [(Side::Sell, "1"), (Side::Sell, "4"), (Side::Buy, "1")]
        .into_iter()
        .map(|(side, qty)| fill(h.market_order(a, BTC, side, qty, price)).realised_pnl)
        .collect();
    assert_eq!(realised, [Fixed::ZERO; 3]);
    assert!(h.engine.accounts()[&a].positions.is_empty());
    assert_eq!(h.balance(a), f("100000"));
}

#[test]
fn the_rounding_rule_changes_at_the_seq_v5_activates() {
    let (mut h, a) = eurusd(Harness::v4(ClientAuth::Unsigned), "1.1");
    h.market_order(a, EURUSD, Side::Buy, "4500000", "1.1");
    (0..3).for_each(|_| {
        h.market_order(a, EURUSD, Side::Buy, DUST, "1.1");
    });
    // Before `V5` each dust buy truncates the long's entry one unit of precision lower.
    assert_eq!(entry(&h, a), f("1.099999997"));

    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V5 });
    // From `V5` each exact average sits just above a whole unit and rounds up onto the next one.
    (0..3).for_each(|_| {
        h.market_order(a, EURUSD, Side::Buy, DUST, "1.1");
    });
    assert_eq!(entry(&h, a), f("1.1"));
    let close = fill(h.market_order(a, EURUSD, Side::Sell, "4500000.000006", "1.1"));
    assert_eq!(close.realised_pnl, Fixed::ZERO);
}

#[test]
fn a_financing_charge_rounds_up_and_a_credit_toward_zero() {
    let mut h = Harness::v5();
    // Longs pay 34% + 2.5% a year, 0.1% a day; shorts receive 34% - 2.5%.
    h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 3_400 });
    let (long, short, dust, dust_short) = (acct(1), acct(2), acct(3), acct(4));
    for account in [long, short, dust, dust_short] {
        h.deposit(account, "10000");
    }
    h.price(BTC, "100000.123456789");
    h.market_order(long, BTC, Side::Buy, "0.333333333", "100000.123456789");
    h.market_order(short, BTC, Side::Sell, "0.333333333", "100000.123456789");
    h.price(ETH, "1000.000000001");
    h.market_order(dust, ETH, Side::Buy, DUST, "1000.000000001");
    h.price(AAPL, "1158.5");
    h.market_order(dust_short, AAPL, Side::Sell, DUST, "1158.5");

    // Past the 22:00 London rollover. On 33,333.374452263 the long owes 33.3333744522…, and the
    // short is owed 28.7671587738…. The dust long's notional is 1000.000000001 units of
    // precision, so its charge, 1.000000000001 units, rounds up to 2 only if the notional rounds up.
    // The dust short's notional is 1158.5 units: rounded down to 1158 it earns 0.997 of a unit a
    // day, which rounds toward zero to nothing, where 1159 would earn a whole unit.
    h.advance_to(ny(2026, 10, 20, 17, 1));
    let charges: Vec<(AccountId, Fixed)> = h
        .cmd(Command::Tick)
        .into_iter()
        .filter_map(|event| match event {
            Event::Financing { account, amount, .. } => Some((account, amount)),
            _ => None,
        })
        .collect();
    assert_eq!(charges, [(long, f("33.333374453")), (short, f("-28.767158773")), (dust, f("0.000000002"))]);
}

#[test]
fn a_stress_spread_rounds_up_for_a_buyer_and_a_seller_from_v10() {
    // 5bps of 100,000.123456789 is 50.0000617283945.
    let cases = [
        (Harness::v9(), "100050.123518517", "99950.123395061"),
        (Harness::v10(), "100050.123518518", "99950.123395060"),
    ];
    for (mut h, buy, sell) in cases {
        let (buyer, seller) = (acct(1), acct(2));
        h.deposit(buyer, "100000");
        h.deposit(seller, "100000");
        h.operator_admin(AdminCommand::SetStressFloor { market: Some(BTC), level: 1 });
        h.price(BTC, "100000.123456789");
        let version = h.engine.protocol_version();
        assert_eq!(
            fill(h.market_order(buyer, BTC, Side::Buy, "0.01", "100000.123456789")).price,
            f(buy),
            "{version:?}"
        );
        assert_eq!(
            fill(h.market_order(seller, BTC, Side::Sell, "0.01", "100000.123456789")).price,
            f(sell),
            "{version:?}"
        );
    }
}

#[test]
fn a_size_spread_shared_over_a_fill_rounds_up_from_the_seq_v10_activates() {
    let mut h = Harness::v9();
    let [a, b, c, d] = [1, 2, 3, 4].map(acct);
    [a, b, c, d].into_iter().for_each(|account| h.deposit(account, "1000000"));
    h.price(BTC, "100000");
    // 30 units grow a position through 1m..3m of notional at 2bps: 400 over 30, 13.333… a unit.
    let prices = |h: &mut Harness, buyer, seller| {
        let buy = fill(h.market_order(buyer, BTC, Side::Buy, "30", "100000")).price;
        (buy, fill(h.market_order(seller, BTC, Side::Sell, "30", "100000")).price)
    };
    assert_eq!(prices(&mut h, a, b), (f("100013.333333333"), f("99986.666666667")));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V10 });
    assert_eq!(prices(&mut h, c, d), (f("100013.333333334"), f("99986.666666666")));
}

#[test]
fn a_size_spread_rounds_up_at_its_slice_end_and_its_stress_multiple_from_v10() {
    // One unit added to each position. The first row's slice ends at an inexact notional, and only
    // rounding that end up moves its band charge from 20.000000001 to 20.000000002. The second row's
    // stress multiple of 1.5 leaves half a unit of an odd band charge, and only rounding it up keeps
    // it.
    let rows = [
        ("10.333333333", "100000.000005", 10_000, "100020.000005002", "99980.000004998"),
        ("10.5", "100000.000000001", 15_000, "100030.000000003", "99969.999999999"),
    ];
    for (held, mid, multiplier_bps, buy, sell) in rows {
        let mut h = Harness::v10();
        let level = StressLevel { size_spread_multiplier_bps: multiplier_bps, min_spread_bps: 0, trigger: None };
        h.operator_admin(AdminCommand::SetParams(ProtocolParams { stress_levels: vec![level], ..params() }));
        h.price(BTC, mid);
        for (n, side, price) in [(1, Side::Buy, buy), (2, Side::Sell, sell)] {
            let a = acct(n);
            h.deposit(a, "200000");
            h.market_order(a, BTC, side, held, mid);
            assert_eq!(fill(h.market_order(a, BTC, side, "1", mid)).price, f(price), "{side:?} at {mid}");
        }
    }
}

#[test]
fn a_margin_requirement_rounds_up_from_v10_for_a_long_and_a_short() {
    for mut h in [Harness::v9(), Harness::v10()] {
        let version = h.engine.protocol_version();
        h.price(BTC, "100.000000001");
        for (n, side) in [(1, Side::Buy), (2, Side::Sell)] {
            let a = acct(n);
            h.deposit(a, "5");
            // 0.5 at 100.000000001 is 50.0000000005 of notional, and 10% of it 5.00000000005.
            let events = h.market_order(a, BTC, side, "0.5", "100.000000001");
            if version >= ProtocolVersion::V10 {
                let required = Reject::InsufficientMargin { equity: f("5"), required: f("5.000000001") };
                assert_eq!(rejection(&events), &required, "{side:?}");
            } else {
                assert_eq!(fills(&events).len(), 1, "{side:?}: {events:?}");
            }
        }
    }
}

#[test]
fn a_liquidation_at_one_unit_of_precision_under_stress_closes_in_v9_and_v10() {
    for mut h in [Harness::v9(), Harness::v10()] {
        let version = h.engine.protocol_version();
        let a = acct(1);
        h.deposit(a, "1");
        h.price(BTC, "0.000000002");
        h.market_order(a, BTC, Side::Buy, "1000000000", "0.000000002");
        // Halving the index is a 5,000bps move, which raises stress to its 5bps spread, and leaves
        // equity of 0 against margin of 0.1. The spread rounds up to the whole index, so it stops a
        // unit short of it.
        let close = fill(h.price(BTC, "0.000000001"));
        assert!(close.liquidation, "{version:?}");
        assert_eq!((close.qty, close.price), (f("1000000000"), f("0.000000001")), "{version:?}");
        assert!(h.engine.accounts()[&a].positions.is_empty(), "{version:?}");
    }
}

#[test]
fn rounding_a_spread_up_refuses_no_fill_the_truncated_spread_allows() {
    // Each row is a stress spread, a spread band, a mid, a side, and the fill price under `V9` and
    // `V10`, or `None` where the spread reaches the price. A 99.96% spread on a mid of two units of
    // precision truncates to one unit and rounds up to the mid itself: a buyer pays it, and a seller
    // stops a unit short of the price. A 5bps spread on a mid of one unit truncates to nothing and
    // rounds up to the mid in the same way. On a mid of 1.000000001 the spread rounds up a unit as
    // usual, and a 100% band reaches the price however it rounds.
    let rows = [
        (9_996, 0, "0.000000002", Side::Sell, Some("0.000000001"), Some("0.000000001")),
        (9_996, 0, "0.000000002", Side::Buy, Some("0.000000003"), Some("0.000000004")),
        (5, 0, "0.000000001", Side::Sell, Some("0.000000001"), Some("0.000000001")),
        (5, 0, "0.000000001", Side::Buy, Some("0.000000001"), Some("0.000000002")),
        (9_996, 0, "1.000000001", Side::Sell, Some("0.000400001"), Some("0.0004")),
        (0, 10_000, "1", Side::Buy, None, None),
    ];
    for (n, (spread_bps, band_bps, mid, side, v9, v10)) in (1..).zip(rows) {
        for (mut h, expected) in [(Harness::v9(), v9), (Harness::v10(), v10)] {
            let version = h.engine.protocol_version();
            let level = StressLevel { size_spread_multiplier_bps: 10_000, min_spread_bps: spread_bps, trigger: None };
            h.operator_admin(AdminCommand::SetParams(ProtocolParams { stress_levels: vec![level], ..params() }));
            let spec = MarketSpec {
                spread_tiers: vec![SpreadTier::bps(None, band_bps)],
                max_slippage_bps: 10_000,
                ..market(WIDE, "WIDE", AssetClass::Index, Schedule::AlwaysOpen, 1_000)
            };
            h.operator_admin(AdminCommand::UpsertMarket(spec));
            h.price(WIDE, mid);
            let a = acct(n);
            h.deposit(a, "1000");
            let events = h.market_order(a, WIDE, side, "1", mid);
            match expected {
                Some(price) => assert_eq!(fill(events).price, f(price), "row {n}, {version:?}"),
                None => assert_eq!(
                    rejection(&events),
                    &Reject::InvalidOrder { reason: "spread at this size reaches the price".into() },
                    "row {n}, {version:?}"
                ),
            }
        }
    }
}

#[test]
fn the_close_out_level_rounds_up_from_v10_for_a_long_and_a_short() {
    for mut h in [Harness::v9(), Harness::v10()] {
        let version = h.engine.protocol_version();
        let (long, short) = (acct(1), acct(2));
        h.deposit(long, "10000");
        h.deposit(short, "10000");
        h.price(ETH, "19500.00000001");
        h.market_order(long, ETH, Side::Buy, "1", "19500.00000001");
        h.price(BTC, "500.00000001");
        h.market_order(short, BTC, Side::Sell, "1", "500.00000001");
        // Each unit, marked at 10,000.00000001, leaves equity of 500 against 10% margin of
        // 1,000.000000001, whose 50% close-out level is 500.0000000005.
        for (account, market) in [(long, ETH), (short, BTC)] {
            let events = h.price(market, "10000.00000001");
            let close_out = Event::CloseOut { account, equity: f("500"), margin: f("1000.000000001") };
            assert_eq!(events.contains(&close_out), version >= ProtocolVersion::V10, "{version:?}: {events:?}");
        }
    }
}

#[test]
fn financing_on_a_dust_priced_long_marked_at_the_input_ceiling_stays_in_range() {
    let (mut h, a, _) = dust_priced(Side::Buy, &["1000000000"; 100]);
    // 100bn units at 1bn is 1e20 of notional, charged 200% a year.
    h.advance_to(ny(2026, 10, 20, 17, 1));
    let charges: Vec<Fixed> = h
        .cmd(Command::Tick)
        .into_iter()
        .filter_map(|event| match event {
            Event::Financing { account, amount, .. } if account == a => Some(amount),
            _ => None,
        })
        .collect();
    assert_eq!(charges, [f("547945205479452054.794520548")]);
}

#[test]
fn adding_to_a_dust_priced_long_marked_at_the_input_ceiling_stays_in_range() {
    // 170.1bn units at the input ceiling: the add is priced and risk-checked, and the house cap
    // refuses it.
    let mut lots = vec!["1000000000"; 170];
    lots.push("100000000");
    let (mut h, a, _) = dust_priced(Side::Buy, &lots);
    let events = h.market_order(a, DUSTY, Side::Buy, DUST, "1000000000");
    assert_eq!(rejection(&events), &Reject::HouseCapacity);
    assert_eq!(entry(&h, a), f(DUST));
}

#[test]
fn a_dust_priced_long_whose_raw_product_with_the_mark_exceeds_i128_is_valued_at_the_input_ceiling() {
    // 171bn units at 1bn is 1.71e20 of notional, inside `Fixed`'s range, but the raw product of
    // qty and mark is 1.71e38, past `i128::MAX`.
    let (h, a, events) = dust_priced(Side::Buy, &["1000000000"; 171]);
    assert!(!events.iter().any(|e| matches!(e, Event::CloseOut { .. })), "{events:?}");
    assert_eq!(h.engine.markets()[&DUSTY].mark, Some(f("1000000000")));
    assert_eq!(h.engine.house_gross_exposure(), f("171000000000000000000"));
    // The long gains its notional less the 171 it paid.
    let gain = f("170999999999999999829");
    assert_eq!(h.engine.house_nav(), h.engine.house_cash() - gain);
    let (equity, margin) = h.engine.equity_and_margin(&h.engine.accounts()[&a], None);
    assert_eq!((equity, margin), (f("1000") + gain, f("17100000000000000000")));
}

#[test]
fn a_dust_priced_short_whose_raw_product_with_the_mark_exceeds_i128_closes_out_at_the_input_ceiling() {
    let (h, a, events) = dust_priced(Side::Sell, &["1000000000"; 171]);
    // The move from one unit of precision to the input ceiling raises stress, so the close-out pays
    // the 5bps stress spread.
    let close = fill(events.clone());
    assert!(close.liquidation);
    assert_eq!((close.qty, close.price), (f("171000000000"), f("1000500000")));
    let loss = f("171085499999999999829");
    assert_eq!(close.realised_pnl, -loss);
    assert!(events.contains(&Event::BadDebt { account: a, amount: loss - f("1000") }), "{events:?}");
    assert!(h.engine.accounts()[&a].positions.is_empty());
    assert_eq!(h.balance(a), Fixed::ZERO);
    h.assert_conserved(Fixed::ZERO);
}

// ── Helpers ──

/// `h` with `EURUSD` listed and priced at `mid`, and account 1 funded with 1m.
fn eurusd(mut h: Harness, mid: &str) -> (Harness, AccountId) {
    let spec = MarketSpec {
        spread_tiers: vec![SpreadTier::bps(None, 0)],
        ..market(EURUSD, "EUR-USD", AssetClass::Fx, Schedule::AlwaysOpen, 100)
    };
    h.admin(AdminCommand::UpsertMarket(spec));
    h.price(EURUSD, mid);
    let a = acct(1);
    h.deposit(a, "1000000");
    (h, a)
}

/// `DUSTY` listed with the most financing a listing allows, account 1 trading `lots` on `side` at
/// one unit of precision each, then the market marked at `Fixed::MAX_INPUT`, with that update's
/// events.
fn dust_priced(side: Side, lots: &[&str]) -> (Harness, AccountId, Vec<Event>) {
    let mut h = Harness::v5();
    h.admin(AdminCommand::SetBenchmarkRate { benchmark: "SOFR".into(), annual_bps: 10_000 });
    let mut spec = market(DUSTY, "DUSTY", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000);
    spec.financing.markup_bps = 10_000;
    h.admin(AdminCommand::UpsertMarket(spec));
    h.price(DUSTY, DUST);
    let a = acct(1);
    h.deposit(a, "1000");
    for lot in lots {
        assert_eq!(fills(&h.market_order(a, DUSTY, side, lot, DUST)).len(), 1, "lot {lot}");
    }
    let events = h.price(DUSTY, "1000000000");
    (h, a, events)
}

/// The one fill among `events`; panics if there is not exactly one.
fn fill(events: Vec<Event>) -> Fill {
    match &fills(&events)[..] {
        [fill] => (*fill).clone(),
        _ => panic!("expected one fill, got {events:?}"),
    }
}

/// The averaged entry of the account's open position, in whichever market it holds one.
fn entry(h: &Harness, account: AccountId) -> Fixed {
    let positions = &h.engine.accounts()[&account].positions;
    assert_eq!(positions.len(), 1, "{positions:?}");
    positions.values().next().expect("one position").entry
}
