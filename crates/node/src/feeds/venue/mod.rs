//! One venue's live feed. A task connects, subscribes, and sequences the venue's top-of-book mids
//! as `Command::Prices`, one per book update as it arrives — the newest quote the host clock has
//! reached, never one stamped ahead of it. It returns only when the sequencer has gone away: a
//! failed connection is retried inside the task with backoff, and while it is down the venue's
//! last quotes age out in the engine, which prices from the other venues. A connection that stays
//! quiet for the read deadline — a stalled login, a dead connection — is failed and retried the
//! same way, even while every market is closed.
//! A quote stamped while its market's schedule is closed is held back, since the engine refuses it,
//! and a symbol whose market is closed is not counted as silent.

mod alerts;
mod confirm;
use confirm::Confirmation;
mod emit;
use emit::rearm;
mod quotes;
use quotes::Pending;
mod setup;
use setup::next_attempt;
pub use setup::tls_config;
mod transport;
use transport::{Incoming, Link};
mod validation;
pub(super) use validation::{check_size, check_time, to_quote};

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::future::pending;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use engine_core::Fixed;
use engine_core::session::Schedule;
use engine_core::types::{MarketId, Millis};
use futures_util::StreamExt;
use rustls::ClientConfig;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep, timeout, timeout_at};
use tracing::info;

use crate::clock::now_ms;
use crate::config::{ApiKey, Venue, VenueConnection, VenuesSection};
use crate::feeds::{bitstamp, coinbase, databento, kraken, massive, tradermade};
use crate::sequencer::Request;
use crate::throttle::Throttle;

/// A venue's protocol on one connection: its opening messages, and a reader that keeps any state
/// the venue's messages build on, such as an order book or a login handshake.
enum Adapter {
    Bitstamp,
    Coinbase(coinbase::Books),
    Kraken(kraken::Books),
    Massive(massive::Session),
    Tradermade(tradermade::Session),
    Databento(databento::Session),
}

/// Why an adapter refused a message.
#[derive(Debug, PartialEq)]
pub(super) enum Refusal {
    /// The message is dropped and the connection carries on.
    Message(String),
    /// The connection's state no longer matches the venue's, so it reconnects.
    Connection(String),
}

/// What one venue message carries, as its adapter reads it.
#[derive(Debug, PartialEq)]
pub(super) enum Frame {
    /// The best bid and ask for one or more of the venue's symbols.
    Books(Vec<Book>),
    /// An acknowledgement, heartbeat, or status message: nothing to price.
    Control,
    /// Messages the venue's protocol needs sent back, such as the subscription once a login is
    /// accepted.
    Reply(Vec<String>),
    /// The venue asked for a reconnect, so this connection ends.
    Reconnect,
}

/// One venue's best bid and ask for a symbol, at the venue's own time.
#[derive(Debug, PartialEq)]
pub(super) struct Book {
    pub symbol: String,
    pub bid: Fixed,
    pub ask: Fixed,
    pub ts: Millis,
}

/// How often a venue task sequences a symbol's latest quote, how it times out and reconnects, and
/// how old a quote may be and still count as the venue quoting.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// The most often one symbol's latest quote is sequenced. A book update sequences its
    /// symbol's quote at once; updates closer together than this coalesce to the newest, so venue
    /// messages a hostile host floods in cannot amplify sequencer load past this rate.
    pub interval: Duration,
    /// The deadline for a read: the login, the subscription, and each message after, whatever its
    /// market's hours.
    pub timeout: Duration,
    pub reconnect_min: Duration,
    pub reconnect_max: Duration,
    pub stale_after: Duration,
}

/// What a venue symbol quotes: a market, and for a market priced from futures, one contract.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Instrument {
    pub market: MarketId,
    pub contract: Option<String>,
}

