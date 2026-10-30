//! The marks task: keeps the derived cache current from the sequencer's live mark reports, and
//! backfills provider history for listed markets the feeds map to a candles adapter. Supervised
//! like every other task: an error stops the node, since the cache shares the history directory's
//! disk. Samples a slow disk drops are re-derived at the next recovery, never roots.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use engine_core::types::MarketId;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, interval};
use tracing::{info, warn};

use crate::clock::now_ms;
use crate::config::BackfillSection;
use crate::history::marks::MarkSample;
use crate::history::marks::backfill::{self, Candles, Source};
use crate::history::marks::store::Store;
use crate::sequencer::{Query, Request};

/// Samples buffered before one write.
const BATCH: usize = 512;
/// How many samples the sequencer may have in flight; over it, a sample is dropped, never the
/// sequencer delayed.
pub(crate) const QUEUE: usize = 4096;
/// How often buffered samples flush, so a quiet market's latest mark reaches the cache.
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// How often a due backfill is attempted, one market at a time.
const BACKFILL_EVERY: Duration = Duration::from_secs(10);
/// How long after a failed backfill the market is retried.
const RETRY_AFTER: Duration = Duration::from_secs(300);

/// Runs until the sequencer's samples end: flushing live mark reports, then backfilling each
/// listed mapped market once per process (a restart, or the markers on disk, skip completed ones).
/// Error cases:
/// - a cache write fails, or the listed markets cannot be queried
pub async fn run(
    mut store: Store,
    mut samples: mpsc::Receiver<MarkSample>,
    backfill: Option<BackfillSection>,
    sources: BTreeMap<MarketId, Source>,
    // A weak sender: the task must never keep the sequencer's request channel open, or a shutdown
    // deadlocks — the sequencer would wait for this task's sender to drop while this task waits for
    // the sequencer's marks sender to drop.
    sequencer: mpsc::WeakSender<Request>,
) -> anyhow::Result<()> {
    let candles = backfill.as_ref().map(|cfg| Candles::new(&cfg.base_url)).transpose()?;
    let mut buffered = Vec::new();
    let mut flush = interval(FLUSH_EVERY);
    let mut backfill_tick = interval(BACKFILL_EVERY);
    let mut retry_at: BTreeMap<MarketId, Instant> = BTreeMap::new();
    loop {
        tokio::select! {
            sample = samples.recv() => {
                // The sequencer is gone: flush and end, as a shutdown does.
                let Some(sample) = sample else {
                    write(&mut store, &mut buffered)?;
                    return Ok(());
                };
                buffered.push(sample);
                if buffered.len() >= BATCH {
                    write(&mut store, &mut buffered)?;
                }
            }
            _ = flush.tick() => {
                if !buffered.is_empty() {
                    write(&mut store, &mut buffered)?;
                }
            }
            _ = backfill_tick.tick(), if candles.is_some() && backfill.is_some() => {
                backfill_due(
                    &mut store,
                    candles.as_ref().expect("armed by the guard"),
                    backfill.as_ref().expect("armed by the guard"),
                    &sources,
                    &sequencer,
                    &mut retry_at,
                )
                .await?;
            }
        }
    }
}

/// Flushes buffered samples into the cache, one transaction.
/// Error cases:
/// - the cache rejects the write
fn write(store: &mut Store, buffered: &mut Vec<MarkSample>) -> anyhow::Result<()> {
    store.observe(buffered).context("writing live marks")?;
    buffered.clear();
    Ok(())
}

/// Backfills one listed, mapped, not-yet-backfilled market, and retries a failed one after
/// `RETRY_AFTER`. Which market is unspecified: the listing order decides, one per tick.
/// Error cases:
/// - the listed markets cannot be queried, or a completed backfill's marker cannot be written
async fn backfill_due(
    store: &mut Store,
    candles: &Candles,
    cfg: &BackfillSection,
    sources: &BTreeMap<MarketId, Source>,
    sequencer: &mpsc::WeakSender<Request>,
    retry_at: &mut BTreeMap<MarketId, Instant>,
) -> anyhow::Result<()> {
    // The sequencer is gone (shutdown): no listing to query, and nothing else to do.
    let Some(sequencer) = sequencer.upgrade() else { return Ok(()) };
    let listed = listed_markets(&sequencer).await?;
    let done = store.backfilled().context("reading backfilled markets")?;
    for market in listed {
        if done.contains(&market) || retry_at.get(&market).is_some_and(|at| Instant::now() < *at) {
            continue;
        }
        let Some(source) = sources.get(&market) else { continue };
        match backfill::seed_market(candles, source, market, cfg, now_ms(), store).await {
            Ok(seeded) => {
                info!(market, venue = source.venue.name(), symbol = %source.symbol, seeded, "marks backfilled from the provider");
                store
                    .mark_backfilled(market, source.venue.name(), &source.symbol, now_ms())
                    .context("recording the backfill")?;
                retry_at.remove(&market);
            }
            Err(e) => {
                warn!(alert = "marks_backfill_failed", market, venue = source.venue.name(), "{e:#}");
                retry_at.insert(market, Instant::now() + RETRY_AFTER);
            }
        }
        return Ok(());
    }
    Ok(())
}

/// The markets the engine lists now, in id order.
/// Error cases:
/// - the sequencer does not answer
async fn listed_markets(sequencer: &mpsc::Sender<Request>) -> anyhow::Result<Vec<MarketId>> {
    let (reply, rx) = oneshot::channel();
    let query: Query = Box::new(move |engine: &engine_core::Engine| {
        let _ = reply.send(engine.markets().keys().copied().collect::<Vec<_>>());
    });
    sequencer
        .send(Request::Query(query))
        .await
        .map_err(|_| anyhow::anyhow!("querying listed markets: the sequencer is gone"))?;
    rx.await.map_err(|_| anyhow::anyhow!("querying listed markets: the sequencer is gone"))
}
