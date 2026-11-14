//! The marks task: keeps the derived cache current from the sequencer's live mark reports, and
//! backfills provider history for listed markets the feeds map to a candles adapter. Supervised
//! like every other task: an error stops the node, since the cache shares the history directory's
//! disk. Samples a slow disk drops are re-derived at the next full recovery without `[snapshots]`, never roots.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use engine_core::types::{MarketId, Millis};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, interval};
use tracing::{info, warn};

use crate::config::BackfillSection;
use crate::history::marks::MarkSample;
use crate::history::marks::backfill::{self, Candles, Source};
use crate::history::marks::fold::MarkFold;
use crate::history::marks::store::{Coverage, Store};
use crate::sequencer::{Query, Request};

/// Samples buffered before one write.
pub(crate) const BATCH: usize = 512;
/// How many samples the sequencer may have in flight; over it, a sample is dropped, never the
/// sequencer delayed.
pub(crate) const QUEUE: usize = 4096;
/// How often buffered samples flush, so a quiet market's latest mark reaches the cache.
pub(crate) const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// How often a due backfill is attempted, one market at a time.
const BACKFILL_EVERY: Duration = Duration::from_secs(10);
/// How long after a failed backfill the market is retried.
const RETRY_AFTER: Duration = Duration::from_secs(300);

/// The live-sample plumbing the task owns: the queue's receiver and the write buffer. A backfill
/// holds the task for minutes; it takes `&mut Live` and drains between page fetches, so the queue
/// cannot fill and drop samples while it works.
pub(crate) struct Live {
    pub samples: mpsc::Receiver<MarkSample>,
    pub buffered: Vec<MarkSample>,
    /// When the buffer last flushed; a backfill keeps this deadline between pages.
    pub flushed_at: Instant,
}

/// What a member's start leaves its marks cache to take once the node sequences: the cache its earlier
/// holding left, moved aside at activation, and the marks its replica folded while it followed.
#[derive(Default)]
pub(crate) struct CatchUp {
    pub prior: Option<PathBuf>,
    pub folded: Option<MarkFold>,
}

/// Runs until the sequencer's samples end: first taking a member's earlier history (`catch_up`)
/// beside the sequencer, after its first command; then flushing live mark reports, and backfilling
/// each listed mapped market once per process (a restart, or the markers on disk, skip completed
/// ones).
/// Error cases:
/// - a cache write fails, or the listed markets cannot be queried
pub async fn run(
    store: Store,
    // A weak sender: the task must never keep the sequencer's request channel open, or a shutdown
    // deadlocks — the sequencer would wait for this task's sender to drop while this task waits for
    // the sequencer's marks sender to drop.
    sequencer: mpsc::WeakSender<Request>,
    backfill: Option<BackfillSection>,
    sources: BTreeMap<MarketId, Source>,
    mut live: Live,
    catch_up: CatchUp,
) -> anyhow::Result<()> {
    let mut store = caught_up(store, catch_up, &mut live).await?;
    let candles = backfill.as_ref().map(|cfg| Candles::new(&cfg.base_url)).transpose()?;
    let mut flush = interval(FLUSH_EVERY);
    let mut backfill_tick = interval(BACKFILL_EVERY);
    let mut retry_at: BTreeMap<MarketId, Instant> = BTreeMap::new();
    loop {
        tokio::select! {
            sample = live.samples.recv() => {
                // The sequencer is gone: flush and end, as a shutdown does.
                let Some(sample) = sample else {
                    write(&mut store, &mut live.buffered)?;
                    return Ok(());
                };
                live.buffered.push(sample);
                if live.buffered.len() >= BATCH {
                    write(&mut store, &mut live.buffered)?;
                    live.flushed_at = Instant::now();
                }
            }
            _ = flush.tick() => {
                if !live.buffered.is_empty() {
                    write(&mut store, &mut live.buffered)?;
                    live.flushed_at = Instant::now();
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
                    &mut live,
                )
                .await?;
            }
        }
    }
}

