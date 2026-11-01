use engine_core::types::Side;

use super::*;
use crate::stats::snapshot::{Grid, Layout, scan};

fn layout() -> Layout {
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
    let snapshot = scan(&rig.engine, 0, T0, layout());
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

/// The estimate agrees with the engine: a mark a bucket short of it closes nothing out, and one a
/// bucket past it does.
#[test]
fn the_estimated_close_out_price_is_where_the_engine_closes_the_account_out() {
    let closes_at = |mid: i64| {
        let mut rig = Rig::new();
        rig.trade(T0 + 1_000, TRADER, Side::Buy, 5);
        // Walk the index down in steps small enough to stay inside the market's price rules.
        let mut price = 100_000;
        let mut at = T0 + 2_000;
        let mut closed = false;
        while price > mid && !closed {
            price = (price - 200).max(mid);
            closed = rig.price(at, price).iter().any(|event| matches!(event, Event::CloseOut { .. }));
            at += 2_000;
        }
        closed
    };
    // 15.8% below 100000 is about 84200.
    assert!(!closes_at(85_500), "no close-out a bucket above the estimate");
    assert!(closes_at(83_000), "a close-out a bucket below it");
}

#[test]
fn a_short_lands_above_the_mark_and_a_flat_book_holds_nothing() {
    let mut rig = Rig::new();
    let flat = scan(&rig.engine, 0, T0, layout());
    assert_eq!((flat.open_positions, flat.open_interest, flat.open_margin), (0, Fixed::ZERO, Fixed::ZERO));
    rig.trade(T0 + 1_000, OTHER, Side::Sell, 5);
    let snapshot = scan(&rig.engine, 1, T0, layout());
    let book = &snapshot.markets[&BTC];
    let hit = book.closeout.iter().position(|b| b.short.count == 1).expect("a bucket holds the short");
    assert!(hit >= book.closeout.len() / 2, "a short closes out above the mark: {hit}");
    assert!(snapshot.open_margin.is_positive());
}

#[test]
fn a_position_far_from_closing_out_is_beyond_the_range_and_profit_buckets_follow_the_mark() {
    let mut rig = Rig::new();
    rig.trade(T0 + 1_000, TRADER, Side::Buy, 1);
    let snapshot = scan(&rig.engine, 0, T0, layout());
    let book = &snapshot.markets[&BTC];
    assert_eq!(book.beyond.long.count, 1, "100000 of equity behind 1 BTC survives a 20% fall");
    // At its entry the position sits in the first bucket at or above zero profit.
    let flat = book.profit.iter().position(|b| b.long.count == 1).unwrap();
    assert_eq!(flat, 1 + 10, "the bucket from 0 to 500 bps");
    rig.price(T0 + 2_000, 110_000);
    let up = scan(&rig.engine, 1, T0, layout());
    assert!(up.markets[&BTC].profit.iter().position(|b| b.long.count == 1).unwrap() > flat);
}
