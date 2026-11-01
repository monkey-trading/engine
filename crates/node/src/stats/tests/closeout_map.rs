use engine_core::types::Side;

use super::*;
use crate::stats::snapshot::{Grid, Layout, scan};

pub(super) fn layout() -> Layout {
    Layout { closeout: Grid::new(2_000, 100), profit: Grid::new(5_000, 500) }
}

fn total(bins: &[crate::stats::snapshot::Bin]) -> (Fixed, u64) {
    bins.iter().fold((Fixed::ZERO, 0), |(usd, count), bin| {
        (usd + bin.long.usd + bin.short.usd, count + bin.long.count + bin.short.count)
    })
}

/// A long of 5 BTC on 100000 of equity closes out where equity falls to half the margin
/// requirement: about 15.8% below the mark, so inside the 16-15% bucket or its neighbour.
#[test]
fn a_long_lands_in_the_bucket_of_the_fall_that_closes_its_account_out() {
    let mut rig = Rig::new();
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 5);
    let snapshot = scan(&rig.engine, 0, T0, layout(), true);
    let book = &snapshot.markets[&BTC];
    assert_eq!((book.long.count, book.short.count), (1, 0));
    let hit: Vec<usize> = book.closeout.iter().enumerate().filter(|(_, b)| b.long.count == 1).map(|(i, _)| i).collect();
    assert_eq!(hit.len(), 1, "one bucket holds the position: {hit:?}");
    // Bucket 4 covers a fall of 16% to 15%, bucket 3 of 17% to 16%.
    assert!((3..=4).contains(&hit[0]), "bucket {}", hit[0]);
    assert_eq!(total(&book.closeout).1 + book.beyond.long.count + book.due.long.count, 1);
    assert_eq!(snapshot.open_positions, 1);
    assert_eq!(snapshot.open_interest, book.long.usd);
}

/// Opens `qty` BTC on `side` for the trader, who holds 100000 plus `extra` of equity, reads the close-out
/// bucket the scan puts the position in, and moves the index against it: a bucket short of the
/// bucket's near edge closes nothing out, and a bucket past its far edge does.
fn engine_agrees_with_the_bucket(side: Side, qty: i64, extra: i64) {
    let layout = Layout { closeout: Grid::new(9_000, 100), profit: Grid::new(5_000, 500) };
    let long = side == Side::Buy;
    let closes_at = |target: i64| {
        let mut rig = Rig::new();
        if extra > 0 {
            let amount = Fixed::from_int(extra);
            rig.apply(T0, Command::Deposit { account: TRADER, amount, chain_ref: "extra".into() });
        }
        rig.trade(T0 + 1_000, TRADER, side, qty);
        // Walk the index towards the target in steps small enough to stay inside the price rules.
        let (mut price, mut at, mut closed) = (100_000, T0 + 2_000, false);
        while price != target && !closed {
            let step = (target - price).clamp(-200, 200);
            price += step;
            closed = rig.price(at, price).iter().any(|event| matches!(event, Event::CloseOut { .. }));
            at += 2_000;
        }
        closed
    };
    let mut rig = Rig::new();
    if extra > 0 {
        let amount = Fixed::from_int(extra);
        rig.apply(T0, Command::Deposit { account: TRADER, amount, chain_ref: "extra".into() });
    }
    rig.trade(T0 + 1_000, TRADER, side, qty);
    let book = &scan(&rig.engine, 0, T0, layout, true).markets[&BTC];
    let held = |bin: &crate::stats::snapshot::Bin| if long { bin.long.count } else { bin.short.count };
    let bucket = book.closeout.iter().position(|bin| held(bin) == 1).expect("the position closes out in range");
    let (width, half) = (i64::from(layout.closeout.width), i64::from(layout.closeout.half));
    let (from, to) = ((bucket as i64 - half) * width, (bucket as i64 - half + 1) * width);
    let price_at = |bps: i64| 100_000 + 100_000 * bps / 10_000;
    // A long falls through the bucket's upper edge first, a short rises through its lower edge.
    let (near, far) = if long {
        (price_at(to + width), price_at(from - width))
    } else {
        (price_at(from - width), price_at(to + width))
    };
    assert!(!closes_at(near), "no close-out at {near}, a bucket short of the estimate's {from}..{to} bps");
    assert!(closes_at(far), "a close-out at {far}, a bucket past the estimate's {from}..{to} bps");
}

#[test]
fn the_estimated_close_out_price_of_a_long_is_where_the_engine_closes_the_account_out() {
    engine_agrees_with_the_bucket(Side::Buy, 5, 0);
}

#[test]
fn the_estimated_close_out_price_of_a_short_is_where_the_engine_closes_the_account_out() {
    engine_agrees_with_the_bucket(Side::Sell, 5, 0);
}

/// 20 BTC is 2M of notional, in the margin band above a million; the account is closed out only
/// after the fall brings it under a million, so the estimate must cross the band edge.
#[test]
fn an_estimate_that_crosses_a_margin_band_agrees_with_the_engine() {
    engine_agrees_with_the_bucket(Side::Buy, 20, 1_400_000);
}

#[test]
fn a_short_lands_above_the_mark_and_a_flat_book_holds_nothing() {
    let mut rig = Rig::new();
    let flat = scan(&rig.engine, 0, T0, layout(), true);
    assert_eq!((flat.open_positions, flat.open_interest, flat.open_margin), (0, Fixed::ZERO, Fixed::ZERO));
    rig.trade(T0 + 1_000, OTHER, Side::Sell, 5);
    let snapshot = scan(&rig.engine, 1, T0, layout(), true);
    let book = &snapshot.markets[&BTC];
    let hit = book.closeout.iter().position(|b| b.short.count == 1).expect("a bucket holds the short");
    assert!(hit >= book.closeout.len() / 2, "a short closes out above the mark: {hit}");
    assert!(snapshot.open_margin.is_positive());
}

#[test]
fn a_position_far_from_closing_out_is_beyond_the_range_and_profit_buckets_follow_the_mark() {
    let mut rig = Rig::new();
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 1);
    let snapshot = scan(&rig.engine, 0, T0, layout(), true);
    let book = &snapshot.markets[&BTC];
    assert_eq!(book.beyond.long.count, 1, "100000 of equity behind 1 BTC survives a 20% fall");
    // At its entry the position sits in the first bucket at or above zero profit.
    let flat = book.profit.iter().position(|b| b.long.count == 1).unwrap();
    assert_eq!(flat, 1 + 10, "the bucket from 0 to 500 bps");
    rig.price(T0 + 2_000, 110_000);
    let up = scan(&rig.engine, 1, T0, layout(), true);
    assert!(up.markets[&BTC].profit.iter().position(|b| b.long.count == 1).unwrap() > flat);
}
