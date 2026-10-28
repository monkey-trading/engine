//! The node watching itself. Every second it reads the sequencer's queue depth and, through a query
//! between batches, the house's NAV and exposure, each market's session and price, and the newest
//! quote from each venue; it sets those as metrics and raises `market_no_price` for an open market
//! a configured feed prices that has gone without a usable price for `alerts.no_price_after`. Each
//! market no configured feed prices is named once, in its own `market_unfed` Warning, and never
//! pages. A sequencer that answers no sample for that long raises `sequencer_stalled`, since no
//! market can be priced meanwhile. A market priced from futures that prices from the last contract
//! its listing names raises `futures_last_contract`, and one whose next contract's roll time has
//! passed with no gap recorded raises `futures_roll_overdue`. It reads the engine's state, not the feeds, so any feed that
//! sequences `Command::Prices` is covered.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use engine_core::event::Reject;
use engine_core::fixed::Rounding;
use engine_core::price::PriceError;
use engine_core::types::{MarketId, Millis};
use engine_core::{Engine, Fixed};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior, interval, timeout};
use tracing::{error, info, warn};

use crate::clock::now_ms;
use crate::metrics::{MarketLabel, Metrics, QuoteLabels, to_f64};
use crate::sequencer::Request;

const SAMPLE_EVERY: Duration = Duration::from_secs(1);

/// What one sample reads from the engine.
#[derive(Debug)]
pub struct Sample {
    pub house_nav: Fixed,
    pub house_gross_exposure: Fixed,
    pub markets: Vec<MarketSample>,
}

#[derive(Debug)]
pub struct MarketSample {
    pub id: MarketId,
    pub symbol: String,
    pub open: bool,
    /// Whether the engine could price a trade in the market at its sequenced time.
    pub price: Result<(), Unpriced>,
    /// In USD, as NAV and gross exposure are.
    pub house_net_exposure: Fixed,
    /// Venue and timestamp of the newest quote held from each venue.
    pub quotes: Vec<(String, Millis)>,
    /// For a market priced from futures that prices from the last contract its listing names, that
    /// contract: it can roll nowhere once the contract expires.
    pub last_contract: Option<String>,
    /// For a market priced from futures whose next contract's roll time has passed with no gap
    /// recorded, that next contract: the market cannot roll until both contracts have an index.
    pub roll_overdue: Option<String>,
}

/// Why the engine could not price a trade in a market: its own index, or, for a market quoted in
/// another currency, the FX index its PnL converts at.
#[derive(Debug, PartialEq)]
pub enum Unpriced {
    Index(PriceError),
    Fx(Reject),
}

/// Open markets without a usable price: since when, and whether the alert has fired. Only a market
/// a configured feed prices can page: nothing on the node can price the others, so a page at each
/// session open would only be noise.
pub struct NoPrice {
    fed: BTreeSet<MarketId>,
    since: BTreeMap<MarketId, (Instant, bool)>,
    /// The unfed markets already named in a `market_unfed` Warning.
    named: BTreeSet<MarketId>,
}

/// Markets priced from futures alerted as pricing from the last contract their listing names.
#[derive(Default)]
pub struct LastContract {
    alerted: BTreeSet<MarketId>,
}

/// Markets priced from futures alerted as past a roll time with no gap recorded.
#[derive(Default)]
pub struct RollOverdue {
    alerted: BTreeSet<MarketId>,
}

/// Since when the sequencer has answered no sample, and whether the alert has fired.
#[derive(Default)]
pub struct Stall {
    since: Option<(Instant, bool)>,
}

impl Sample {
    /// Reads a sample from `engine`.
    pub fn of(engine: &Engine) -> Self {
        let now = engine.now();
        let markets = engine
            .markets()
            .iter()
            .map(|(id, market)| MarketSample {
                id: *id,
                symbol: market.spec.symbol.clone(),
                open: market.open,
                // The checks a fill makes: the market's own index, then its FX index.
                price: market
                    .index(now)
                    .map_err(Unpriced::Index)
                    .and_then(|_| engine.live_fx(*id).map(|_| ()).map_err(Unpriced::Fx)),
                house_net_exposure: market
                    .mark
                    .map_or(Fixed::ZERO, |mark| market.usd_at_mark(-market.client_qty.mul(mark), Rounding::Up)),
                quotes: market.quotes.values().map(|quote| (quote.source.clone(), quote.ts)).collect(),
                last_contract: market
                    .futures
                    .as_ref()
                    .filter(|_| market.next_contract().is_none())
                    .map(|futures| futures.contract.clone()),
                roll_overdue: market
                    .futures
                    .as_ref()
                    .filter(|futures| futures.gap.is_none())
                    .and(market.next_contract())
                    .filter(|next| next.from.timestamp_millis() <= now)
                    .map(|next| next.contract.clone()),
            })
            .collect();
        Sample { house_nav: engine.house_nav(), house_gross_exposure: engine.house_gross_exposure(), markets }
    }

