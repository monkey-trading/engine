//! Round trips from `V54`, end to end through `Engine::apply`: a fill matches the account's opposite
//! fills within the horizon newest-first, what it closes against what they opened and what it opens
//! against what they closed, and quantity matched at the same index adds nothing to the score,
//! whether one fill closes one, many, or one in parts; only unmatched quantity scores.

mod common;

use common::toxicity::{BAND_BPS, another, fill, mark, scored, standing};
use common::{BTC, Harness, f};
use engine_core::Fixed;
use engine_core::command::AdminCommand;
use engine_core::listing_bounds::ListingNotice;
use engine_core::market::{MarketSpec, Tier};
use engine_core::operator_bounds::NOTICE_MS;
use engine_core::toxicity::HORIZON_MS;
use engine_core::types::Side;

/// Sequences `trades` at the current time and index, then a horizon on scores them, and returns the
/// decayed fills the account's score counts.
fn fills_scored(h: &mut Harness, client: &common::Client, trades: &[(Side, &str)]) -> Fixed {
    for (side, qty) in trades {
        fill(h, client, *side, qty, "100000");
    }
    mark(h, "100000");
    standing(h, client.account).fills
}

#[test]
fn a_fill_closed_in_two_parts_within_the_horizon_scores_nothing() {
    let (mut h, client) = scored();
    assert_eq!(fills_scored(&mut h, &client, &[(Side::Buy, "2"), (Side::Sell, "1"), (Side::Sell, "1")]), Fixed::ZERO);
}

#[test]
fn an_add_closed_in_parts_and_two_fills_closed_by_one_score_nothing() {
    let (mut h, client) = scored();
    let trades = [(Side::Buy, "1"), (Side::Buy, "1"), (Side::Sell, "1.5"), (Side::Sell, "0.5")];
    assert_eq!(fills_scored(&mut h, &client, &trades), Fixed::ZERO);
    let trades = [(Side::Sell, "1"), (Side::Sell, "0.5"), (Side::Buy, "1.5")];
    assert_eq!(fills_scored(&mut h, &client, &trades), Fixed::ZERO);
}

#[test]
fn only_the_unmatched_share_of_a_fill_scores() {
    let (mut h, client) = scored();
    // Half the buy is closed at its index: the buy counts half a fill, and the sell none.
    fill(&mut h, &client, Side::Buy, "2", "100000");
    fill(&mut h, &client, Side::Sell, "1", "100000");
    mark(&mut h, "100010");
    let now = standing(&h, client.account);
    assert_eq!(now.fills, f("0.5"));
    assert!(now.score.is_positive(), "the unmatched half rose with the index: {now:?}");
}

#[test]
fn a_flip_matches_only_the_quantity_it_closes() {
    let (mut h, client) = scored();
    // Selling 3 against a long 1 closes the 1, matched, and opens 2 short, which scores: two thirds
    // of the sell.
    assert_eq!(fills_scored(&mut h, &client, &[(Side::Buy, "1"), (Side::Sell, "3")]), f("0.666666666"));
}

#[test]
fn a_close_and_the_reopening_after_it_score_nothing() {
    let (mut h, client) = scored();
    // A long held past the horizon: its buy has scored, and its quantity is no longer open.
    assert_eq!(fills_scored(&mut h, &client, &[(Side::Buy, "2")]), f("1"));
    // Selling part of it and buying it back at the index within the horizon is a round trip, and so
    // is reversing to a short and back.
    assert_eq!(fills_scored(&mut h, &client, &[(Side::Sell, "1"), (Side::Buy, "1")]), f("1"));
    assert_eq!(fills_scored(&mut h, &client, &[(Side::Sell, "3"), (Side::Buy, "3")]), f("1"));
}

#[test]
fn a_reopening_takes_the_newest_closed_quantity_first() {
    let (mut h, client) = scored();
    assert_eq!(fills_scored(&mut h, &client, &[(Side::Buy, "2")]), f("1"));
    // A sell at one index, a buy back at another, which takes the sell's closed quantity unmatched,
    // and a buy at the sell's index, which finds none left: all three score.
    fill(&mut h, &client, Side::Sell, "1", "100000");
    h.advance(300);
    h.price(BTC, "100010");
    fill(&mut h, &client, Side::Buy, "1", "100010");
    h.advance(300);
    h.price(BTC, "100000");
    fill(&mut h, &client, Side::Buy, "1", "100000");
    mark(&mut h, "100000");
    assert_eq!(standing(&h, client.account).fills, f("4"));
}

