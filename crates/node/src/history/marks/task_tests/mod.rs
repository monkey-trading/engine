//! The marks task end to end: recovery seeding from a short log, and live maintenance through the
//! sequencer's mark reports.

mod live;
mod recovery;

use std::sync::Arc;

use engine_core::command::Command;
use engine_core::price::SourceQuote;
use engine_core::types::MarketId;

use crate::clock::test_clock::TestClock;
use crate::sequencer::genesis_commands;
use crate::test_support::{TestSequencer, dev_config};

/// A dev-genesis sequencer on a clock the test moves, so mark samples land in distinct buckets.
fn priced_node(dir: &std::path::Path) -> (TestSequencer, Arc<TestClock>) {
    let clock = Arc::new(TestClock::default());
    let node = TestSequencer::start_at(dir, 1024, clock.clone(), genesis_commands(&dev_config().genesis));
    (node, clock)
}

/// Three fresh venue quotes for `market` at the clock's time, which the engine's median takes.
fn prices(market: MarketId, ts: i64, mid: &str) -> Command {
    let quotes = ["mock-a", "mock-b", "mock-c"]
        .iter()
        .map(|source| SourceQuote { source: source.to_string(), mid: mid.parse().unwrap(), ts, contract: None })
        .collect();
    Command::Prices { market, quotes }
}