    /// Sets the sampled values as metrics. Quote times are set for exactly the sources `sources`
    /// feeds each market from, and one with no quote yet reads as quoting last at `started_ms`, when
    /// the node started: the engine keeps every source it has heard from, so a venue taken out of
    /// the config would otherwise age on every aggregate and hide a live one going stale, and a venue
    /// that never quoted would be missing rather than stale.
    pub fn record(&self, metrics: &Metrics, sources: &BTreeMap<MarketId, BTreeSet<String>>, started_ms: Millis) {
        metrics.house_nav.set(to_f64(self.house_nav));
        metrics.house_gross_exposure.set(to_f64(self.house_gross_exposure));
        for market in &self.markets {
            let label = MarketLabel { market: market.id };
            // Created at zero before the market's first fill, so a rate counts that fill.
            let _ = metrics.fills.get_or_create(&label);
            metrics.house_net_exposure.get_or_create(&label).set(to_f64(market.house_net_exposure));
            metrics.market_open.get_or_create(&label).set(market.open.into());
            metrics.market_priced.get_or_create(&label).set(market.price.is_ok().into());
        }
        for (market, venues) in sources {
            let held = self.markets.iter().find(|m| m.id == *market).map_or(&[][..], |m| &m.quotes);
            for venue in venues {
                let ts = held.iter().find(|(source, _)| source == venue).map_or(started_ms, |(_, ts)| *ts);
                let labels = QuoteLabels { market: *market, venue: venue.clone() };
                metrics.quote_timestamp_seconds.get_or_create(&labels).set(ts as f64 / 1000.0);
            }
        }
    }
}

impl fmt::Display for Unpriced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unpriced::Index(error) => write!(f, "{error}"),
            Unpriced::Fx(Reject::NoFxPrice { fx_market }) => write!(f, "its FX market {fx_market} has no usable price"),
            Unpriced::Fx(reject) => write!(f, "{reject:?}"),
        }
    }
}

impl NoPrice {
    /// Tracks the markets in `fed`, those a configured feed prices.
    pub fn new(fed: BTreeSet<MarketId>) -> Self {
        Self { fed, since: BTreeMap::new(), named: BTreeSet::new() }
    }

    /// Fires `market_no_price` once an open market a configured feed prices has been unpriced for
    /// `after`, and resolves it when the market is priced again or closes. Names each market no
    /// configured feed prices in a `market_unfed` Warning of its own the first time a sample holds
    /// it, so one unfed market's episode does not swallow another's.
    pub fn check(&mut self, sample: &Sample, now: Instant, after: Duration) {
        for market in
            sample.markets.iter().filter(|market| !self.fed.contains(&market.id) && self.named.insert(market.id))
        {
            warn!(
                alert = "market_unfed",
                market = market.id,
                "no configured feed prices {}: it cannot trade, and does not page market_no_price",
                market.symbol
            );
        }
        for market in sample.markets.iter().filter(|market| self.fed.contains(&market.id)) {
            match (&market.price, market.open) {
                (Err(reason), true) => {
                    let (since, fired) = self.since.entry(market.id).or_insert((now, false));
                    if !*fired && now.duration_since(*since) >= after {
                        *fired = true;
                        error!(
                            alert = "market_no_price",
                            market = market.id,
                            "{} has had no usable price for {after:?}: {reason}",
                            market.symbol
                        );
                    }
                }
                _ => {
                    if let Some((_, true)) = self.since.remove(&market.id) {
                        info!(
                            alert_resolved = "market_no_price",
                            market = market.id,
                            "{} is priced or closed",
                            market.symbol
                        );
                    }
                }
            }
        }
    }
}

