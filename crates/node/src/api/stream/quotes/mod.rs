//! Executable quotes over a signed stream. A client sends `{"op":"quotes","account":"0x…","markets":[…]}`
//! and gets a quote message per market, each in a signed frame, and again whenever one changes:
//! after the log head moves, at most once per `api.quote_interval` per connection. Sending the op
//! again replaces the subscription, and an empty `markets` ends it. A subscription is by address
//! and unsigned, as the account reads are.
//!
//! The quotes are read from the engine on the sequencer's thread, one query per connection per
//! interval, between batches of commands (`ladder`); the sequencer runs the queries gathered in a
//! batch together after its commands. Nothing is read while the log head has
//! not moved, since every input of a quote is a sequenced command's.

mod ladder;

use std::collections::BTreeMap;
use std::future::pending;
use std::time::Duration;

use engine_core::types::{AccountId, MarketId};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until};
use tracing::{debug, info};

use crate::log::{Manifest, Offset};
use crate::sequencer;
use ladder::{MarketQuote, market_quote};

/// What the connection sends to ask for quotes.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Op {
    #[serde(rename = "op")]
    _name: OpName,
    account: AccountId,
    markets: Vec<MarketId>,
}

/// The only op there is.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum OpName {
    Quotes,
}

/// A message to the client that is not a quote.
#[derive(Serialize)]
#[serde(tag = "type", rename = "quote_error")]
struct QuoteError {
    error: String,
}

/// Why a read found nothing to quote.
enum Unquotable {
    Account,
    Market(MarketId),
}

type Reply = Result<Vec<MarketQuote>, Unquotable>;

/// One connection's quote subscription.
pub(super) struct Quotes {
    /// Weak, so an open stream does not keep the sequencer's channel open at shutdown.
    tx: mpsc::WeakSender<sequencer::Request>,
    manifest: watch::Receiver<Manifest>,
    /// What the client last sent, in a watch so a client that floods its input costs one slot.
    inbox: watch::Receiver<String>,
    interval: Duration,
    max_markets: usize,
    subscription: Option<(AccountId, Vec<MarketId>)>,
    /// The earliest the next message may start: an interval after the last reply or refusal, so
    /// messages are at least an interval apart.
    gate: Instant,
    /// The instant the connection's slots count from: reads start only at whole intervals after it.
    anchor: Instant,
    /// The slot the read that has fallen due waits for.
    read_slot: Option<Instant>,
    /// The log head the last read started at; `None` for a subscription not yet read.
    read_at: Option<Option<Offset>>,
    in_flight: Option<oneshot::Receiver<Reply>>,
    /// The last message sent per market, to send only what changed.
    sent: BTreeMap<MarketId, String>,
    /// A refusal waiting for the gate.
    refusal: Option<String>,
}

impl Quotes {
    /// A subscription source for a connection whose client messages arrive on `inbox`, reading at
    /// the instants `phase` after the connection's start, and every `interval` after.
    pub(super) fn new(
        tx: mpsc::WeakSender<sequencer::Request>,
        manifest: watch::Receiver<Manifest>,
        inbox: watch::Receiver<String>,
        interval: Duration,
        max_markets: usize,
        phase: Duration,
    ) -> Self {
        let now = Instant::now();
        Self {
            tx,
            manifest,
            inbox,
            interval,
            max_markets,
            subscription: None,
            gate: now,
            anchor: now + phase,
            read_slot: None,
            read_at: None,
            in_flight: None,
            sent: BTreeMap::new(),
            refusal: None,
        }
    }

    /// The next messages to send the client, once there are any: a refusal of what it asked for, or
    /// the quotes that changed. Cancel-safe: a read in flight is kept across polls.
    pub(super) async fn next(&mut self) -> Vec<String> {
        loop {
            let reading = self.subscription.is_some() && self.in_flight.is_none();
            let head = self.manifest.borrow().flushed;
            let due = reading && self.read_at != Some(head);
            // The slot is fixed when a read falls due and kept across polls, so a poll that is dropped
            // and made again, as the feed's messages do, cannot push the read to a later slot.
            let slot = if due {
                let wanted = self.slot(self.gate.max(Instant::now()));
                *self.read_slot.get_or_insert(wanted)
            } else {
                self.read_slot = None;
                self.gate
            };
            tokio::select! {
                changed = self.inbox.changed() => {
                    if changed.is_err() {
                        return pending().await;
                    }
                    let text = self.inbox.borrow_and_update().clone();
                    self.subscribe(&text);
                }
                reply = in_flight(&mut self.in_flight) => {
                    self.in_flight = None;
                    self.gate = Instant::now() + self.interval;
                    if let Some(messages) = self.changed(reply) {
                        return messages;
                    }
                }
                () = sleep_until(self.gate), if self.refusal.is_some() => {
                    self.gate = Instant::now() + self.interval;
                    return self.refusal.take().into_iter().collect();
                }
                () = sleep_until(slot), if due => self.read(head),
                changed = self.manifest.changed(), if reading && !due => {
                    // The log has stopped, and the stream ends with it.
                    if changed.is_err() {
                        return pending().await;
                    }
                }
            }
        }
    }

