//! The launch book's own properties (#905), on a small book: it is what `Mix` says, at the latest
//! protocol on the production genesis's markets, and a price moving only the mark trades nothing. The
//! benchmarks that use it are `launch_book_benchmark`.

mod common;

use common::f;
use common::launch_book::{Mix, NamedCounts, build, price_command};
use engine_core::account::Account;
use engine_core::command::OrderKind;
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Side;

const ACCOUNTS: usize = 4_000;
const NAMED: NamedCounts = NamedCounts { adders: 2, openers: 2, closeouts: 3 };

#[test]
fn the_book_is_the_mix_on_the_production_genesis() {
    let mix = Mix::proposal(ACCOUNTS);
    let mut book = build(&mix, NAMED);
    let engine = &book.h.engine;
    assert_eq!(engine.protocol_version(), ProtocolVersion::LATEST);
    assert_eq!(engine.markets().len(), 34);
    assert_eq!(engine.accounts().len(), ACCOUNTS);
    // Shares of accounts, within a tenth of the share plus a few accounts, which the named ones fall in.
    let near = |count: usize, share: u64| {
        let want = ACCOUNTS as u64 * share / 100;
        count.abs_diff(want as usize) as u64 <= want / 10 + 5
    };
    for (tier, share) in mix.tiers.iter().enumerate() {
        assert!(near(book.tiers[tier], *share), "tier {tier}: {} accounts", book.tiers[tier]);
    }
    assert!(near(book.resting_accounts, mix.resting), "{} accounts rest orders", book.resting_accounts);
    // Every account but an opener holds a position, and each holding bucket is its share of them.
    let flat = engine.accounts().values().filter(|a| a.positions.is_empty()).count();
    assert_eq!(flat, NAMED.openers, "only the openers hold nothing");
    for (share, fewest, most) in &mix.holdings {
        let held = engine.accounts().values().filter(|a| (*fewest..=*most).contains(&a.positions.len())).count();
        assert!(near(held, *share), "{share}% hold {fewest} to {most} markets: {held} accounts");
    }
    let all = engine.accounts().values().filter(|a| a.positions.len() == 34).count();
    assert!(book.holders.values().all(|held| *held >= all), "{:?}", book.holders);
    // Holders follow the cap study's market mix: BTC-USD, ETH-USD and SOL-USD lead it, and EUR-USD
    // trails them by far.
    let holders = |market| book.holders[&market];
    assert!(holders(1) > holders(2) && holders(2) > holders(3), "{:?}", book.holders);
    assert!(holders(3) > 2 * holders(10), "{:?}", book.holders);
    // Each account's orders are at its tier's limit, and a share of them inside the first band, which
    // the template's order away from the price at 50 does not shift.
    let (mut in_band, mut orders) = (0, 0);
    for (id, account) in engine.accounts() {
        for order in account.orders.values() {
            let OrderKind::Limit { price } = order.kind else { panic!("{order:?}") };
            in_band += usize::from((price - f("100")).abs() < f("1"));
            orders += 1;
        }
        if account.orders.is_empty() {
            continue;
        }
        let limit = engine.params().unwrap().volume_tiers[engine.volume_tier(*id)].max_resting_orders;
        assert!(account.orders.len() == limit || book.named.adders.iter().any(|c| c.account == *id));
    }
    let share = in_band * 100 / orders;
    assert!(share.abs_diff(mix.in_band as usize) <= 3, "{share}% of {orders} orders in the first band");
    // Scores, session keys and eligibility for every account.
    let state = serde_json::to_value(engine).unwrap();
    // The openers have made no fill to score.
    assert_eq!(state["toxicity_scores"].as_object().unwrap().len(), ACCOUNTS - NAMED.openers);
    assert_eq!(state["eligibility"].as_object().unwrap().len(), ACCOUNTS);
    let keys = state["session_keys"].as_object().unwrap();
    // Every account but the named ones holds them, but for the one that registered them.
    assert_eq!(keys.len(), ACCOUNTS - (NAMED.adders + NAMED.openers + NAMED.closeouts) + 1);
    assert!(keys.values().all(|k| k["keys"].as_object().unwrap().len() == mix.session_keys));
    // A price that moves only the mark trades nothing, in any market.
    for id in state["markets"].as_object().unwrap().keys().map(|id| id.parse().unwrap()) {
        book.h.advance(1);
        let cmd = price_command(&book.h, id, "100.0001", book.h.ts);
        let events = book.apply(cmd);
        assert!(!events.iter().any(|e| matches!(e, Event::Fill(_) | Event::OrderCancelled { .. })), "{id}: {events:?}");
    }
}