/// One venue's feed task and the alert state that outlives each connection.
struct VenueFeed {
    conn: VenueConnection,
    /// The vendor's API key, for a venue whose feed needs one.
    api_key: Option<ApiKey>,
    /// The connection's symbols, mapped to what they quote.
    instruments: BTreeMap<String, Instrument>,
    timing: Timing,
    tls: Arc<ClientConfig>,
    tx: mpsc::Sender<Request>,
    /// Whether the venue is in an alerted outage that no fresh quote has ended yet.
    down: bool,
    /// Whether the current connection has sequenced a fresh quote.
    quoted: bool,
    /// When each symbol last had a fresh quote sequenced, counted from the task's start, the
    /// venue's last recovery, or the last time the engine took no quote for it, and kept across
    /// routine reconnects.
    last_quote: BTreeMap<String, Instant>,
    /// Symbols alerted as silent that have not quoted since.
    silent: BTreeSet<String>,
    /// Symbols the current connection has sequenced quotes for since its last confirmation.
    unconfirmed: BTreeSet<String>,
    /// The one confirmation awaited (see `confirm`).
    confirming: Option<oneshot::Receiver<Confirmation>>,
    /// Symbols the last confirmation found the engine could never take quotes for, by the alert
    /// raised for them: their market unlisted, or their contract not named by its listing. Their
    /// quotes are held back, not sequenced for the engine to refuse, and they are not counted as
    /// silent.
    held_back: BTreeMap<&'static str, BTreeSet<String>>,
    /// Symbols for a contract the market does not take now, one it has rolled off or one beyond the
    /// next: held back like `held_back`, without an alert.
    idle: BTreeSet<String>,
    /// Each market's trading hours, as the engine last confirmed them.
    schedules: Arc<BTreeMap<MarketId, Schedule>>,
    refusals: Throttle,
    clock_alerts: Throttle,
}

impl Timing {
    /// The timing from the node config: `min_quote_spacing` is the per-symbol sequencing pace.
    pub fn new(min_quote_spacing: Duration, venues: &VenuesSection) -> Self {
        Timing {
            interval: min_quote_spacing,
            timeout: venues.timeout,
            reconnect_min: venues.reconnect_min,
            reconnect_max: venues.reconnect_max,
            stale_after: venues.stale_after,
        }
    }
}

impl Adapter {
    /// A fresh adapter for a new connection on `conn`, authenticating with `api_key` where the
    /// venue needs one, and subscribed to `symbols`.
    fn new(conn: &VenueConnection, api_key: Option<&ApiKey>, symbols: &[&str]) -> Self {
        let key = api_key.map(ApiKey::expose).unwrap_or_default();
        match conn.venue {
            Venue::Bitstamp => Adapter::Bitstamp,
            Venue::Coinbase => Adapter::Coinbase(coinbase::Books::new(symbols)),
            Venue::Kraken => Adapter::Kraken(kraken::Books::new(symbols)),
            Venue::Massive => Adapter::Massive(massive::Session::new(&conn.url, key, symbols)),
            Venue::Tradermade => Adapter::Tradermade(tradermade::Session::new(key, symbols)),
            Venue::Databento => {
                let dataset = conn.dataset.as_deref().unwrap_or_default();
                Adapter::Databento(databento::Session::new(key, dataset, symbols))
            }
        }
    }

    /// The text messages a new connection opens with: its subscriptions, or the login that leads
    /// to them.
    fn opening(&self, symbols: &[&str]) -> Vec<String> {
        match self {
            Adapter::Bitstamp => bitstamp::subscriptions(symbols),
            Adapter::Coinbase(_) => coinbase::subscriptions(symbols),
            Adapter::Kraken(_) => kraken::subscriptions(symbols),
            Adapter::Massive(session) => session.opening(),
            Adapter::Tradermade(session) => session.opening(),
            Adapter::Databento(_) => Vec::new(),
        }
    }

    /// Whether the adapter keeps state built from earlier messages, so a refused book means that
    /// state can no longer be trusted.
    fn keeps_state(&self) -> bool {
        matches!(self, Adapter::Coinbase(_) | Adapter::Kraken(_))
    }

