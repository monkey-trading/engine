//! #867's faults, injected one at a time through the links in front of the crypto stand-ins
//! (`feeds::tests::link`), the stand-ins' quiet products and the node's trusted time, in turn on the
//! soak's cadence. Each lasts a fixed time, and once it ends every market must be priced again
//! within the deadline `scripts/dev-venue-load.sh` holds: `DEADLINE`, or `reconnect_max` more after
//! an outage of both venues or of trusted time, which every feed backs off through.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use engine_core::types::MarketId;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, timeout_at};

use crate::config::Venue;
use crate::feeds::tests::link::{Faults, FaultyLink};
use crate::feeds::venue::Timing;
use crate::load_scenario::soak::timeline::FEED_ALERTS;
use crate::load_scenario::unpriced;
use crate::load_scenario::venues::Quiet;
use crate::sequencer::Request;

/// How soon every market is priced once its venues stream again (`feeds::tests::venue_load`).
pub(super) const DEADLINE: Duration = Duration::from_secs(10);
/// The market that goes quiet: ETH-USD, while BTC-USD and SOL-USD stream.
const QUIET_MARKET: MarketId = 2;
/// What a slow link passes, in bytes a second: a few hundred bytes of each symbol's book go ahead of a
/// pong, so the connection's delay passes the template's bound.
const SLOW_BYTES_PER_SECOND: u64 = 2_000;
/// Past `trusted_time::NO_SAMPLE_GRACE` (60 s) without a sample sequencing halts.
const TIME_OUT: Duration = Duration::from_secs(75);

/// Every fault, in the order the soak injects them.
pub(super) const ALL: [Fault; 6] =
    [Fault::Stalled, Fault::Dropped, Fault::Slow, Fault::Quiet, Fault::BothDown, Fault::TimeDown];

/// A fault, by what it takes from the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Fault {
    /// Coinbase's connections hold what it sends, its data queueing behind the stall.
    Stalled,
    /// Kraken's connections are dropped and refused.
    Dropped,
    /// Coinbase's connections pass a few kilobytes a second, behind which the pongs queue.
    Slow,
    /// ETH-USD goes quiet at every venue while the other markets stream.
    Quiet,
    /// Both venues are dropped and refused.
    BothDown,
    /// Trusted time takes no sample, so past its grace sequencing halts.
    TimeDown,
}

impl Fault {
    pub(super) fn name(self) -> &'static str {
        match self {
            Fault::Stalled => "stalled",
            Fault::Dropped => "dropped",
            Fault::Slow => "slow_link",
            Fault::Quiet => "quiet_product",
            Fault::BothDown => "both_venues_down",
            Fault::TimeDown => "trusted_time_down",
        }
    }

    /// How long the fault lasts.
    pub(super) fn lasting(self) -> Duration {
        match self {
            Fault::Stalled | Fault::Slow | Fault::BothDown => Duration::from_secs(15),
            Fault::Dropped => Duration::from_secs(10),
            // The market goes unpriced once its quotes age out, and alerts 5 s after.
            Fault::Quiet => Duration::from_secs(20),
            Fault::TimeDown => TIME_OUT,
        }
    }

    /// The alert a fault that unprices a market must raise, once its market has been unpriced for
    /// `alerts.no_price_after`; `None` for a fault that may leave every market priced.
    pub(super) fn alert(self) -> Option<&'static str> {
        match self {
            Fault::Stalled | Fault::Dropped | Fault::Quiet | Fault::BothDown => Some("market_no_price"),
            Fault::TimeDown => Some("trusted_time_unavailable"),
            Fault::Slow => None,
        }
    }

    /// The alerts the fault may raise: those of a feed that loses a source, and for trusted time its own.
    pub(super) fn explains(self) -> Vec<&'static str> {
        let mut alerts = FEED_ALERTS.to_vec();
        if self == Fault::TimeDown {
            alerts.push("trusted_time_unavailable");
        }
        alerts
    }

    /// How soon after the fault ends every market is priced again.
    pub(super) fn deadline(self, timing: &Timing) -> Duration {
        match self {
            Fault::BothDown | Fault::TimeDown => timing.reconnect_max + DEADLINE,
            _ => DEADLINE,
        }
    }
}

/// The fault to inject at the `at`th cadence, each in turn.
pub(super) fn nth(at: usize) -> Fault {
    ALL[at % ALL.len()]
}

/// What a fault is injected through.
#[derive(Clone)]
pub(super) struct Rig {
    pub links: Arc<BTreeMap<Venue, FaultyLink>>,
    pub quiet: Quiet,
    pub time_out: Arc<AtomicBool>,
    pub tx: mpsc::Sender<Request>,
    pub timing: Timing,
}

