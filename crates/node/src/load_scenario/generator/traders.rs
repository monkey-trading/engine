//! The traders: accounts of the book, each trading under a session key it registers, sending the
//! requests the scheduler hands its client, one at a time over the client's connection, and keeping
//! what the replies say it holds.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, ensure};
use engine_core::Fixed;
use engine_core::auth::RequestAuth;
use engine_core::command::{CancelAllRequest, CancelRequest, CloseAllRequest, CloseEntry, OrderKind, OrderRequest};
use engine_core::eip712::{self, Eip712Domain};
use engine_core::market::QuoteCurrency;
use engine_core::types::{AccountId, MarketId, Millis, Side};
use hyper::Method;
use k256::ecdsa::SigningKey;
use rustls::ClientConfig;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

use crate::clock::now_ms;
use crate::common::launch_book::named_key;
use crate::common::{CHAIN_ID, VAULT};
use crate::load_scenario::generator::client::Http;
use crate::load_scenario::generator::record::Recorder;

/// How long a signed request, and a session key, stay valid.
const REQUEST_LIFE_MS: Millis = 60_000;
const SESSION_LIFE_MS: Millis = 6 * 60 * 60 * 1_000;
/// Where a limit order rests, away from the mark of 100 so no price reaches it.
pub(crate) const LIMIT_BUY: i64 = 90;
pub(crate) const LIMIT_SELL: i64 = 110;
/// The book's mark in every market (`common::launch_book::MARK`).
const MARK: i64 = 100;
/// A request the scheduler hands out later than this after its schedule counts as late: the
/// generator fell behind it.
const LATE: Duration = Duration::from_millis(1);
/// How long a request may wait for its reply, and a client for its connection, before it counts as
/// failed and the client gives up.
const REPLY_WITHIN: Duration = Duration::from_secs(30);
const CONNECT_WITHIN: Duration = Duration::from_secs(60);

/// A request kind of the mix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Market,
    Limit,
    Cancel,
    CancelAll,
    CloseAll,
}

impl Kind {
    /// The kind's name in the figures.
    pub(super) fn name(self) -> &'static str {
        match self {
            Kind::Market => "market",
            Kind::Limit => "limit",
            Kind::Cancel => "cancel",
            Kind::CancelAll => "cancel_all",
            Kind::CloseAll => "close_all",
        }
    }
}

/// What a trader holds, as its replies say: the scheduler reads it to pick a trader a request can act
/// on.
#[derive(Debug, Default)]
pub(super) struct Holding {
    pub resting: Vec<u64>,
    /// Each market the trader holds a position in, and the side that closes it.
    pub positions: BTreeMap<MarketId, Side>,
}

impl Holding {
    /// Whether a request of `kind` has something to act on: a resting order to cancel, a position to
    /// close.
    pub(super) fn acts_on(&self, kind: Kind) -> bool {
        match kind {
            Kind::Cancel | Kind::CancelAll => !self.resting.is_empty(),
            Kind::CloseAll => !self.positions.is_empty(),
            Kind::Market | Kind::Limit => true,
        }
    }
}

/// One request the scheduler hands a client: which of its traders sends it, what it is, and when it
/// was due.
#[derive(Debug)]
pub(super) struct Job {
    pub trader: usize,
    pub kind: Kind,
    pub market: MarketId,
    pub side: Side,
    pub due: Instant,
    /// When the scheduler handed it out, at or a timer's tick after `due`.
    pub dispatched: Instant,
}

/// One trader: its account and session keys, the session key's next nonce, and what it holds.
pub(super) struct Trader {
    pub account: AccountId,
    key: SigningKey,
    session: SigningKey,
    nonce: u64,
    pub holding: Arc<Mutex<Holding>>,
    next_client_id: u64,
    /// Whether a rejection of this trader's has been written out.
    rejection_said: bool,
    /// The statuses other than 200 and 409 whose reply this trader has written out.
    statuses_said: BTreeSet<u16>,
}

impl Trader {
    /// The book's trader with named key `number` (`common::launch_book::named_key`), with a session
    /// key of its own.
    pub(super) fn new(number: u32) -> Self {
        let key = named_key(number);
        let mut scalar = [0x22; 32];
        scalar[28..].copy_from_slice(&number.to_be_bytes());
        let session = SigningKey::from_slice(&scalar).expect("a valid scalar");
        let account = eip712::address(key.verifying_key());
        Trader {
            account,
            key,
            session,
            nonce: 0,
            holding: Arc::default(),
            next_client_id: 1,
            rejection_said: false,
            statuses_said: BTreeSet::new(),
        }
    }