impl LastContract {
    /// Warns once for each market priced from futures that prices from the last contract its
    /// listing names, so the operator lists the next before it expires: a market whose front stops
    /// quoting with no gap to its next recorded can never roll. Resolves once a next contract is
    /// listed.
    pub fn check(&mut self, sample: &Sample) {
        for market in &sample.markets {
            match &market.last_contract {
                Some(contract) if self.alerted.insert(market.id) => {
                    warn!(
                        alert = "futures_last_contract",
                        market = market.id,
                        "{} prices from {contract}, the last contract its listing names: list the next before it expires",
                        market.symbol
                    );
                }
                None if self.alerted.remove(&market.id) => {
                    info!(
                        alert_resolved = "futures_last_contract",
                        market = market.id,
                        "{} lists a next contract",
                        market.symbol
                    );
                }
                _ => {}
            }
        }
    }
}

impl RollOverdue {
    /// Warns once for each market priced from futures whose next contract's roll time has passed
    /// with no gap recorded, so the operator checks the next contract's feeds before the front
    /// stops quoting: the market rolls only once both contracts have an index. Resolves when the
    /// market rolls, or records a gap and so is no longer overdue.
    pub fn check(&mut self, sample: &Sample) {
        for market in &sample.markets {
            match &market.roll_overdue {
                Some(next) if self.alerted.insert(market.id) => {
                    warn!(
                        alert = "futures_roll_overdue",
                        market = market.id,
                        "{} is past the roll time of {next} with no gap recorded: check the feeds quoting {next}",
                        market.symbol
                    );
                }
                None if self.alerted.remove(&market.id) => {
                    info!(
                        alert_resolved = "futures_roll_overdue",
                        market = market.id,
                        "{} has rolled or recorded a gap",
                        market.symbol
                    );
                }
                _ => {}
            }
        }
    }
}

impl Stall {
    /// Fires `sequencer_stalled` once samples have gone unanswered for `after`, and resolves it at
    /// the next answer.
    pub fn check(&mut self, answered: bool, now: Instant, after: Duration) {
        if answered {
            if let Some((_, true)) = self.since.take() {
                info!(alert_resolved = "sequencer_stalled", "the sequencer is answering again");
            }
            return;
        }
        let (since, fired) = self.since.get_or_insert((now, false));
        if !*fired && now.duration_since(*since) >= after {
            *fired = true;
            error!(alert = "sequencer_stalled", "the sequencer has answered no sample for {after:?}");
        }
    }
}

/// Samples every second until the sequencer goes away. A sample the sequencer has not answered
/// within a second is skipped; the queue depth is still recorded, so a stalled sequencer shows.
/// Error cases:
/// - the sequencer has gone away
pub async fn run(
    tx: mpsc::Sender<Request>,
    metrics: Arc<Metrics>,
    no_price_after: Duration,
    sources: BTreeMap<MarketId, BTreeSet<String>>,
) -> anyhow::Result<()> {
    let started_ms = now_ms();
    let mut every = interval(SAMPLE_EVERY);
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let (mut no_price, mut stall) = (NoPrice::new(sources.keys().copied().collect()), Stall::default());
    let (mut last_contract, mut roll_overdue) = (LastContract::default(), RollOverdue::default());
    loop {
        every.tick().await;
        metrics.queue_depth.set(queue_depth(&tx));
        let (reply, sample) = oneshot::channel();
        let query = Request::Query(Box::new(move |engine| {
            let _ = reply.send(Sample::of(engine));
        }));
        let answered = timeout(SAMPLE_EVERY, async {
            tx.send(query).await.map_err(|_| anyhow!("sequencer gone"))?;
            sample.await.map_err(|_| anyhow!("sequencer gone"))
        })
        .await;
        let Ok(sample) = answered else {
            stall.check(false, Instant::now(), no_price_after);
            continue;
        };
        let sample = sample?;
        stall.check(true, Instant::now(), no_price_after);
        sample.record(&metrics, &sources, started_ms);
        no_price.check(&sample, Instant::now(), no_price_after);
        last_contract.check(&sample);
        roll_overdue.check(&sample);
    }
}

/// Requests waiting in the sequencer's channel.
fn queue_depth(tx: &mpsc::Sender<Request>) -> i64 {
    (tx.max_capacity() - tx.capacity()) as i64
}

#[cfg(test)]
mod tests;
