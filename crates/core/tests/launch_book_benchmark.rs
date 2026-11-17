//! Command latency on a launch-scale book (#905): 100k accounts at `ProtocolVersion::LATEST` on the
//! production genesis's 34 markets, each holding positions, resting orders up to its tier's limit,
//! a volume book, a toxicity score and session keys (`common::launch_book`, dealt out by
//! `Mix::proposal`). Each case prints its p50, p99 and p999 in microseconds as `LAUNCH name ...`, and
//! the case's cost per holder or per order where it scales. Run in a quiet CPU window:
//! `cargo test -p engine-core --test launch_book_benchmark --locked -- --ignored --nocapture --test-threads=1 launch_book_latency`;
//! `launch_book_price_attribution` and `launch_book_price_walk` run apart, and the close-out index's
//! memory is `launch_book_memory`'s.

#[path = "launch_book_benchmark/cases.rs"]
mod cases;
mod common;
#[path = "launch_book_benchmark/report.rs"]
mod report;

use cases::Bar;
use common::launch_book::{Mix, NamedCounts, build};
use engine_core::CloseoutOracle;
use report::{exceeded, rss_mib};

/// The proposal's book size; `LAUNCH_BOOK_ACCOUNTS` runs a smaller one.
const ACCOUNTS: usize = 100_000;
const NAMED: NamedCounts = NamedCounts { adders: 100, openers: 100, closeouts: 30 };

/// The budgets #904's launch bar sets, which #905's proposal asserts (microseconds).
const PRICE_P99_US: u128 = 250_000;
/// #904's price-to-stamp bar, a price's p99 at 100k holders of BTC-USD, which the close-out index
/// (#912) holds a price on the book to.
const PRICE_TO_STAMP_P99_US: u128 = 10_000;
/// The prices of `launch_book_price_walk`'s walk on each book.
const WALK_PRICES: usize = 1_000;
const SIGNED_P99_US: u128 = 2_000;
/// The root over 100k changed accounts: #904's bar, which the book misses (CI measured 0.8 s on
/// 17 November 2026, over the accounts a rollover financed), and the guard CI holds it to until the
/// owner accepts that figure or a change meets the bar.
const ROOT: Bar = Bar { bar_us: 500_000, guard_us: 3_000_000 };
/// The rollover's slowest of four: #904's bar, which CI's slower runners miss (0.50 s and 0.56 s on
/// 17 November 2026), and the guard CI holds it to as a regression check until the owner accepts the
/// figure or a change meets the bar.
const ROLLOVER: Bar = Bar { bar_us: 500_000, guard_us: 2_000_000 };
/// No single command, rollover and close-out included, holds the thread over this.
const SINGLE_COMMAND_US: u128 = 500_000;

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn launch_book_latency() {
    let accounts = std::env::var("LAUNCH_BOOK_ACCOUNTS").ok().map_or(ACCOUNTS, |n| n.parse().expect("a count"));
    let rss_before = rss_mib();
    let mut book = build(&Mix::proposal(accounts), NAMED);
    cases::summary(&book, accounts, rss_before, rss_mib());
    cases::roots_and_proofs(&mut book, accounts);
    cases::prices(&mut book, PRICE_TO_STAMP_P99_US, SINGLE_COMMAND_US);
    cases::price_rounds(&mut book, PRICE_P99_US);
    cases::signed_orders(&mut book, SIGNED_P99_US, SINGLE_COMMAND_US);
    cases::close_outs(&mut book, SINGLE_COMMAND_US);
    cases::rollover(&mut book, ROLLOVER, ROOT);
    cases::encoding_and_recovery(&mut book);
    println!("LAUNCH resident_mib={}", rss_mib());
    let over = exceeded();
    assert!(over.is_empty(), "over budget: {over:?}");
}

/// A walk of BTC-USD (`cases::price_walk`) on the book with resting orders away from the price, and with
/// no account, 1% and 10% of accounts a move of 1% from their close-out level, each with the close-out
/// index and with today's full check in its place (#912), each on a book of its own; the index's walk is
/// held to #904's price-to-stamp bar.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn launch_book_price_walk() {
    let accounts = std::env::var("LAUNCH_BOOK_ACCOUNTS").ok().map_or(ACCOUNTS, |n| n.parse().expect("a count"));
    for near_level in [0, 1, 10] {
        for (check, full_check) in [("index", false), ("full_check", true)] {
            let mix = Mix { in_band: 0, near_level, ..Mix::proposal(accounts) };
            let mut book = build(&mix, NamedCounts { adders: 1, openers: 0, closeouts: 0 });
            book.h.engine.set_closeout_oracle(CloseoutOracle { full_check, ..CloseoutOracle::default() });
            let p99 =
                cases::price_walk(&mut book, &format!("walk_btc_usd_near_level_{near_level}_{check}"), WALK_PRICES);
            if !full_check {
                report::within(&format!("walk_btc_usd_near_level_{near_level}_p99"), p99, PRICE_TO_STAMP_P99_US);
            }
        }
    }
    let over = exceeded();
    assert!(over.is_empty(), "over budget: {over:?}");
}

/// What a price in an FX market and in BTC-USD costs on the book with a part taken away, to say what
/// its cost is made of: the same 100k accounts without resting orders, with resting orders only
/// away from the price, and holding one market each.
#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn launch_book_price_attribution() {
    let accounts = std::env::var("LAUNCH_BOOK_ACCOUNTS").ok().map_or(ACCOUNTS, |n| n.parse().expect("a count"));
    let proposal = Mix::proposal(accounts);
    let variants = [
        ("proposal", proposal.clone()),
        ("no_resting_orders", Mix { resting: 0, ..proposal.clone() }),
        ("resting_orders_away", Mix { in_band: 0, ..proposal.clone() }),
        ("one_market_each", Mix { holdings: vec![(100, 1, 1)], ..proposal.clone() }),
        ("one_market_each_no_resting", Mix { holdings: vec![(100, 1, 1)], resting: 0, ..proposal }),
    ];
    for (name, mix) in variants {
        let mut book = build(&mix, NamedCounts { adders: 1, openers: 0, closeouts: 0 });
        cases::price_attribution(&mut book, name);
    }
}
