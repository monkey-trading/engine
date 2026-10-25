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

use engine_core::Fixed;
use engine_core::command::Command;
use engine_core::price::SourceQuote;
use engine_core::types::{MarketId, Millis};
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

/// A random walk per market, quoted by three venues that differ by about a bp, until the market's
/// `stop_after`, and only while the engine's hours have it open, since the engine refuses a quote
/// from outside them. Returns only when the sequencer has gone away.
pub async fn run_mock(cfg: &FeedsSection, tx: mpsc::Sender<Request>) {
    let mut rng = StdRng::from_entropy();
    let mut mids: Vec<(u32, Fixed, Option<Duration>)> =
        cfg.mock_mids.iter().map(|m| (m.market, m.mid, m.stop_after)).collect();
    let started = Instant::now();
    let mut every = interval(cfg.interval);
    loop {
        every.tick().await;
        let ts = now_ms();
        let Some(closed) = closed_markets(&tx, ts).await else { return };
        let updates: Vec<Command> = mids
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

/// The listed markets whose hours have them closed at `ts`, as the engine holds them, or `None`
/// once the sequencer has gone away.
async fn closed_markets(tx: &mpsc::Sender<Request>, ts: Millis) -> Option<BTreeSet<MarketId>> {
    let (reply, closed) = oneshot::channel();
    let query: Query = Box::new(move |engine| {
        let closed = engine.markets().iter().filter(|(_, market)| !market.spec.schedule.is_open(ts));
        let _ = reply.send(closed.map(|(id, _)| *id).collect());
    });
    tx.send(Request::Query(query)).await.ok()?;
    closed.await.ok()
}

#[cfg(test)]
mod tests;