    /// Registers the session key with the account's signature at its next nonce.
    /// Error cases:
    /// - the nonce read or the registration fails, or is refused
    pub(super) async fn register(&mut self, http: &mut Http) -> anyhow::Result<()> {
        let (status, body) =
            http.send(Method::GET, &format!("/v1/accounts/{}/nonce", self.account), String::new()).await?;
        ensure!(status == 200, "the nonce read answered {status}");
        let nonce = serde_json::from_slice::<Value>(&body)?["next_nonce"].as_u64().context("a nonce")?;
        let (session, now) = (eip712::address(self.session.verifying_key()), now_ms());
        let (session_expires_at, expires_at) = (now + SESSION_LIFE_MS, now + REQUEST_LIFE_MS);
        let hash = eip712::session_key_registration_hash(
            &domain(),
            self.account,
            session,
            session_expires_at,
            nonce,
            expires_at,
        );
        let auth = RequestAuth { nonce, expires_at, signature: eip712::sign(&self.key, &hash) };
        let body =
            json!({ "account": self.account, "session_key": session, "expires_at": session_expires_at, "auth": auth });
        let (status, reply) = http.send(Method::POST, "/v1/session-keys", body.to_string()).await?;
        let events: Value = serde_json::from_slice(&reply).unwrap_or_default();
        ensure!(
            status == 200 && events[0]["type"] == "session_key_registered",
            "registration answered {status}: {events}"
        );
        Ok(())
    }

    /// Turns a cancel or close-all into a market order when what it acts on is gone: an earlier reply
    /// emptied the holding after the scheduler handed the job out, and the engine would reject the
    /// request, which the figures would count as the node refusing it.
    fn settle(&self, job: &mut Job) {
        if !self.holding.lock().expect("a holding").acts_on(job.kind) {
            job.kind = Kind::Market;
        }
    }

    /// The path and body of `job`, signed by the session key at its next nonce.
    fn request(&mut self, job: &Job, units: &BTreeMap<MarketId, Fixed>) -> (&'static str, String) {
        let (nonce, expires_at) = (self.nonce, now_ms() + REQUEST_LIFE_MS);
        let sign = |hash| RequestAuth { nonce, expires_at, signature: eip712::sign(&self.session, &hash) };
        let account = self.account;
        let holding = self.holding.lock().expect("a holding");
        match job.kind {
            Kind::Market | Kind::Limit => {
                let kind = match (job.kind, job.side) {
                    (Kind::Market, _) => OrderKind::Market { quoted: quoted(job.side), max_slippage_bps: None },
                    (_, Side::Buy) => OrderKind::Limit { price: Fixed::from_int(LIMIT_BUY) },
                    (_, Side::Sell) => OrderKind::Limit { price: Fixed::from_int(LIMIT_SELL) },
                };
                let client_id = self.next_client_id;
                self.next_client_id += 1;
                let qty = units[&job.market];
                let mut order = OrderRequest {
                    account,
                    client_id,
                    market: job.market,
                    side: job.side,
                    qty,
                    kind,
                    reduce_only: false,
                    take_profit: None,
                    stop_loss: None,
                    auth: None,
                };
                order.auth = Some(sign(eip712::order_hash(&domain(), &order, nonce, expires_at)));
                ("/v1/orders", json!(order).to_string())
            }
            Kind::Cancel => {
                let order_id = holding.resting.last().copied().unwrap_or(0);
                let auth = Some(sign(eip712::cancel_hash(&domain(), account, order_id, nonce, expires_at)));
                ("/v1/orders/cancel", json!(CancelRequest { account, order_id, auth }).to_string())
            }
            Kind::CancelAll => {
                let auth = Some(sign(eip712::cancel_all_hash(&domain(), account, nonce, expires_at)));
                ("/v1/orders/cancel-all", json!(CancelAllRequest { account, auth }).to_string())
            }
            Kind::CloseAll => {
                let closes = holding
                    .positions
                    .iter()
                    .map(|(market, side)| CloseEntry { market: *market, quoted: quoted(*side) });
                let mut request =
                    CloseAllRequest { account, closes: closes.collect(), max_slippage_bps: None, auth: None };
                request.auth = Some(sign(eip712::close_all_hash(&domain(), &request, nonce, expires_at)));
                ("/v1/positions/close-all", json!(request).to_string())
            }
        }
    }

    /// What to write out about a reply with `status`, the first this trader sees with that status: the
    /// body, so a run's refusals can be read as well as counted.
    pub(super) fn refusal_line(&mut self, status: u16, reply: &Value) -> Option<String> {
        self.statuses_said
            .insert(status)
            .then(|| format!("load generator: {} was answered {status}: {reply}", self.account))
    }

