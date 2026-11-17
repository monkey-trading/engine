//! Command latency on a launch-scale book (#905): 100k accounts at `ProtocolVersion::LATEST` on the
//! production genesis's 34 markets, each holding positions, resting orders up to its tier's limit,
//! a volume book, a toxicity score and session keys (`common::launch_book`, dealt out by
//! `Mix::proposal`). Each case prints its p50, p99 and p999 in microseconds as `LAUNCH name ...`, and
//! the case's cost per holder or per order where it scales. Run in a quiet CPU window:
//! `cargo test -p engine-core --test launch_book_benchmark --locked -- --ignored --nocapture --test-threads=1`.

#[path = "launch_book_benchmark/cases.rs"]
mod cases;
mod common;
#[path = "launch_book_benchmark/report.rs"]
mod report;

use common::launch_book::{Mix, NamedCounts, build};
use report::{exceeded, rss_mib};

/// The proposal's book size; `LAUNCH_BOOK_ACCOUNTS` runs a smaller one.
const ACCOUNTS: usize = 100_000;
const NAMED: NamedCounts = NamedCounts { adders: 100, openers: 100, closeouts: 30 };

/// The budgets #904's launch bar sets, which #905's proposal asserts (microseconds).
const PRICE_P99_US: u128 = 250_000;
const SIGNED_P99_US: u128 = 2_000;
const ROOT_100K_US: u128 = 500_000;
/// No single command, rollover and close-out included, holds the thread over this.
const SINGLE_COMMAND_US: u128 = 500_000;

#[test]
#[ignore = "100k-account benchmark: run in an isolated CPU window"]
fn launch_book_latency() {
    let accounts = std::env::var("LAUNCH_BOOK_ACCOUNTS").ok().map_or(ACCOUNTS, |n| n.parse().expect("a count"));
    let rss_before = rss_mib();
    let mut book = build(&Mix::proposal(accounts), NAMED);
    cases::summary(&book, accounts, rss_before, rss_mib());
    cases::roots_and_proofs(&mut book, accounts, ROOT_100K_US);
    cases::prices(&mut book, PRICE_P99_US);
    cases::price_rounds(&mut book);
    cases::signed_orders(&mut book, SIGNED_P99_US);
    cases::close_outs(&mut book, SINGLE_COMMAND_US);
    cases::rollover(&mut book, SINGLE_COMMAND_US);
    cases::encoding_and_recovery(&mut book);
    println!("LAUNCH resident_mib={}", rss_mib());
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