    /// Reads one venue message at host time `now`.
    /// Error cases:
    /// - the message is not one the venue's protocol defines, a field fails to parse, or the
    ///   venue reported an error
    /// - the message cannot be applied to the state kept for this connection
    fn read(&mut self, text: &str, now: Millis) -> Result<Frame, Refusal> {
        match self {
            Adapter::Bitstamp => bitstamp::parse(text).map_err(Refusal::Message),
            Adapter::Coinbase(books) => books.read(text, now),
            Adapter::Kraken(books) => books.read(text, now),
            Adapter::Massive(session) => session.read(text),
            Adapter::Tradermade(session) => session.read(text),
            Adapter::Databento(session) => session.read(text),
        }
    }
}

impl VenueFeed {
    /// A feed for `conn`, with no outage, refusal, or silence on record, and every symbol's
    /// silence counted from now.
    fn new(
        conn: VenueConnection,
        api_key: Option<ApiKey>,
        timing: Timing,
        tls: Arc<ClientConfig>,
        tx: mpsc::Sender<Request>,
    ) -> Self {
        let instruments: BTreeMap<String, Instrument> = conn
            .symbols
            .iter()
            .map(|s| (s.symbol.clone(), Instrument { market: s.market, contract: s.contract.clone() }))
            .collect();
        let started = Instant::now();
        let last_quote = instruments.keys().map(|symbol| (symbol.clone(), started)).collect();
        VenueFeed {
            conn,
            api_key,
            instruments,
            timing,
            tls,
            tx,
            down: false,
            quoted: false,
            last_quote,
            silent: BTreeSet::new(),
            unconfirmed: BTreeSet::new(),
            confirming: None,
            held_back: BTreeMap::new(),
            idle: BTreeSet::new(),
            schedules: Arc::default(),
            refusals: Throttle::default(),
            clock_alerts: Throttle::default(),
        }
    }

    /// Connects and relays until the connection fails, then retries after a backoff that doubles
    /// while attempts sequence no fresh quote and resets once one does.
    /// Error cases:
    /// - the sequencer has gone away
    async fn run(mut self) -> anyhow::Result<()> {
        let mut backoff = self.timing.reconnect_min;
        loop {
            self.quoted = false;
            let Err(error) = self.relay().await;
            self.settle().await;
            if self.tx.is_closed() {
                return Err(error.context("sequencer gone"));
            }
            let (wait, next) = next_attempt(backoff, self.quoted, &self.timing);
            self.dropped(&error, wait);
            sleep(wait).await;
            backoff = next;
        }
    }