/// A fault that has run: its window and how long after it ended every market was priced again, or why
/// one was not within its deadline.
#[derive(Clone, Debug)]
pub(super) struct Done {
    pub fault: Fault,
    pub began: Instant,
    pub priced_again: Result<Duration, String>,
}

impl Rig {
    fn links(&self, venues: &[Venue], faults: Faults) {
        venues.iter().for_each(|venue| self.links[venue].set(faults));
    }

    /// Injects `fault` for its time, lifts it, and waits for every market to be priced again.
    pub(super) async fn inject(&self, fault: Fault) -> Done {
        let (stall, down) = (Faults { stalled: true, ..Faults::default() }, Faults { down: true, ..Faults::default() });
        let coinbase = [Venue::Coinbase, Venue::CoinbaseExchange];
        let all = [Venue::Coinbase, Venue::CoinbaseExchange, Venue::Kraken];
        let began = Instant::now();
        match fault {
            Fault::Stalled => self.links(&coinbase, stall),
            Fault::Dropped => self.links(&[Venue::Kraken], down),
            Fault::Slow => {
                // A reconnect's snapshots arrive behind the throttle, as on a slow link.
                self.links(&coinbase, down);
                sleep(Duration::from_millis(200)).await;
                self.links(&coinbase, Faults { rate: Some(SLOW_BYTES_PER_SECOND), ..Faults::default() });
            }
            Fault::Quiet => self.quiet.set(QUIET_MARKET, true),
            Fault::BothDown => self.links(&all, down),
            Fault::TimeDown => self.time_out.store(true, Ordering::Relaxed),
        }
        sleep(fault.lasting()).await;
        self.links(&all, Faults::default());
        self.quiet.set(QUIET_MARKET, false);
        self.time_out.store(false, Ordering::Relaxed);
        let priced_again = priced_within(&self.tx, fault.deadline(&self.timing)).await;
        Done { fault, began, priced_again }
    }
}

/// How long every market takes to be priced, or why they were not within `within`: the markets still
/// unpriced, or a query to the sequencer the deadline passed in.
pub(super) async fn priced_within(tx: &mpsc::Sender<Request>, within: Duration) -> Result<Duration, String> {
    let started = Instant::now();
    loop {
        let unpriced = timeout_at(started + within, unpriced(tx))
            .await
            .map_err(|_| format!("the sequencer did not answer a pricing query within {within:?}"))?;
        if unpriced.is_empty() && started.elapsed() <= within {
            return Ok(started.elapsed());
        }
        if started.elapsed() >= within {
            return Err(format!("markets {unpriced:?} unpriced {within:?} after the start of the wait"));
        }
        sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fault_is_injected_in_turn_and_the_turn_wraps() {
        let first: Vec<Fault> = (0..ALL.len()).map(nth).collect();
        assert_eq!(first, ALL);
        assert_eq!(nth(ALL.len()), Fault::Stalled);
        let names: std::collections::BTreeSet<_> = ALL.iter().map(|fault| fault.name()).collect();
        assert_eq!(names.len(), ALL.len(), "each fault has its own name");
    }

    #[test]
    fn an_outage_of_both_venues_or_of_time_gets_the_reconnect_wait_and_the_others_do_not() {
        let timing = Timing::for_venues(&crate::load_scenario::venues::template_venues());
        for fault in ALL {
            let outage = matches!(fault, Fault::BothDown | Fault::TimeDown);
            let expected = if outage { timing.reconnect_max + DEADLINE } else { DEADLINE };
            assert_eq!(fault.deadline(&timing), expected, "{fault:?}");
        }
        assert!(timing.reconnect_max > Duration::ZERO);
    }

    #[test]
    fn a_fault_explains_the_feed_alerts_and_only_trusted_times_own_adds_its_alert() {
        assert!(Fault::Stalled.explains().contains(&"market_no_price"));
        assert!(!Fault::Stalled.explains().contains(&"trusted_time_unavailable"));
        assert!(Fault::TimeDown.explains().contains(&"trusted_time_unavailable"));
        assert!(!Fault::Stalled.explains().contains(&"snapshot_write_failed"));
    }

    #[tokio::test]
    async fn a_pricing_query_the_sequencer_never_answers_fails_at_the_deadline() {
        let (tx, mut held) = mpsc::channel(4);
        let started = Instant::now();
        let failed = priced_within(&tx, Duration::from_millis(200)).await;
        assert!(failed.is_err_and(|why| why.contains("did not answer")), "the wait gave up");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(held.try_recv().is_ok(), "the query was sent");
    }

    #[test]
    fn trusted_time_stays_out_past_its_grace_so_sequencing_halts() {
        assert!(Fault::TimeDown.lasting() > crate::trusted_time::NO_SAMPLE_GRACE);
    }
}
