//! The launch book's own properties (#905), on a small book: it is what `Mix` says, at the latest
//! protocol on the production genesis's markets, and a price moving only the mark visits nothing
//! resting. The benchmarks that use it are `launch_book_benchmark`.

mod common;

use common::launch_book::{Mix, NamedCounts, build, price_command};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;

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
    // Shares of accounts, within a tenth of the draw's spread.
    let near = |count: usize, share: u64| {
        let want = ACCOUNTS as u64 * share / 100;
        count.abs_diff(want as usize) as u64 <= want / 10 + 5
    };
    for (tier, share) in mix.tiers.iter().enumerate() {
        assert!(near(book.tiers[tier], *share), "tier {tier}: {} accounts", book.tiers[tier]);
    }
    assert!(near(book.resting_accounts, mix.resting), "{} accounts rest orders", book.resting_accounts);
    // Every account holds a position, and the accounts holding all markets are the proposal's 5%.
    let all = engine.accounts().values().filter(|a| a.positions.len() == 34).count();
    assert!(near(all, 5), "{all} accounts hold every market");
    let flat = engine.accounts().values().filter(|a| a.positions.is_empty()).count();
    assert_eq!(flat, NAMED.openers, "only the openers hold nothing");
    // `holders` counts the unnamed accounts; the named adders and close-outs hold every market too.
    let named_all = NAMED.adders + NAMED.closeouts;
    assert!(book.holders.values().all(|held| held + named_all >= all), "{:?}", book.holders);
    // Each account's orders are at its tier's limit, half of them inside the first band.
    for (id, account) in engine.accounts() {
        if account.orders.is_empty() {
            continue;
        }
        let limit = engine.params().unwrap().volume_tiers[engine.volume_tier(*id)].max_resting_orders;
        assert!(account.orders.len() == limit || book.named.adders.iter().any(|c| c.account == *id));
    }
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