/// Takes `work`, a member's earlier history, into `store` on a blocking thread, receiving the live
/// samples into the buffer meanwhile, so the sequencer's queue never fills and drops one, and writing
/// none of them before it: they extend the chains the catch-up writes below them. The backlog then
/// goes out a batch a write, the queue drained between writes.
/// Error cases:
/// - the catch-up fails or panics (see `Store::catch_up`), or a backlog write fails
async fn caught_up(mut store: Store, work: CatchUp, live: &mut Live) -> anyhow::Result<Store> {
    if work.prior.is_none() && work.folded.is_none() {
        return Ok(store);
    }
    let mut job = tokio::task::spawn_blocking(move || {
        store.catch_up(work.prior.as_deref(), work.folded)?;
        anyhow::Ok(store)
    });
    let mut open = true;
    let mut store = loop {
        tokio::select! {
            done = &mut job => break done.context("the marks catch-up stopped")??,
            sample = live.samples.recv(), if open => match sample {
                Some(sample) => live.buffered.push(sample),
                None => open = false,
            },
        }
    };
    while live.buffered.len() >= BATCH {
        let rest = live.buffered.split_off(BATCH);
        write(&mut store, &mut live.buffered)?;
        live.buffered = rest;
        while let Ok(sample) = live.samples.try_recv() {
            live.buffered.push(sample);
        }
        tokio::task::yield_now().await;
    }
    live.flushed_at = Instant::now();
    Ok(store)
}

/// Flushes buffered samples into the cache, one transaction.
/// Error cases:
/// - the cache rejects the write
fn write(store: &mut Store, buffered: &mut Vec<MarkSample>) -> anyhow::Result<()> {
    store.observe(buffered).context("writing live marks")?;
    buffered.clear();
    Ok(())
}

/// Backfills one listed, mapped market whose recorded coverage is not the depths the config asks
/// for now, and retries a failed one after `RETRY_AFTER`. Which market is unspecified: the
/// listing order decides, one per tick.
/// Error cases:
/// - the listed markets cannot be queried, a completed backfill's marker cannot be written, or the
///   cache rejects a write during the backfill
async fn backfill_due(
    store: &mut Store,
    candles: &Candles,
    cfg: &BackfillSection,
    sources: &BTreeMap<MarketId, Source>,
    sequencer: &mpsc::WeakSender<Request>,
    retry_at: &mut BTreeMap<MarketId, Instant>,
    live: &mut Live,
) -> anyhow::Result<()> {
    // The sequencer is gone (shutdown): no listing to query, and nothing else to do.
    let Some(sequencer) = sequencer.upgrade() else { return Ok(()) };
    let (listed, engine_now) = listed_markets(&sequencer).await?;
    // No command sequenced yet (genesis waits for trusted time): no proven time to anchor on.
    if engine_now == 0 {
        return Ok(());
    }
    let coverage = Coverage {
        depths: (
            Millis::try_from(cfg.depth_1m.as_millis()).context("backfill depth_1m")?,
            Millis::try_from(cfg.depth_1h.as_millis()).context("backfill depth_1h")?,
            Millis::try_from(cfg.depth_1d.as_millis()).context("backfill depth_1d")?,
        ),
        retentions: {
            let r = store.retentions();
            (r.s1, r.m1, r.h1, r.d1)
        },
    };
    let done = store.backfilled().context("reading backfilled markets")?;
    for market in listed {
        // A market is done when the recorded backfill covers the depths and retentions the config
        // now asks for; a wider retention re-runs it, because the original seed pruned more.
        let covered = done.get(&market).is_some_and(|recorded| *recorded == coverage);
        if covered || retry_at.get(&market).is_some_and(|at| Instant::now() < *at) {
            continue;
        }
        let Some(source) = sources.get(&market) else { continue };
        match backfill::seed_market(candles, source, market, cfg, engine_now, store, live).await {
            Ok(seeded) => {
                info!(market, venue = source.venue.name(), symbol = %source.symbol, seeded, "marks backfilled from the provider");
                store
                    .mark_backfilled(market, source.venue.name(), &source.symbol, engine_now, coverage)
                    .context("recording the backfill")?;
                retry_at.remove(&market);
            }
            Err(e) if e.downcast_ref::<backfill::StoreFault>().is_some() => return Err(e),
            Err(e) => {
                warn!(alert = "marks_backfill_failed", market, venue = source.venue.name(), "{e:#}");
                retry_at.insert(market, Instant::now() + RETRY_AFTER);
            }
        }
        return Ok(());
    }
    Ok(())
}

/// The markets the engine lists now, in id order, with the engine's time: the admitted stamp the
/// backfill anchors its windows and pruning on, never the host clock.
/// Error cases:
/// - the sequencer does not answer
async fn listed_markets(sequencer: &mpsc::Sender<Request>) -> anyhow::Result<(Vec<MarketId>, Millis)> {
    let (reply, rx) = oneshot::channel();
    let query: Query = Box::new(move |engine: &engine_core::Engine| {
        let _ = reply.send((engine.markets().keys().copied().collect::<Vec<_>>(), engine.now()));
    });
    sequencer
        .send(Request::Query(query))
        .await
        .map_err(|_| anyhow::anyhow!("querying listed markets: the sequencer is gone"))?;
    rx.await.map_err(|_| anyhow::anyhow!("querying listed markets: the sequencer is gone"))
}