#[test]
fn a_close_at_another_index_or_after_the_horizon_matches_nothing() {
    let (mut h, client) = scored();
    fill(&mut h, &client, Side::Buy, "1", "100000");
    h.advance(500);
    h.price(BTC, "100010");
    fill(&mut h, &client, Side::Sell, "1", "100010");
    mark(&mut h, "100010");
    assert_eq!(standing(&h, client.account).fills, f("2"));
    // A close a whole horizon after the open, when the open's markout has fallen due, is no round trip.
    fill(&mut h, &client, Side::Buy, "1", "100010");
    mark(&mut h, "100010");
    fill(&mut h, &client, Side::Sell, "1", "100010");
    mark(&mut h, "100010");
    assert_eq!(standing(&h, client.account).fills, f("4"));
}

#[test]
fn a_close_matches_the_newest_open_quantity_first() {
    let (mut h, client) = scored();
    // Two buys at two indexes, then a close back at the first one's index: it takes the newer buy,
    // at another index, so nothing matches and all three fills score.
    fill(&mut h, &client, Side::Buy, "1", "100000");
    h.advance(300);
    h.price(BTC, "100010");
    fill(&mut h, &client, Side::Buy, "1", "100010");
    h.advance(300);
    h.price(BTC, "100000");
    fill(&mut h, &client, Side::Sell, "1", "100000");
    mark(&mut h, "100000");
    assert_eq!(standing(&h, client.account).fills, f("3"));
}

#[test]
fn a_close_out_takes_open_quantity_but_matches_none() {
    let (mut h, _) = scored();
    // The initial margin of its buy and the buy's 1 bp spread.
    let thin = another(&mut h, 2, "1001");
    // A listing notice raises BTC's margin to the whole notional from a week on.
    let full = vec![Tier { up_to: None, bps: 10_000 }];
    let effective_at = h.ts + NOTICE_MS;
    let notice = ListingNotice { spread_tiers: None, margin_tiers: Some(full.clone()), markup_bps: None, effective_at };
    h.operator_admin(AdminCommand::AnnounceListing { market: BTC, notice });
    h.advance_to(effective_at - 1_000);
    h.price(BTC, "100000");
    fill(&mut h, &thin, Side::Buy, "0.1", "100000");
    // The relisting at full margin a second on closes the position out at the same index within
    // the horizon: the close-out is no round trip, so the buy still scores.
    h.advance(1_000);
    h.price(BTC, "100000");
    let listed = h.engine.markets()[&BTC].spec.clone();
    h.operator_admin(AdminCommand::UpsertMarket(MarketSpec { margin_tiers: full, ..listed }));
    assert!(h.engine.accounts()[&thin.account].positions.is_empty());
    // It took the buy's open quantity, which no later close can match, and matched none of it.
    let queued = serde_json::to_value(&h.engine).unwrap()["markouts"].clone();
    assert_eq!(queued.as_array().map(Vec::len), Some(1), "{queued}");
    assert_eq!(queued[0]["qty"], "0.1");
    assert!(queued[0].get("open").is_none() && queued[0].get("matched").is_none(), "{queued}");
    mark(&mut h, "100000");
    assert_eq!(standing(&h, thin.account).fills, f("1"));
}

#[test]
fn a_recovered_engine_matches_the_fills_queued_before_it() {
    let (mut h, client) = scored();
    fill(&mut h, &client, Side::Buy, "1", "100000");
    fill(&mut h, &client, Side::Sell, "2", "100000");
    // Recovered from its state, as a restart or a snapshot recovers it, the engine rebuilds what a
    // fill may match from the queued markouts: a buy closing the sell's open half at the same index
    // leaves all three fills round trips.
    h.engine = serde_json::from_value(serde_json::to_value(&h.engine).unwrap()).unwrap();
    fill(&mut h, &client, Side::Buy, "1", "100000");
    mark(&mut h, "100000");
    assert_eq!(standing(&h, client.account).fills, Fixed::ZERO);
}

#[test]
fn round_trips_leave_a_toxic_score_where_it_was() {
    let (mut h, client) = scored();
    let mut mid = f("100000");
    for _ in 0..25 {
        fill(&mut h, &client, Side::Buy, "0.1", &mid.to_string());
        mid += f("10");
        mark(&mut h, &mid.to_string());
    }
    let toxic = standing(&h, client.account);
    assert_eq!(toxic.tier, 1);
    // Forty round trips of 5 BTC at the index, each fill paying the tier's add-on: none scores.
    for _ in 0..40 {
        for side in [Side::Buy, Side::Sell] {
            assert_eq!(fill(&mut h, &client, side, "5", &mid.to_string()).spread_bps, BAND_BPS + 2);
        }
        h.advance(HORIZON_MS / 10);
        h.price(BTC, &mid.to_string());
    }
    mark(&mut h, &mid.to_string());
    assert_eq!(standing(&h, client.account), toxic);
}
