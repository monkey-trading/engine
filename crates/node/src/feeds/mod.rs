//! Price feeds. Each feed handler turns venue data into `Command::Prices`. The engine builds the
//! index (median, staleness, divergence, and the roll adjustment of a market priced from futures),
//! so the aggregation rules are protocol and the raw venue quotes are in the public log for anyone
//! to check against venue history.
//!
//! `venue` runs one supervised task per venue connection; each venue's adapter builds its opening
//! messages and reads its messages into books: the crypto venues (`bitstamp`, `coinbase`,
//! `kraken`), and the market-data vendors that price FX, index and commodity futures, and US
//! stocks (`massive`, `tradermade`, `databento`). A new source is an adapter plus a `Venue`
//! variant. The mock feed serves local development.

mod bitstamp;
mod coinbase;
mod databento;
mod kraken;
mod massive;
mod tradermade;
pub mod venue;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use engine_core::command::Command;
use engine_core::corporate::Adjustment;
use engine_core::fixed::Rounding;
use engine_core::price::SourceQuote;
use engine_core::types::{MarketId, Millis};
use engine_core::{Engine, Fixed};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, interval};

use crate::clock::now_ms;
use crate::config::{FeedProvider, FeedsSection};
use crate::sequencer::{Query, Request};

const MOCK_VENUES: [&str; 3] = ["mock-a", "mock-b", "mock-c"];

/// The sources `cfg` feeds each market from: the quote sources the engine is still sent.
pub fn configured_sources(cfg: &FeedsSection) -> BTreeMap<MarketId, BTreeSet<String>> {
    let pairs: Vec<(MarketId, String)> = match cfg.provider {
        FeedProvider::Mock => {
            cfg.mock_mids.iter().flat_map(|m| MOCK_VENUES.map(|venue| (m.market, venue.to_owned()))).collect()
        }
        FeedProvider::Venues => cfg
            .venues
            .iter()
            .flat_map(|venues| &venues.connections)
            .flat_map(|conn| conn.symbols.iter().map(|s| (s.market, conn.venue.name().to_owned())))
            .collect(),
    };
    pairs.into_iter().fold(BTreeMap::new(), |mut sources, (market, venue)| {
        sources.entry(market).or_default().insert(venue);
        sources
    })
}

/// A scheduled split: its market, its opening, and its `new_units` and `old_units`.
pub(super) type Split = (MarketId, Millis, u32, u32);

/// What the mock follows of the engine's splits.
#[derive(Debug, Default, PartialEq)]
pub(super) struct SplitView {
    /// Each pending split, by id.
    pub(super) pending: BTreeMap<String, Split>,
    /// The mark of each market that has split before, in its current share units.
    pub(super) marks: BTreeMap<MarketId, Fixed>,
    /// The `last_split_at` of each market that has split before.
    pub(super) last_split: BTreeMap<MarketId, Millis>,
}

/// Each market's walk, and the splits it has seen scheduled and already followed.
pub(super) struct Walks {
    pub(super) mids: Vec<(MarketId, Fixed, Option<Duration>)>,
    scheduled: BTreeMap<String, Split>,
    followed: BTreeSet<String>,
    /// The `last_split_at` each market held the last time the walk looked, so a move the walk did
    /// not follow is a split to re-anchor on.
    seen_last_split: BTreeMap<MarketId, Millis>,
    /// The opening of the latest split the walk followed, per market: the `last_split_at` the
    /// engine records when it applies that split.
    followed_openings: BTreeMap<MarketId, Millis>,
}

impl Walks {
    /// Walks from each market's configured mid, or from its current mark once it has split. Keeps
    /// the startup view's pending splits, so one a ticker applies before the next view is still
    /// followed.
    pub(super) fn start(cfg: &FeedsSection, view: &SplitView) -> Self {
        let mids = cfg
            .mock_mids
            .iter()
            .map(|m| (m.market, view.marks.get(&m.market).copied().unwrap_or(m.mid), m.stop_after))
            .collect();
        Walks {
            mids,
            scheduled: view.pending.clone(),
            followed: BTreeSet::new(),
            seen_last_split: view.last_split.clone(),
            followed_openings: BTreeMap::new(),
        }
    }

