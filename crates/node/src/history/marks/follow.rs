//! The provider backfill of a member that follows (#884). It holds no enclave key, so it cannot write
//! the cache's authenticated rows; but the candles it fetches need nothing from its turn. A task
//! fetches each listed market's depth while the member follows and stages the rows beside the folded
//! marks (`MarkFold::stage`), and the marks task seeds them with the rest of the member's history,
//! so the first answer after its start already holds the depth the operator it succeeds served.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_core::Engine;
use engine_core::types::{MarketId, Millis};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{info, warn};

use crate::config::{BackfillSection, NodeConfig};
use crate::history::marks::Retentions;
use crate::history::marks::backfill::{Candles, Source, fetch_market, sources};
use crate::history::marks::run::coverage;
use crate::history::marks::store::{Coverage, Seed};

/// How often the task looks for a listed market still to fetch.
const EVERY: Duration = if cfg!(test) { Duration::from_millis(20) } else { Duration::from_secs(10) };
/// How long after a failed fetch the market is tried again.
const RETRY_AFTER: Duration = if cfg!(test) { Duration::from_millis(200) } else { Duration::from_secs(300) };

/// One market's completed fetch, as its marker in the cache records it.
#[derive(Debug)]
pub(crate) struct Done {
    pub market: MarketId,
    pub source: Source,
    pub at: Millis,
    pub coverage: Coverage,
}

/// The provider history fetched while following, whole markets only.
#[derive(Debug, Default)]
pub(crate) struct Staged {
    pub rows: Vec<Seed>,
    pub done: Vec<Done>,
}

#[derive(Default)]
struct Shared {
    /// The markets the replica lists and its time, the stamp the fetch anchors on.
    listing: (Vec<MarketId>, Millis),
    staged: Staged,
}

/// The fetch running while a member follows.
pub(crate) struct Following {
    shared: Arc<Mutex<Shared>>,
    task: JoinHandle<()>,
}

impl Following {
    /// Starts the fetch `cfg` asks for, `None` when its backfill is off or no feed maps to a candles
    /// adapter.
    /// Error cases:
    /// - the candles client or the retentions cannot be built
    pub(crate) fn start(cfg: &NodeConfig) -> anyhow::Result<Option<Self>> {
        let marks = cfg.history.marks.as_ref();
        let Some(backfill) = marks.and_then(|marks| marks.backfill.clone()).filter(|backfill| backfill.enabled) else {
            return Ok(None);
        };
        let sources = sources(&cfg.feeds);
        if sources.is_empty() {
            return Ok(None);
        }
        let candles = Candles::new(&backfill.base_url)?;
        let covers = coverage(&backfill, Retentions::of(marks)?)?;
        Ok(Some(Self::spawn(candles, backfill, sources, covers)))
    }

    fn spawn(candles: Candles, cfg: BackfillSection, sources: BTreeMap<MarketId, Source>, covers: Coverage) -> Self {
        let shared = Arc::<Mutex<Shared>>::default();
        let task = tokio::spawn(fetch(shared.clone(), candles, cfg, sources, covers));
        Self { shared, task }
    }

    /// Tells the task what `engine`, the replica's, lists and what time it has reached.
    pub(crate) fn observe(&self, engine: &Engine) {
        self.list(engine.markets().keys().copied().collect(), engine.now());
    }

    fn list(&self, markets: Vec<MarketId>, now: Millis) {
        self.shared.lock().expect("the staged fetch is never poisoned").listing = (markets, now);
    }

    /// Stops the fetch and returns what it completed; a market it was part way through is dropped.
    pub(crate) async fn finish(mut self) -> Staged {
        self.task.abort();
        let _ = (&mut self.task).await;
        std::mem::take(&mut self.shared.lock().expect("the staged fetch is never poisoned").staged)
    }
}

impl Drop for Following {
    /// A member whose wait is cancelled, by a shutdown, fetches no more.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Fetches, one market at a time, each listed mapped market once, retrying a failed one later.
async fn fetch(
    shared: Arc<Mutex<Shared>>,
    candles: Candles,
    cfg: BackfillSection,
    sources: BTreeMap<MarketId, Source>,
    covers: Coverage,
) {
    let mut retry_at: BTreeMap<MarketId, Instant> = BTreeMap::new();
    loop {
        tokio::time::sleep(EVERY).await;
        let next = {
            let shared = shared.lock().expect("the staged fetch is never poisoned");
            let (listed, now) = &shared.listing;
            let waiting = |market: &MarketId| retry_at.get(market).is_some_and(|at| Instant::now() < *at);
            let done = |market: &MarketId| shared.staged.done.iter().any(|done| done.market == *market);
            // No command applied yet: no proven time to anchor the windows on.
            listed
                .iter()
                .filter(|market| *now != 0 && !done(market) && !waiting(market))
                .find_map(|market| sources.get(market).map(|source| (*market, source.clone(), *now)))
        };
        let Some((market, source, now)) = next else { continue };
        match fetch_market(&candles, &source, market, &cfg, now).await {
            Ok(rows) => {
                info!(
                    market,
                    venue = source.venue.name(),
                    rows = rows.len(),
                    "fetched provider history while following"
                );
                let mut shared = shared.lock().expect("the staged fetch is never poisoned");
                shared.staged.rows.extend(rows);
                shared.staged.done.push(Done { market, source, at: now, coverage: covers });
            }
            Err(e) => {
                warn!(alert = "marks_backfill_failed", market, venue = source.venue.name(), "{e:#}");
                retry_at.insert(market, Instant::now() + RETRY_AFTER);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
