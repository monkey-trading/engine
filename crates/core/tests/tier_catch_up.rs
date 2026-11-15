//! `V69`: a rollover reads an account's tier from the volume that had counted by its own time, however
//! many fills of the account a catch-up makes first. A command stamped a little after a Friday's 22:00
//! UTC catches up the weekend close-out and the rollover that coincide there; the close-out's fill
//! settles the wallet's volume book to the command's stamp, and the rollover must still read the book as
//! it stood at 22:00. Before `V69` it read volume that matured, or whose round-trip horizon ended, after
//! 22:00 and charged the tier 2 financing discount early.

mod common;

use common::close_spread::{reduce, trade};
use common::events::{fills, only_fill};
use common::toxicity::v54_params;
use common::weekend::{OIL, OIL_WEEKEND_BPS, chicago, client, genesis_with_params, globex};
use common::{BTC, Client, ETH, Harness};
use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::fixed::Rounding;
use engine_core::holding_fee::DEFAULT_PROFIT_FEE_CURVE;
use engine_core::params::ProtocolParams;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{MarketId, Side};
use engine_core::volume_tier::{
    ACTIVATION_EMISSIONS_BOOSTS_BPS, ACTIVATION_FEE_DISCOUNTS_BPS, ACTIVATION_FINANCING_DISCOUNTS_BPS, VolumeTier,
};

const V68: ProtocolVersion = ProtocolVersion::V68;
const V69: ProtocolVersion = ProtocolVersion::V69;
const SECOND_MS: i64 = 1_000;

/// How the wallet's tier 2 volume comes about just after 22:00 UTC.
#[derive(Clone, Copy)]
enum Volume {
    /// 0.8 BTC bought at 21:59:30, which counts from 22:00:30.
    Opening,
    /// 0.2 BTC bought hours early and 0.1 sold at 21:59:58, which counts from 22:00:03.
    Closing,
}

#[test]
fn a_rollover_reads_no_opening_volume_that_matures_after_it() {
    let (events, wallet, h) = catch_up(V69, Volume::Opening);
    assert_eq!(financing(&events, &wallet, BTC), undiscounted(&h, "0.8"));
    assert_eq!(h.engine.volume_tier(wallet.account), 2, "the volume counts once its time has come");
}

#[test]
fn a_rollover_reads_no_closing_volume_inside_the_horizon_at_it() {
    let (events, wallet, h) = catch_up(V69, Volume::Closing);
    assert_eq!(financing(&events, &wallet, BTC), undiscounted(&h, "0.1"));
    assert_eq!(h.engine.volume_tier(wallet.account), 2, "the volume counts once its time has come");
}

#[test]
fn a_catch_up_deferred_across_commands_keeps_the_early_counts_for_the_rollovers_it_still_owes() {
    // The wallet's close-out counts $52,000 on the stamp's day, which alone reaches tier 2 at $50,000. A
    // stamp an hour after the 367th rollover defers the first catch-up command at that rollover, whose own
    // tier a second command reads on the same day.
    let (mut h, wallet, close) = setup(V69, Volume::Closing, 50_000);
    let stamp = close + 366 * 24 * 60 * 60 * SECOND_MS + 60 * 60 * SECOND_MS;
    h.advance_to(stamp);
    let first = h.cmd(Command::Tick);
    assert!(first.iter().any(|event| matches!(event, Event::CatchUpDeferred { .. })), "{first:?}");
    assert!(fills(&first).iter().any(|fill| fill.liquidation && fill.market == OIL), "{first:?}");
    let second = h.cmd(Command::Tick);
    assert_eq!(financing(&second, &wallet, BTC), undiscounted(&h, "0.1"));
    assert_eq!(h.engine.volume_tier(wallet.account), 2);
}

#[test]
fn before_v69_the_rollover_reads_the_volume_the_settlement_counted() {
    for volume in [Volume::Opening, Volume::Closing] {
        let (events, wallet, h) = catch_up(V68, volume);
        let qty = if matches!(volume, Volume::Opening) { "0.8" } else { "0.1" };
        assert!(financing(&events, &wallet, BTC) < undiscounted(&h, qty));
    }
}

/// Runs the scenario: a wallet at tier 1 holding a long of BTC and an oil long that Friday's weekend
/// close liquidates after an adverse oil price, with the tier 2 volume of `volume` counting just after
/// 22:00 UTC, and a `Tick` at 22:00:40 that catches up the close and the rollover. Returns that command's
/// events.
fn catch_up(version: ProtocolVersion, volume: Volume) -> (Vec<Event>, Client, Harness) {
    let (mut h, wallet, close) = setup(version, volume, 100_000);
    h.advance_to(close + 40 * SECOND_MS);
    let events = h.cmd(Command::Tick);
    assert!(fills(&events).iter().any(|fill| fill.liquidation && fill.market == OIL), "{events:?}");
    (events, wallet, h)
}