    /// One connection: connect, send the opening messages, then read messages and sequence each
    /// symbol's newest reached quote as its book updates arrive (see `emit`), at most one per
    /// symbol per `timing.interval`. Once the sequencer has applied it, the venue counts as
    /// quoting that symbol while the quote the engine holds from it is no older than
    /// `timing.stale_after` or the market's `max_age_ms` (see `confirm`). Every read has
    /// `timing.timeout` to produce a message, whether or not any market is open, so a stalled
    /// login or a dead connection is replaced instead of hanging unseen through a closure.
    /// Error cases:
    /// - connecting or sending the opening messages fails or takes longer than `timing.timeout`
    /// - no message arrives within `timing.timeout`, whatever its market's hours
    /// - the connection sequences no fresh quote within `timing.timeout` while a market it quotes
    ///   is open, or the venue closes it or asks for a reconnect
    /// - a symbol goes silent, so the connection is replaced to resubscribe it
    /// - the connection fails, or the sequencer has gone away
    async fn relay(&mut self) -> anyhow::Result<Infallible> {
        let connect = transport::connect(&self.conn, self.tls.clone());
        let mut link = timeout(self.timing.timeout, connect).await.context("connect timed out")?.context("connect")?;
        let symbols: Vec<&str> = self.instruments.keys().map(String::as_str).collect();
        let mut adapter = Adapter::new(&self.conn, self.api_key.as_ref(), &symbols);
        let opening = adapter.opening(&symbols);
        timeout(self.timing.timeout, link.send_all(opening))
            .await
            .context("subscribe timed out")?
            .context("subscribe")?;
        info!(venue = self.conn.venue.name(), feed = self.conn.url.as_str(), "venue connected");
        let mut waiting_since = Instant::now();
        // Ask at once, so the first quotes are judged against the markets' hours.
        if self.confirming.is_none() {
            self.confirming = Some(self.confirm().await?);
        }

        let mut buffered: BTreeMap<String, Pending> = BTreeMap::new();
        let mut paced: BTreeMap<String, Instant> = BTreeMap::new();
        // The emit timer wakes for what `emit` holds: a symbol whose updates outran the pace, and
        // a quote the host clock has not reached yet. `emit` returns the next deadline; it idles
        // a day out when nothing waits. Like the read deadline, it outlives each select below.
        let mut emit_wake = Box::pin(sleep(Duration::from_secs(24 * 60 * 60)));
        // Silence and the no-fresh-quote count are checked once an interval, as they were on the
        // sequencing tick.
        let mut checks = interval_at(Instant::now() + self.timing.interval, self.timing.interval);
        checks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // The one deadline every read is judged against, whatever its market's hours: the login,
        // the subscription, and each message after must arrive within it, and any message met
        // sets the next. It outlives each select below, which the checks arm would otherwise
        // restart it past.
        let mut read_deadline = Instant::now() + self.timing.timeout;
        loop {
            tokio::select! {
                message = timeout_at(read_deadline, link.stream.next()) => {
                    let message = message.context("read timed out")?.context("connection closed")?.context("read")?;
                    read_deadline = Instant::now() + self.timing.timeout;
                    match message {
                        Incoming::Text(text) => {
                            self.receive(&mut adapter, &mut link, &text, &mut buffered).await?;
                            rearm(&mut emit_wake, self.emit(&mut buffered, &mut paced).await?);
                        }
                        Incoming::Binary => self.refuse("binary message", ""),
                        Incoming::Closed(reason) => bail!("closed by the venue: {reason}"),
                        Incoming::Other => {}
                    }
                }
                answer = async {
                    match &mut self.confirming {
                        Some(reply) => reply.await,
                        None => pending().await,
                    }
                } => {
                    self.confirming = None;
                    self.confirmed(answer.map_err(|_| anyhow!("sequencer gone"))?);
                }
                _ = &mut emit_wake => {
                    rearm(&mut emit_wake, self.emit(&mut buffered, &mut paced).await?);
                }
                _ = checks.tick() => {
                    if self.check_silent() {
                        bail!("resubscribing after a symbol went silent");
                    }
                    // The count waits while no market the connection quotes is open.
                    if !self.expects_quotes(now_ms()) {
                        waiting_since = Instant::now();
                    }
                    // Covers a connection that stays open but carries nothing usable: silent,
                    // heartbeats only, or every symbol refused, even while the venue is down.
                    if !self.quoted && waiting_since.elapsed() >= self.timing.timeout {
                        bail!("no fresh quote for {:?}", self.timing.timeout);
                    }
                    if self.confirming.is_none() {
                        self.unconfirmed.clear();
                        self.confirming = Some(self.confirm().await?);
                    }
                }
            }
        }
    }

