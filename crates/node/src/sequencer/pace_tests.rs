//! Sequenced time on a test clock against a host clock that stalls or steps back: a quote the engine
//! holds ages out on monotonic time, and a run's first stamp is past every quote carried across
//! its restart.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
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
const MAX_DELAY: Duration = Duration::from_secs(2);
/// How long a hostile host holds a quote back, within the drift bound.
const HELD: Duration = Duration::from_secs(5);

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
    // While stamps hold there, a quote stored at that stamp would count for longer: none is taken.
    let fresh_quote = Some(clock.instant() + MAX_DELAY);
    assert!(seq.sequence_quote(prices(seq.engine.now()), fresh_quote).unwrap().is_none());
    assert_eq!(seq.engine.markets()[&1].quotes["venue-a"].ts, AT, "only the carried quote is held");

    // Once the pace from the last stamp passes the hold, stamps follow it, not the hold.
    clock.advance(Duration::from_millis(max_age as u64 + 100));
    seq.sequence(Command::Tick).unwrap().expect("within the drift bound");
    assert_eq!(seq.engine.now(), AT + max_age + 100);
    let quote = Some(clock.instant() + MAX_DELAY);
    assert!(seq.sequence_quote(prices(seq.engine.now()), quote).unwrap().is_some());
}

#[test]
fn a_quote_held_back_past_its_deadline_is_dropped_however_fresh_its_stamp_looks() {
    let dir = scratch_dir("pace-deadline");
    let clock = Arc::new(TestClock::at(AT));
    // The host clock runs behind true time, within the drift bound.
    clock.skew(-(HELD.as_millis() as Millis));
    let mut seq = fresh(&dir, &clock);
    let not_after = clock.instant() + MAX_DELAY;
    let stamp = clock.true_ms();

    // The feed has read and queued a quote, and the host stalls the sequencer's log for as long as
    // its clock is behind, so sequenced time reaches the quote's stamp just as it is sequenced.
    clock.advance(HELD);
    assert!(seq.sequence_quote(prices(stamp), Some(not_after)).unwrap().is_none());
    assert_eq!(seq.engine.markets()[&1].quotes.len(), 0);

    // Within its deadline, or on a connection with no pings, the same quote prices the market: only
    // the deadline kept it out.
    assert!(seq.sequence_quote(prices(stamp), Some(clock.instant() + MAX_DELAY)).unwrap().is_some());
    assert!(seq.engine.markets()[&1].index(seq.engine.now()).is_ok());
    clock.advance(Duration::from_secs(1));
    let later = clock.now_ms();
    assert!(seq.sequence_quote(prices(later), None).unwrap().is_some());
    assert_eq!(seq.engine.markets()[&1].quotes["venue-a"].ts, later);
}

#[test]
fn a_quote_is_judged_on_the_clock_readings_its_stamp_is_made_from() {
    let dir = scratch_dir("pace-one-reading");
    let clock = Arc::new(TestClock::at(AT));
    fresh(&dir, &clock).flush().unwrap();
    // The host stalls the CPU between the sequencer's reads: from the second read on, monotonic
    // time is `HELD` later than the first saw.
    let stalling = Arc::new(StallingClock {
        inner: clock.clone(),
        armed: AtomicBool::new(false),
        stalled: AtomicBool::new(false),
        jump: HELD,
    });
    let mut seq = sequencer_on(&dir, stalling.clone());
    seq.trust(sample_around(&TrueTime(clock.clone()))).unwrap();
    stalling.armed.store(true, Ordering::SeqCst);
    let not_after = clock.instant() + MAX_DELAY;
    assert!(seq.sequence_quote(prices(clock.now_ms()), Some(not_after)).unwrap().is_some());
    // Judged and stamped on one reading: the stamp is the host clock's, not the stall's pace.
    assert_eq!(seq.engine.now(), clock.now_ms());
}