    /// Takes what a reply says: the nonce moves on for a request the log took, or to the one a
    /// conflict names; orders rest, cancel and fill, and positions open and close. Returns the fills
    /// the reply holds for this account, and the kind of the first rejection it holds, if any.
    fn take(&mut self, status: u16, reply: &Value) -> (u64, Option<String>) {
        match status {
            200 => self.nonce += 1,
            409 => {
                if let Some(expected) = reply["expected"].as_u64() {
                    self.nonce = expected;
                }
                return (0, None);
            }
            _ => {
                if let Some(line) = self.refusal_line(status, reply) {
                    eprintln!("{line}");
                }
                return (0, None);
            }
        }
        let mut holding = self.holding.lock().expect("a holding");
        let (mut fills, mut rejected) = (0, None);
        let mine = |event: &Value| event["account"] == json!(self.account);
        for event in reply.as_array().into_iter().flatten().filter(|event| mine(event)) {
            match event["type"].as_str() {
                Some("order_accepted") => holding.resting.extend(event["order_id"].as_u64()),
                Some("order_cancelled") => {
                    let id = event["order_id"].as_u64();
                    holding.resting.retain(|order| Some(*order) != id);
                }
                Some("fill") => {
                    fills += 1;
                    let (market, position) = (event["market"].as_u64(), event["position"].as_str());
                    if let (Some(market), Some(position)) = (market, position) {
                        let market = MarketId::try_from(market).expect("a market id");
                        match position.parse::<Fixed>() {
                            Ok(qty) if qty.is_zero() => drop(holding.positions.remove(&market)),
                            Ok(qty) => drop(
                                holding
                                    .positions
                                    .insert(market, if qty.is_negative() { Side::Buy } else { Side::Sell }),
                            ),
                            Err(_) => {}
                        }
                    }
                    if let Some(id) = event["order_id"].as_u64() {
                        holding.resting.retain(|order| *order != id);
                    }
                }
                Some("order_rejected") => {
                    // Said once per trader, so a rejection the figures only count can be read.
                    if rejected.is_none() && !std::mem::replace(&mut self.rejection_said, true) {
                        eprintln!("load generator: {} had an order rejected: {event}", self.account);
                    }
                    rejected = rejected.or_else(|| event["reason"]["kind"].as_str().map(str::to_owned));
                }
                _ => {}
            }
        }
        (fills, rejected)
    }
}

/// The domain the book's requests are signed under.
fn domain() -> Eip712Domain {
    eip712::domain(CHAIN_ID, VAULT)
}

/// Each market's units worth the book's position of $5,000 at its mark of 100: a euro or a pound is
/// 100 dollars and a dollar 100 yen in the book (`common::launch_book`).
pub(crate) fn units(markets: &BTreeMap<MarketId, QuoteCurrency>) -> BTreeMap<MarketId, Fixed> {
    markets
        .iter()
        .map(|(id, currency)| {
            let per_unit = match currency {
                QuoteCurrency::Usd => 100,
                QuoteCurrency::Eur { .. } | QuoteCurrency::Gbp { .. } => 10_000,
                QuoteCurrency::Jpy { .. } => 1,
            };
            (*id, Fixed::from_int(5_000).div_int(per_unit))
        })
        .collect()
}

/// The price an order to `side` quotes: twice the mark to buy and half it to sell, which no fill is
/// worse than, so no spread declines it. The house's stress levels widen a spread past a market's
/// slippage bound, and the generator measures the node's latency, not its slippage guard.
pub(crate) fn quoted(side: Side) -> Fixed {
    let mark = Fixed::from_int(MARK);
    if side == Side::Buy { mark.mul_int(2) } else { mark.div_int(2) }
}

/// A client: the address its connection names, its traders, and the jobs the scheduler hands it.
pub(super) struct Client {
    pub api: SocketAddr,
    pub address: IpAddr,
    pub tls: Arc<ClientConfig>,
    pub traders: Vec<Trader>,
    pub units: Arc<BTreeMap<MarketId, Fixed>>,
    pub recorder: Recorder,
}

impl Client {
    /// Opens the client's connection, retrying until the node takes it.
    /// Panics if the node takes none within `CONNECT_WITHIN`, which ends the generator.
    async fn connect(&self) -> Http {
        let started = Instant::now();
        loop {
            let failure = match timeout(REPLY_WITHIN, Http::open(self.api, self.address, &self.tls)).await {
                Ok(Ok(http)) => return http,
                Ok(Err(e)) => format!("{e:#}"),
                Err(_) => "the handshake timed out".to_owned(),
            };
            assert!(started.elapsed() < CONNECT_WITHIN, "client {} cannot connect: {failure}", self.address);
            sleep(Duration::from_millis(100)).await;
        }
    }