#[test]
fn a_book_is_the_same_on_every_build() {
    let roots: Vec<[u8; 32]> = (0..2).map(|_| build(&Mix::proposal(1_000), NAMED).h.engine.state_root()).collect();
    assert_eq!(roots[0], roots[1]);
}

#[test]
fn the_in_band_orders_are_the_ones_a_price_reaches() {
    let in_band = |market, buy: bool, book: &common::launch_book::Book| {
        let rests = |account: &Account| {
            account
                .orders
                .values()
                .filter(|o| o.market == market && (o.side == Side::Buy) == buy)
                .filter(|o| matches!(o.kind, OrderKind::Limit { price } if (price - f("100")).abs() < f("1")))
                .count()
        };
        book.h.engine.accounts().values().map(rests).sum::<usize>()
    };
    // BTC-USD's in-band orders buy, ETH-USD's sell: a fall in BTC-USD fills the first and a rise
    // in ETH-USD the second, each on a book of its own, since the first move's fills put the house at
    // a stress level whose spreads keep the second out of reach.
    for (market, buy, mid) in [(1, true, "99.5"), (2, false, "100.5")] {
        let mut book = build(&Mix::proposal(ACCOUNTS), NAMED);
        let expected = in_band(market, buy, &book);
        assert!(expected > 0, "market {market} rests in-band orders");
        let other = in_band(market, !buy, &book);
        assert_eq!(other, 0, "market {market}'s in-band orders all sit on one side");
        book.h.advance(70_000);
        let cmd = price_command(&book.h, market, mid, book.h.ts);
        let events = book.apply(cmd);
        let fills = events.iter().filter(|e| matches!(e, Event::Fill(f) if f.market == market)).count();
        assert!(fills > 0 && fills <= expected, "market {market}: {fills} fills of {expected} in-band orders");
    }
}

/// Gold's `GCG7` and silver's `SIH7` roll at 14:30 UTC on 25 Nov 2026.
const GOLD_ROLL: engine_core::types::Millis = 1_795_617_000_000;

#[test]
fn a_book_ten_minutes_before_a_roll_time_holds_the_gap_a_node_started_past_it_rolls_on() {
    // The load scenarios start a node ten minutes after the book's clock, so one that starts past a
    // roll time finds the market with no gap recorded, and pages `futures_roll_overdue` until its feeds
    // quote both contracts, unless the book quoted the next contract as well.
    let mix = Mix { at: GOLD_ROLL - 5 * 60_000, always_open: true, ..Mix::proposal(200) };
    let book = build(&mix, NamedCounts { adders: 1, openers: 1, closeouts: 0 });
    let engine = &book.h.engine;
    assert!(engine.now() < GOLD_ROLL, "the book ends before the roll time");
    let mut checked = 0;
    for (id, market) in engine.markets() {
        let (Some(futures), Some(next)) = (&market.futures, market.next_contract()) else { continue };
        checked += 1;
        assert!(
            futures.gap.is_some(),
            "market {id} ({}) has no gap recorded for {}",
            market.spec.symbol,
            next.contract
        );
    }
    assert!(checked >= 7, "{checked} futures markets with a next contract");
}