    /// Reads one text message into `buffered`, refusing what does not parse or check, and sends
    /// any reply the venue's protocol asks for.
    /// Error cases:
    /// - the venue asked for a reconnect, or the adapter's state no longer matches the venue's,
    ///   including a refused book from an adapter that keeps state
    /// - a reply cannot be sent
    async fn receive(
        &mut self,
        adapter: &mut Adapter,
        link: &mut Link,
        text: &str,
        buffered: &mut BTreeMap<String, Pending>,
    ) -> anyhow::Result<()> {
        let now = now_ms();
        let books = match adapter.read(text, now) {
            Ok(Frame::Books(books)) => books,
            Ok(Frame::Control) => return Ok(()),
            Ok(Frame::Reply(messages)) => {
                return timeout(self.timing.timeout, link.send_all(messages)).await.context("reply timed out")?;
            }
            Ok(Frame::Reconnect) => bail!("the venue asked for a reconnect"),
            Err(Refusal::Message(reason)) => {
                self.refuse(&reason, text);
                return Ok(());
            }
            Err(Refusal::Connection(reason)) => {
                self.refuse(&reason, text);
                bail!("resyncing: {reason}");
            }
        };
        for book in books {
            match to_quote(self.conn.venue, &self.instruments, book, now) {
                Ok((symbol, quote)) => {
                    let market = self.instruments[&symbol].market;
                    if let Err(lead_ms) = buffered.entry(symbol).or_default().add(quote, now) {
                        self.clock_ahead(market, lead_ms);
                    }
                }
                Err(reason) if adapter.keeps_state() => {
                    self.refuse(&reason, text);
                    bail!("resyncing: {reason}");
                }
                Err(reason) => self.refuse(&reason, text),
            }
        }
        Ok(())
    }

    /// Whether the engine's last confirmed hours have `market` closed at `ts`. A market whose
    /// hours are not known yet counts as open, and the engine refuses a quote outside them.
    fn closed(&self, market: MarketId, ts: Millis) -> bool {
        self.schedules.get(&market).is_some_and(|schedule| !schedule.is_open(ts))
    }

    /// Whether the engine's last confirmed hours keep `market` open from a quote's stamp `ts` to an
    /// interval after host time `now`, by when the sequencer stamps a quote sent now. A span longer
    /// than `timing.stale_after` holds only a quote too old to count, so only its two ends are
    /// checked.
    fn in_session(&self, market: MarketId, ts: Millis, now: Millis) -> bool {
        let Some(schedule) = self.schedules.get(&market) else { return true };
        let interval = Millis::try_from(self.timing.interval.as_millis()).unwrap_or(Millis::MAX);
        let stale_after = Millis::try_from(self.timing.stale_after.as_millis()).unwrap_or(Millis::MAX);
        let (first, last) = (ts.min(now), now.saturating_add(interval));
        if last.saturating_sub(first) > stale_after.saturating_add(interval) {
            return schedule.is_open(first) && schedule.is_open(last);
        }
        schedule.open_throughout(first, last)
    }

    /// Every symbol held back from the engine: unlisted, unnamed, or idle.
    fn held(&self) -> BTreeSet<String> {
        self.held_back.values().flatten().chain(&self.idle).cloned().collect()
    }

    /// Whether `symbol` is held back from the engine (see `held`).
    fn is_held(&self, symbol: &str) -> bool {
        self.idle.contains(symbol) || self.held_back.values().any(|held| held.contains(symbol))
    }

    /// Whether any symbol should be quoting at host time `now`: one the engine takes, whose market
    /// is open.
    fn expects_quotes(&self, now: Millis) -> bool {
        self.instruments
            .iter()
            .any(|(symbol, instrument)| !self.idle.contains(symbol) && !self.closed(instrument.market, now))
    }
}

/// Streams one venue's quotes into the sequencer until the sequencer goes away, authenticating
/// with `api_key` where the venue needs one.
/// Error cases:
/// - the sequencer has gone away
pub async fn run(
    conn: VenueConnection,
    api_key: Option<ApiKey>,
    timing: Timing,
    tls: Arc<ClientConfig>,
    tx: mpsc::Sender<Request>,
) -> anyhow::Result<()> {
    VenueFeed::new(conn, api_key, timing, tls, tx).run().await
}

#[cfg(test)]
mod tests;