    /// Registers every trader's session key.
    /// Error cases:
    /// - a registration fails
    pub(super) async fn register(&mut self) -> anyhow::Result<()> {
        let mut http = self.connect().await;
        for trader in &mut self.traders {
            trader
                .register(&mut http)
                .await
                .with_context(|| format!("registering {}'s session key", trader.account))?;
        }
        Ok(())
    }

    /// Sends each job as it comes, one at a time, until the scheduler hangs up, counting each off
    /// `in_flight` once its reply is recorded. A connection the node has closed, as it closes one idle
    /// for its read timeout, is opened again at once, so no request waits for a handshake it would not
    /// have waited for.
    pub(super) async fn run(mut self, mut jobs: mpsc::UnboundedReceiver<Job>, in_flight: Arc<AtomicU64>) {
        let mut http = self.connect().await;
        loop {
            let job = tokio::select! {
                job = jobs.recv() => match job { Some(job) => job, None => return },
                () = sleep(Duration::from_millis(200)) => {
                    if http.closed() {
                        http = self.connect().await;
                    }
                    continue;
                }
            };
            if http.closed() {
                http = self.connect().await;
            }
            self.send(&mut http, job).await;
            in_flight.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Sends one job and records its reply. Its time runs from when it was due to its reply, less the
    /// time the client took to sign it: a wait behind the client's previous request counts, and so
    /// does the scheduler's lateness, which `late` counts; the client's own signing does not. A
    /// reply with a rejection in it is refused, as one with any status but 200 is.
    async fn send(&mut self, http: &mut Http, mut job: Job) {
        let trader = &mut self.traders[job.trader];
        trader.settle(&mut job);
        let late = job.dispatched.duration_since(job.due) > LATE;
        let signing = Instant::now();
        let (path, body) = trader.request(&job, &self.units);
        let signed_in = signing.elapsed();
        let sent = timeout(REPLY_WITHIN, http.send(Method::POST, path, body))
            .await
            .unwrap_or_else(|_| Err(anyhow!("no reply")));
        let waited = job.due.elapsed().saturating_sub(signed_in).as_micros() as u64;
        let (outcome, fills, refused) = match sent {
            Ok((status, reply)) => {
                let reply: Value = serde_json::from_slice(&reply).unwrap_or_default();
                let (fills, rejected) = trader.take(status, &reply);
                let refused = status != 200 || rejected.is_some();
                let outcome = rejected.map_or_else(|| status.to_string(), |kind| format!("{status}_rejected_{kind}"));
                (outcome, fills, refused)
            }
            Err(_) => {
                *http = self.connect().await;
                ("failed".to_owned(), 0, true)
            }
        };
        let mut samples = self.recorder.lock().expect("a recorder");
        *samples.sent.entry(job.kind.name().to_owned()).or_default() += 1;
        *samples.replies.entry(format!("{}_{outcome}", job.kind.name())).or_default() += 1;
        samples.late += u64::from(late);
        samples.refused += u64::from(refused);
        samples.request_us.push(waited);
        samples.fills += fills;
        if job.kind == Kind::Market && fills > 0 {
            samples.order_to_fill_us.push(waited);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancel_or_close_all_whose_target_is_gone_goes_out_as_a_market_order() {
        let mut trader = Trader::new(1);
        let units = BTreeMap::from([(2, Fixed::from_int(50))]);
        let job =
            |kind| Job { trader: 0, kind, market: 2, side: Side::Buy, due: Instant::now(), dispatched: Instant::now() };
        for kind in [Kind::Cancel, Kind::CancelAll, Kind::CloseAll] {
            let mut gone = job(kind);
            trader.settle(&mut gone);
            assert_eq!(gone.kind, Kind::Market, "{kind:?} with nothing to act on");
            assert_eq!(trader.request(&gone, &units).0, "/v1/orders");
        }
        {
            let mut holding = trader.holding.lock().unwrap();
            holding.resting.push(7);
            holding.positions.insert(2, Side::Sell);
        }
        for (kind, path) in [
            (Kind::Cancel, "/v1/orders/cancel"),
            (Kind::CancelAll, "/v1/orders/cancel-all"),
            (Kind::CloseAll, "/v1/positions/close-all"),
        ] {
            let mut held = job(kind);
            trader.settle(&mut held);
            assert_eq!(held.kind, kind, "{kind:?} with a target");
            assert_eq!(trader.request(&held, &units).0, path);
        }
    }
}