    /// The first instant at or after `from` that is a whole interval from the connection's anchor.
    fn slot(&self, from: Instant) -> Instant {
        let interval = self.interval.as_nanos().max(1);
        let since = from.saturating_duration_since(self.anchor).as_nanos();
        let slots = u64::try_from(since.div_ceil(interval) * interval).expect("a slot is within centuries");
        self.anchor + Duration::from_nanos(slots)
    }

    /// Replaces the subscription with what `text` asks for, dropping the read in flight for the one
    /// before it. A request the node cannot serve ends the subscription and waits to be refused.
    /// Error cases:
    /// - `text` is not a quote op, or lists more than `api.quote_max_markets` distinct markets
    fn subscribe(&mut self, text: &str) {
        self.subscription = None;
        self.in_flight = None;
        self.sent.clear();
        self.read_at = None;
        let op: Op = match serde_json::from_str(text) {
            Ok(op) => op,
            Err(e) => return self.refuse(format!("not a quotes op: {e}")),
        };
        let mut markets = op.markets;
        markets.sort_unstable();
        markets.dedup();
        if markets.len() > self.max_markets {
            return self.refuse(format!("at most {} markets", self.max_markets));
        }
        self.refusal = None;
        info!(account = %op.account, markets = ?markets, "quote subscription");
        self.subscription = (!markets.is_empty()).then_some((op.account, markets));
    }

    fn refuse(&mut self, error: String) {
        self.refusal = Some(refusal(error));
    }

    /// Asks the sequencer for the subscription's quotes. A full queue leaves the read for the next
    /// interval.
    fn read(&mut self, head: Option<Offset>) {
        let Some((account, markets)) = self.subscription.clone() else { return };
        let Some(tx) = self.tx.upgrade() else {
            // The sequencer is stopping, and the stream ends with it.
            self.subscription = None;
            return;
        };
        let (reply, rx) = oneshot::channel();
        let query = Box::new(move |engine: &engine_core::Engine| {
            let _ = reply.send(read(engine, account, &markets));
        });
        match tx.try_send(sequencer::Request::Query(query)) {
            Ok(()) => (self.in_flight, self.read_at) = (Some(rx), Some(head)),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.gate = Instant::now() + self.interval;
                self.read_slot = None;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => self.subscription = None,
        }
    }

    /// The messages for the quotes in `reply` that differ from those last sent; `None` when none do.
    fn changed(&mut self, reply: Result<Reply, oneshot::error::RecvError>) -> Option<Vec<String>> {
        let quotes = match reply {
            Ok(Ok(quotes)) => quotes,
            Ok(Err(why)) => {
                self.subscription = None;
                return Some(vec![refusal(match why {
                    Unquotable::Account => "the account has never been credited".to_owned(),
                    Unquotable::Market(id) => format!("unknown market {id}"),
                })]);
            }
            // Dropped unanswered when the sequencer stops.
            Err(_) => {
                self.subscription = None;
                return None;
            }
        };
        let messages: Vec<String> = quotes
            .iter()
            .filter_map(|quote| {
                let text = serde_json::to_string(quote).expect("a quote encodes");
                (self.sent.get(&quote.market) != Some(&text)).then(|| {
                    self.sent.insert(quote.market, text.clone());
                    text
                })
            })
            .collect();
        debug!(markets = quotes.len(), changed = messages.len(), "quotes read");
        (!messages.is_empty()).then_some(messages)
    }
}

/// `account`'s quotes in `markets`, on the sequencer's thread.
fn read(engine: &engine_core::Engine, account: AccountId, markets: &[MarketId]) -> Reply {
    if !engine.accounts().contains_key(&account) {
        return Err(Unquotable::Account);
    }
    let quoter = engine.quoter();
    markets.iter().map(|&id| market_quote(engine, &quoter, account, id).ok_or(Unquotable::Market(id))).collect()
}

fn refusal(error: String) -> String {
    debug!(%error, "quote request refused");
    serde_json::to_string(&QuoteError { error }).expect("a refusal encodes")
}

/// The reply to the read in flight; never, with none.
async fn in_flight(read: &mut Option<oneshot::Receiver<Reply>>) -> Result<Reply, oneshot::error::RecvError> {
    match read {
        Some(rx) => rx.await,
        None => pending().await,
    }
}

#[cfg(test)]
mod api_tests;
#[cfg(test)]
mod bench;
#[cfg(test)]
mod tests;