    /// Remembers each split seen pending, and rescales a market's walk by `old / new` once a quote
    /// stamped `ts` would fall at or after that split's opening, so the quote is in the new units.
    /// A market whose engine `last_split_at` moves to an opening the walk did not follow
    /// re-anchors to the engine's mark, in the market's current share units.
    pub(super) fn follow(&mut self, view: SplitView, ts: Millis) {
        self.scheduled.extend(view.pending.into_iter().filter(|(id, _)| !self.followed.contains(id)));
        let due: Vec<_> =
            self.scheduled.iter().filter(|(_, (_, ex_open, ..))| *ex_open <= ts).map(|(id, _)| id.clone()).collect();
        for id in due {
            let (market, ex_open, new, old) = self.scheduled.remove(&id).expect("selected above");
            for (_, mid, _) in self.mids.iter_mut().filter(|(quoted, ..)| *quoted == market) {
                *mid = mid.mul_div(Fixed::from_int(old.into()), Fixed::from_int(new.into()), Rounding::TowardZero);
            }
            self.followed.insert(id);
            self.followed_openings.insert(market, ex_open);
        }
        for (market, at) in view.last_split {
            if self.seen_last_split.get(&market) == Some(&at) {
                continue;
            }
            if self.followed_openings.get(&market) != Some(&at)
                && let Some(mark) = view.marks.get(&market)
            {
                for (_, mid, _) in self.mids.iter_mut().filter(|(quoted, ..)| *quoted == market) {
                    *mid = *mark;
                }
            }
            self.seen_last_split.insert(market, at);
        }
    }
}

/// A random walk per market, quoted by three venues that differ by about a bp, until the market's
/// `stop_after`, and only while the engine's hours have it open, since the engine refuses a quote
/// from outside them. A walk stays in its market's current share units across splits. Returns only
/// when the sequencer has gone away.
pub async fn run_mock(cfg: &FeedsSection, tx: mpsc::Sender<Request>) {
    let mut rng = StdRng::from_entropy();
    let Some((_, view)) = market_view(&tx, now_ms()).await else { return };
    let mut walks = Walks::start(cfg, &view);
    let started = Instant::now();
    let mut every = interval(cfg.interval);
    loop {
        every.tick().await;
        let ts = now_ms();
        let Some((closed, view)) = market_view(&tx, ts).await else { return };
        walks.follow(view, ts);
        let updates: Vec<Command> = walks
            .mids
            .iter_mut()
            .filter(|(_, _, stop_after)| stop_after.is_none_or(|after| started.elapsed() < after))
            .filter(|(market, _, _)| !closed.contains(market))
            .map(|(market, mid, _)| {
                *mid += mid.mul_bps(rng.gen_range(-2..=2));
                let quotes = MOCK_VENUES
                    .iter()
                    .map(|venue| SourceQuote {
                        source: (*venue).to_owned(),
                        mid: *mid + mid.mul_bps(rng.gen_range(-1..=1)),
                        ts,
                        contract: None,
                    })
                    .collect();
                Command::Prices { market: *market, quotes }
            })
            .collect();
        for cmd in updates {
            if tx.send(Request::Submit { cmd, reply: None }).await.is_err() {
                return;
            }
        }
    }
}

/// The markets closed at `ts` and the splits the mock follows, as the engine holds them, or `None`
/// once the sequencer has gone away.
async fn market_view(tx: &mpsc::Sender<Request>, ts: Millis) -> Option<(BTreeSet<MarketId>, SplitView)> {
    let (reply, rx) = oneshot::channel();
    let query: Query = Box::new(move |engine: &Engine| {
        let closed = engine
            .markets()
            .iter()
            .filter(|(_, market)| !market.spec.schedule.is_open(ts))
            .map(|(id, _)| *id)
            .collect();
        let _ = reply.send((closed, split_view(engine)));
    });
    tx.send(Request::Query(query)).await.ok()?;
    rx.await.ok()
}

/// Every pending split, the mark of every market that has split before, and each such market's
/// `last_split_at`.
pub(super) fn split_view(engine: &Engine) -> SplitView {
    let pending = engine
        .corporate_actions()
        .iter()
        .filter_map(|(id, pending)| match pending.instruction.adjustment {
            Adjustment::Split { new_units, old_units } => {
                Some((id.clone(), (pending.instruction.market, pending.instruction.ex_open, new_units, old_units)))
            }
            Adjustment::Dividend { .. } => None,
        })
        .collect();
    let marks = engine
        .markets()
        .iter()
        .filter(|(_, market)| market.last_split_at.is_some())
        .filter_map(|(id, market)| market.mark.map(|mark| (*id, mark)))
        .collect();
    let last_split =
        engine.markets().iter().filter_map(|(id, market)| market.last_split_at.map(|at| (*id, at))).collect();
    SplitView { pending, marks, last_split }
}

#[cfg(test)]
mod tests;