#[test]
fn a_first_stamp_past_what_trusted_time_admits_is_held_to_it_rather_than_halting() {
    let dir = scratch_dir("pace-long-age");
    let clock = Arc::new(TestClock::at(AT));
    let mut seq = fresh_with(&dir, &clock, MAX_DRIFT.as_millis() as Millis * 6);
    seq.sequence(prices(AT)).unwrap().expect("admitted");
    seq.flush().unwrap();
    drop(seq);

    let mut seq = recovered(&dir, &clock);
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("sequencing does not halt");
    // The sample allows AT - 1s at the earliest, and a stamp may be the drift bound past that.
    assert_eq!(seq.engine.now(), AT - 1_000 + MAX_DRIFT.as_millis() as Millis);

    // Once the hold is over, stamps are back on the honest host clock: nothing ran on ahead of it
    // for a market with a shorter `max_age_ms` to lose its price to.
    clock.advance(Duration::from_millis(MAX_DRIFT.as_millis() as u64 * 6 + 100));
    seq.trust(sample_around(clock.as_ref())).unwrap();
    seq.sequence(Command::Tick).unwrap().expect("admitted");
    assert_eq!(seq.engine.now(), clock.now_ms());
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

/// A clock that reads `inner` until armed, and from the read after that one on is `jump` later in
/// monotonic time, as the host stalling the CPU between two reads makes it.
struct StallingClock {
    inner: Arc<TestClock>,
    armed: AtomicBool,
    stalled: AtomicBool,
    jump: Duration,
}

impl Clock for StallingClock {
    /// The inner clock's wall time.
    fn now_ms(&self) -> Millis {
        self.inner.now_ms()
    }

    /// The inner clock's monotonic time, `jump` later after the first read once armed.
    fn instant(&self) -> std::time::Instant {
        if self.stalled.load(Ordering::SeqCst) {
            return self.inner.instant() + self.jump;
        }
        if self.armed.load(Ordering::SeqCst) {
            self.stalled.store(true, Ordering::SeqCst);
        }
        self.inner.instant()
    }
}

/// A sequencer over the log in `dir`, with `clock`.
fn recovered(dir: &Path, clock: &Arc<TestClock>) -> Sequencer {
    sequencer_on(dir, clock.clone())
}

/// A sequencer over the log in `dir`, reading `clock`.
fn sequencer_on(dir: &Path, clock: Arc<dyn Clock>) -> Sequencer {
    let genesis = finished_genesis(&log_entries(dir));
    let manifest = watch::channel(Manifest::default()).0;
    let cfg = SequencerConfig { clock, max_drift: MAX_DRIFT, genesis, ..sequencer_config(dir, manifest) };
    let key = Arc::new(EnclaveKey::test_key());
    Sequencer::recover(cfg, key, broadcast::channel(16).0, mpsc::unbounded_channel().0).unwrap()
}

/// A sequencer on a fresh log at the latest protocol, under BTC-USD alone, with trusted time on
/// `clock`'s true time.
fn fresh(dir: &Path, clock: &Arc<TestClock>) -> Sequencer {
    fresh_with(dir, clock, dev_config().genesis.markets[0].price_rules.max_age_ms)
}

/// `fresh`, with BTC-USD's `max_age_ms` set.
fn fresh_with(dir: &Path, clock: &Arc<TestClock>, max_age_ms: Millis) -> Sequencer {
    let mut seq = recovered(dir, clock);
    let mut genesis = dev_config().genesis;
    genesis.markets.retain(|market| market.id == 1);
    genesis.markets[0].price_rules.max_age_ms = max_age_ms;
    genesis.close_spread_ceilings.retain(|ceiling| ceiling.market == 1);
    genesis.benchmarks.clear();
    seq.activate_protocol().unwrap();
    seq.genesis(&genesis).unwrap();
    seq.trust(sample_around(&TrueTime(clock.clone()))).unwrap();
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
