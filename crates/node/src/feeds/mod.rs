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
}

/// Each market's walk, and the splits it has seen scheduled and already followed.
pub(super) struct Walks {
    pub(super) mids: Vec<(MarketId, Fixed, Option<Duration>)>,
    scheduled: BTreeMap<String, Split>,
    followed: BTreeSet<String>,
}

impl Walks {
    /// Walks from each market's configured mid, or from its current mark once it has split.
    pub(super) fn start(cfg: &FeedsSection, marks: &BTreeMap<MarketId, Fixed>) -> Self {
        let mids = cfg
            .mock_mids
            .iter()
            .map(|m| (m.market, marks.get(&m.market).copied().unwrap_or(m.mid), m.stop_after))
            .collect();
        Walks { mids, scheduled: BTreeMap::new(), followed: BTreeSet::new() }
    }

    /// Remembers each split seen pending, and rescales a market's walk by `old / new` once a quote
    /// stamped `ts` would fall at or after that split's opening, so the quote is in the new units.
    pub(super) fn follow(&mut self, pending: BTreeMap<String, Split>, ts: Millis) {
        self.scheduled.extend(pending.into_iter().filter(|(id, _)| !self.followed.contains(id)));
        let due: Vec<_> =
            self.scheduled.iter().filter(|(_, (_, ex_open, ..))| *ex_open <= ts).map(|(id, _)| id.clone()).collect();
        for id in due {
            let (market, _, new, old) = self.scheduled.remove(&id).expect("selected above");
            for (_, mid, _) in self.mids.iter_mut().filter(|(quoted, ..)| *quoted == market) {
                *mid = mid.mul_div(Fixed::from_int(old.into()), Fixed::from_int(new.into()), Rounding::TowardZero);
            }
            self.followed.insert(id);
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
    let mut walks = Walks::start(cfg, &view.marks);
    let started = Instant::now();
    let mut every = interval(cfg.interval);
    loop {
        every.tick().await;
        let ts = now_ms();
        let Some((closed, view)) = market_view(&tx, ts).await else { return };
        walks.follow(view.pending, ts);
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

/// Every pending split, and the mark of every market that has split before.
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
    SplitView { pending, marks }
}

#[cfg(test)]
mod tests;
