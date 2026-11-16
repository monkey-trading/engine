//! The server's operating state, for `GET /v1/status` and the signed stream's `status` frames:
//! what stops the sequencer taking orders, trusted time, each venue connection, and the markets
//! with no price. The feed tasks publish their own state, and every other part is read from what
//! the node already keeps (trusted time's sample, the log-sync gauge, the poster's rollback latch)
//! and from the API's copy of the engine, so the sequencer does nothing for it, per price or ever.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use engine_core::Engine;
use engine_core::types::{MarketId, Millis};
use serde::Serialize;

use crate::api::pricing::{Pricing, Unpriced, pricing};
use crate::clock::now_ms;
use crate::feeds::status::{FeedStatus, VenueStatus};
use crate::trusted_time::TimeState;

/// What stops the sequencer taking orders.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Cause {
    /// Trusted time refuses stamps: no sample within the grace, or the host clock beyond the drift bound.
    TrustedTime,
    /// The oldest batch's log sync has been held past its bound.
    LogSyncHeld,
    /// A detected log rollback froze sequencing until the node restarts.
    LogRollback,
}

impl Cause {
    const ALL: [Cause; 3] = [Cause::TrustedTime, Cause::LogSyncHeld, Cause::LogRollback];
}

/// When each hold was first seen by a status read, or 0 while it does not hold. Cloned handles share them.
#[derive(Clone, Debug, Default)]
pub(crate) struct Operating(Arc<[AtomicU64; 3]>);

impl Operating {
    /// Records whether `cause` holds sequencing now; the time a read first saw it is kept while it goes on.
    pub(crate) fn hold(&self, cause: Cause, held: bool) {
        let slot = &self.0[cause as usize];
        if held == (slot.load(Ordering::Relaxed) != 0) {
            return;
        }
        slot.store(if held { now_ms().max(1).unsigned_abs() } else { 0 }, Ordering::Relaxed);
    }

    /// The causes holding sequencing, with when each began, in a fixed order.
    pub(crate) fn holds(&self) -> Vec<Hold> {
        Cause::ALL
            .into_iter()
            .filter_map(|cause| {
                let since = self.0[cause as usize].load(Ordering::Relaxed);
                (since != 0).then_some(Hold { cause, since_ms: since as Millis })
            })
            .collect()
    }
}

/// One cause holding sequencing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Hold {
    pub cause: Cause,
    pub since_ms: Millis,
}

/// Everything the node publishes about its own operation, shared by the tasks that publish it and
/// the API that serves it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Status {
    pub feeds: FeedStatus,
    pub operating: Operating,
    /// The log-head poster's rollback latch, which never clears before a restart.
    pub rollback: Option<crate::log_heads::FreezeLatch>,
    /// The gauge the sequencer already sets while a log sync is held past its bound.
    pub log_sync_held: Option<prometheus_client::metrics::gauge::Gauge>,
    /// The view the stream last read and when, so every status subscription shares one read an
    /// interval (see `ApiState::shared_operating_state`).
    pub shared: Arc<tokio::sync::Mutex<Option<(std::time::Instant, StatusView)>>>,
}

/// A market that is open and has no price.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct UnpricedMarket {
    pub market: MarketId,
    pub symbol: String,
    pub reason: Unpriced,
    pub fresh_sources: usize,
    pub min_sources: usize,
}

/// `GET /v1/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct StatusView {
    /// The node's time the answer was read at.
    pub as_of_ms: Millis,
    /// Whether the sequencer takes orders: no hold stops it. Reduce-only markets and unpriced markets
    /// refuse orders of their own, and are listed apart.
    pub takes_orders: bool,
    /// What stops it, each with when it began; empty while it takes orders.
    pub holds: Vec<Hold>,
    pub trusted_time: TimeState,
    /// Every price feed's venue, with the state of each of its symbols.
    pub venues: Vec<VenueStatus>,
    /// Markets open for trading with no price now; `GET /v1/markets` gives each one's sources.
    pub unpriced_markets: Vec<UnpricedMarket>,
}

impl Status {
    /// A status that reads the poster's rollback latch, if the node runs one.
    pub(crate) fn watching(poster: Option<&crate::log_heads::Publisher>, metrics: &crate::metrics::Metrics) -> Self {
        let rollback = poster.map(|poster| poster.freeze.clone());
        Self { rollback, log_sync_held: Some(metrics.log_sync_held.clone()), ..Self::default() }
    }

    /// The operating state at `engine`'s last stamp, with trusted time as `time` shows it.
    pub(crate) fn view(&self, engine: &Engine, time: TimeState) -> StatusView {
        // Every hold is read here, from what the sequencer and the poster already publish, so the
        // sequencer's path holds nothing for this read.
        let halted = matches!(time, TimeState::Held { .. });
        self.operating.hold(Cause::TrustedTime, halted);
        self.operating.hold(Cause::LogSyncHeld, self.log_sync_held.as_ref().is_some_and(|gauge| gauge.get() > 0));
        self.operating.hold(Cause::LogRollback, self.rollback.as_ref().is_some_and(|latch| latch.frozen()));
        let holds = self.operating.holds();
        let (now, venues) = (engine.now(), self.feeds.venues());
        let unpriced_markets = engine
            .markets()
            .values()
            .filter(|market| market.open)
            .filter_map(|market| {
                let pricing: Pricing = pricing(market, now, &venues);
                Some(UnpricedMarket {
                    market: market.spec.id,
                    symbol: market.spec.symbol.clone(),
                    reason: pricing.unpriced?,
                    fresh_sources: pricing.fresh_sources,
                    min_sources: pricing.min_sources,
                })
            })
            .collect();
        StatusView {
            as_of_ms: now_ms(),
            takes_orders: holds.is_empty(),
            holds,
            trusted_time: time,
            venues,
            unpriced_markets,
        }
    }
}