/// The scenario up to the last price before the weekend gap starts at the returned time, with a table
/// whose tier 2 needs `tier_two` USD.
fn setup(version: ProtocolVersion, volume: Volume, tier_two: i64) -> (Harness, Client, i64) {
    let params = ProtocolParams {
        volume_tiers: tiers(tier_two),
        profit_fee_curve: DEFAULT_PROFIT_FEE_CURVE.to_vec(),
        ..v54_params()
    };
    let mut h = genesis_with_params(version, globex(vec![]), OIL_WEEKEND_BPS, 0, params);
    // Friday 6 November 2026, 16:00 in Chicago: 22:00 UTC, when the weekend gap starts and a rollover is due.
    let close = chicago(2026, 11, 6, 16, 0);
    let (hour, minute) = (60 * 60 * SECOND_MS, 60 * SECOND_MS);
    h.advance_to(close - 4 * hour);
    h.cmd(Command::Tick);
    let wallet = client(&mut h, 1, "20000");
    for (market, mid) in [(BTC, "100000"), (ETH, "2500"), (OIL, "70")] {
        h.price(market, mid);
    }
    // $100 of counted volume reaches tier 1; so does the oil long, a lot of $70,000 that counts after a minute.
    only_fill(&trade(&mut h, &wallet, ETH, Side::Sell, "0.04", "2500"));
    if matches!(volume, Volume::Closing) {
        only_fill(&trade(&mut h, &wallet, BTC, Side::Buy, "0.2", "100000"));
    }
    h.advance_to(close - 3 * hour);
    h.price(OIL, "70");
    only_fill(&trade(&mut h, &wallet, OIL, Side::Buy, "1000", "70"));
    h.advance_to(close - 3 * hour + minute);
    h.price(BTC, "100000");
    // The oil lot's $70,000 and the earlier fills reach tier 1, and tier 2 where it needs no more than that.
    let before = if tier_two <= 70_100 { 2 } else { 1 };
    assert_eq!(h.engine.volume_tier(wallet.account), before);
    match volume {
        Volume::Opening => {
            h.advance_to(close - 30 * SECOND_MS);
            h.price(BTC, "100000");
            only_fill(&trade(&mut h, &wallet, BTC, Side::Buy, "0.8", "100000"));
        }
        Volume::Closing => {
            h.advance_to(close - 2 * SECOND_MS);
            h.price(BTC, "100000");
            only_fill(&reduce(&mut h, &wallet, BTC, Side::Sell, "0.1", "100000"));
        }
    }
    // An adverse oil price leaves the wallet below its weekend margin.
    h.advance_to(close - SECOND_MS);
    h.price(OIL, if matches!(volume, Volume::Opening) { "57" } else { "52" });
    assert_eq!(h.engine.volume_tier(wallet.account), before, "the volume has not counted yet");
    (h, wallet, close)
}

/// A long of `qty` BTC's financing at the listed markup and the 4.30% benchmark, rounded up as the engine does.
fn undiscounted(h: &Harness, qty: &str) -> Fixed {
    let spec = &h.engine.markets()[&BTC].spec;
    let mark = h.engine.markets()[&BTC].mark.expect("priced");
    let annual = Fixed::from_bps(430) + Fixed::from_bps(spec.financing.markup_bps);
    let notional = common::f(qty).mul_rounded(mark, Rounding::Up);
    notional.mul_rounded(annual, Rounding::Up).div_int_rounded(spec.financing.day_basis, Rounding::Up)
}

/// The financing `client` paid on `market` among `events`.
fn financing(events: &[Event], client: &Client, market: MarketId) -> Fixed {
    events
        .iter()
        .find_map(|event| match event {
            Event::Financing { account, market: m, amount } if *account == client.account && *m == market => {
                Some(*amount)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no financing for {market}: {events:?}"))
}

/// A table whose tier 1 needs $1 of counted volume in a day and tier 2 $100,000, each with the benefits
/// the `V68` activation gives its position.
fn tiers(tier_two: i64) -> Vec<VolumeTier> {
    [(0, 2_500), (1, 5_000), (tier_two, 10_000), (10_000_000, 25_000)]
        .into_iter()
        .enumerate()
        .map(|(at, (usd, best_price_share_bps))| VolumeTier {
            threshold: Fixed::from_int(usd),
            day_divisor: 1,
            best_price_share_bps,
            margin_floor_bps: 0,
            max_resting_orders: 0,
            requests_per_second: 0,
            fee_discount_bps: ACTIVATION_FEE_DISCOUNTS_BPS[at],
            financing_discount_bps: ACTIVATION_FINANCING_DISCOUNTS_BPS[at],
            emissions_boost_bps: ACTIVATION_EMISSIONS_BOOSTS_BPS[at],
        })
        .collect()
}
