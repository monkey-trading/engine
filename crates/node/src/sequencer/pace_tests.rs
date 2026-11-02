//! Sequenced time on a test clock against a host clock that stalls or steps back: a quote the engine
//! holds ages out on monotonic time, and a run's first stamp is past every quote carried across
//! its restart.

use std::path::Path;
use std::time::Duration;

use engine_core::price::{PriceError, SourceQuote};
use engine_core::types::Millis;
use tokio::sync::{broadcast, mpsc, watch};

use super::*;
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::log::Manifest;
use crate::sequencer::fixture_tests::finished_genesis;
use crate::test_support::{dev_config, log_entries, sample_around, scratch_dir, sequencer_config};

const AT: Millis = 1_792_530_000_000;
const STEP: Duration = Duration::from_millis(100);
const MAX_DRIFT: Duration = Duration::from_secs(10);

#[test]
fn a_stalled_host_clock_cannot_keep_a_held_quote_pricing_the_market() {
    let dir = scratch_dir("pace-stalled");
    let clock = Arc::new(TestClock::at(AT));
    let mut seq = fresh(&dir, &clock);
    let max_age = seq.engine.markets()[&1].spec.price_rules.max_age_ms;
    seq.sequence(prices(AT)).unwrap().expect("admitted");
    assert!(seq.engine.markets()[&1].index(seq.engine.now()).is_ok());

    // True and monotonic time move on while the host clock stands still, within the drift bound,
    // and every command's stamp is still admitted.
    let mut stalled = Duration::ZERO;
    while stalled.as_millis() <= (max_age as u128) * 101 / 100 {
        stalled += STEP;
        clock.advance(STEP);
        clock.skew(-(stalled.as_millis() as Millis));
        seq.sequence(Command::Tick).unwrap().expect("within the drift bound");
    }
    assert_eq!(clock.now_ms(), AT, "the host clock has not moved");
    assert!(seq.engine.now() > AT + max_age, "sequenced time kept pace: {}", seq.engine.now() - AT);
    assert_eq!(seq.engine.markets()[&1].index(seq.engine.now()), Err(PriceError::Stale { fresh: 0, needed: 2 }));
}

#[test]
fn the_first_stamp_of_a_run_is_past_every_quote_carried_across_its_restart() {
    let dir = scratch_dir("pace-restart");
    let clock = Arc::new(TestClock::at(AT));
    let mut seq = fresh(&dir, &clock);
    let max_age = seq.engine.markets()[&1].spec.price_rules.max_age_ms;
    seq.sequence(prices(AT)).unwrap().expect("admitted");
    seq.flush().unwrap();
    drop(seq);

    // The restart takes as long as the host likes, and the clock then reads what it likes within
    // the drift bound: here, just where it stopped.
    let down = Duration::from_secs(5);
    clock.advance(down);
    clock.skew(-(down.as_millis() as Millis));
    let mut seq = recovered(&dir, &clock);
    seq.trust(sample_around(&TrueTime(clock.clone()))).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("within the drift bound");
    assert_eq!(seq.engine.now(), AT + max_age + 1, "the first stamp past the quotes the log leaves");
    assert_eq!(seq.engine.markets()[&1].index(seq.engine.now()), Err(PriceError::Stale { fresh: 0, needed: 2 }));
}

#[test]
fn a_restart_after_the_held_quotes_have_aged_out_stamps_at_the_host_clock() {
    let dir = scratch_dir("pace-honest-restart");
    let clock = Arc::new(TestClock::at(AT));
    let mut seq = fresh(&dir, &clock);
    seq.sequence(prices(AT)).unwrap().expect("admitted");
    seq.flush().unwrap();
    drop(seq);

    clock.advance(Duration::from_secs(5));
    let mut seq = recovered(&dir, &clock);
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    assert_eq!(seq.engine.now(), clock.now_ms());
}

#[test]
fn an_honest_host_clock_is_what_stamps_read() {
    let dir = scratch_dir("pace-honest");
    let clock = Arc::new(TestClock::at(AT));
    let mut seq = fresh(&dir, &clock);
    for _ in 0..50 {
        clock.advance(STEP);
        seq.sequence(Command::Tick).unwrap().expect("admitted");
        assert_eq!(seq.engine.now(), clock.now_ms());
    }
}

// ── Helpers ──

/// A clock reading the test clock's true time, for a sample taken while the host clock is off it.
struct TrueTime(Arc<TestClock>);

impl Clock for TrueTime {
    /// True time.
    fn now_ms(&self) -> Millis {
        self.0.true_ms()
    }

    /// The test clock's monotonic time.
    fn instant(&self) -> std::time::Instant {
        self.0.instant()
    }
}

/// A sequencer over the log in `dir`, with `clock`.
fn recovered(dir: &Path, clock: &Arc<TestClock>) -> Sequencer {
    let genesis = finished_genesis(&log_entries(dir));
    let manifest = watch::channel(Manifest::default()).0;
    let cfg =
        SequencerConfig { clock: clock.clone(), max_drift: MAX_DRIFT, genesis, ..sequencer_config(dir, manifest) };
    let key = Arc::new(EnclaveKey::test_key());
    Sequencer::recover(cfg, key, broadcast::channel(16).0, mpsc::unbounded_channel().0).unwrap()
}

/// A sequencer on a fresh log at the latest protocol, under BTC-USD alone, with trusted time on `clock`.
fn fresh(dir: &Path, clock: &Arc<TestClock>) -> Sequencer {
    let mut seq = recovered(dir, clock);
    let mut genesis = dev_config().genesis;
    genesis.markets.retain(|market| market.id == 1);
    genesis.close_spread_ceilings.retain(|ceiling| ceiling.market == 1);
    genesis.benchmarks.clear();
    seq.activate_protocol().unwrap();
    seq.genesis(&genesis).unwrap();
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq
}

/// Two venues' quotes for BTC-USD, stamped `ts`.
fn prices(ts: Millis) -> Command {
    let quotes = ["venue-a", "venue-b"]
        .into_iter()
        .map(|source| SourceQuote { source: source.into(), mid: "100000".parse().unwrap(), ts, contract: None })
        .collect();
    Command::Prices { market: 1, quotes }
}
