//! Limits on the public API: request and connection rates per client, request rates per account,
//! request body size and read time, open connections, stream subscribers, and the browser origins
//! allowed. A client is its socket peer's IP, or the address a trusted proxy's PROXY protocol header
//! names, never a forwarding header, which the untrusted host could forge; an IPv6 client counts by
//! its /64, the block one host is usually given. Limiters live in enclave memory
//! and start empty on every boot. They read the host's clock, which the host could skew; the host
//! controls the network anyway, so the limits bound clients, not the host. Each request checks its
//! limits on its own task, so the limiters are shared rather than owned by one task: routing every
//! check through an owner would put a channel round trip and a single queue in front of every
//! request.

use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::fmt::Display;
use std::hash::Hash;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::body::{Body, to_bytes};
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use engine_core::types::AccountId;
use governor::clock::{Clock, DefaultClock};
use governor::middleware::NoOpMiddleware;
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};
use http_body_util::LengthLimitError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{interval, timeout};
use tracing::{debug, warn};

use crate::api::ApiError;
use crate::config::{ApiSection, RateLimit};

/// How often the rate limiters forget clients and accounts whose buckets have refilled.
const RETAIN_EVERY: Duration = Duration::from_secs(10);
/// Least time between two raises of one alert.
const ALERT_EVERY: Duration = Duration::from_secs(60);
const SHARED_CAP_FULL: &str = "a cap every client shares is full; clients are being refused";

type KeyedLimiter<K, C> = RateLimiter<K, DashMapStateStore<K>, C, NoOpMiddleware<<C as Clock>::Instant>>;

/// The limits checked per connection, per request, and per stream subscription.
pub struct ApiLimits<C: Clock = DefaultClock> {
    per_ip: KeyedLimiter<IpAddr, C>,
    per_account: KeyedLimiter<AccountId, C>,
    /// Session-key requests for an account, whatever key signs them: one bucket per account at
    /// the per-account rate, separate from the account's own, so keys cannot multiply its
    /// allowance and a flooding key cannot starve the wallet's requests.
    session_accounts: KeyedLimiter<AccountId, C>,
    initial_accounts: KeyedLimiter<(), C>,
    initial_per_ip: KeyedLimiter<IpAddr, C>,
    per_ip_connections: KeyedLimiter<IpAddr, C>,
    subscribers: Arc<Semaphore>,
    subscribers_full: ThrottledAlert,
    /// A client gets this long to send a request's body.
    read_timeout: Duration,
    /// A stream subscriber that takes longer than this to take a message is disconnected.
    pub send_timeout: Duration,
    pub max_body_bytes: usize,
    /// The least time between quote messages to one connection, and the most markets it may ask for.
    pub quote_interval: Duration,
    pub quote_max_markets: usize,
    /// The browser origins `api.allowed_origins` lists.
    allowed_origins: BTreeSet<String>,
}

/// Open connections, in total and per client.
pub struct ConnectionCaps {
    total: Arc<Semaphore>,
    /// Open connections per client. Locked only to count one in or out, never across an await.
    per_client: Mutex<HashMap<IpAddr, usize>>,
    max_per_client: usize,
    full: ThrottledAlert,
}

/// One open connection's place under the caps, given back when it is dropped.
pub struct ConnectionSlot {
    _total: OwnedSemaphorePermit,
    caps: Arc<ConnectionCaps>,
    client: IpAddr,
}

/// A Warning alert raised at most once per `ALERT_EVERY`, so a flood or a failure that repeats logs
/// one line rather than one per occurrence.
pub(crate) struct ThrottledAlert {
    name: &'static str,
    message: &'static str,
    last: Mutex<Option<Instant>>,
}

impl Drop for ConnectionSlot {
    /// Gives the client's place back, forgetting a client with no connection left.
    fn drop(&mut self) {
        let mut per_client = self.caps.per_client.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(open) = per_client.get_mut(&self.client) {
            *open -= 1;
            if *open == 0 {
                per_client.remove(&self.client);
            }
        }
    }
}

impl ApiLimits {
    /// Limits from the `[api]` config, on the monotonic clock.
    pub fn new(cfg: &ApiSection) -> Self {
        Self::with_quota(cfg, per_second, DefaultClock::default())
    }
}

impl<C: Clock + Clone> ApiLimits<C> {
    /// Limits from the `[api]` config on `clock`, each configured rate made a bucket by `quota`.
    pub(crate) fn with_quota(cfg: &ApiSection, quota: impl Fn(RateLimit) -> Quota, clock: C) -> Self {
        Self {
            per_ip: RateLimiter::dashmap_with_clock(quota(cfg.per_ip), clock.clone()),
            per_account: RateLimiter::dashmap_with_clock(quota(cfg.per_account), clock.clone()),
            session_accounts: RateLimiter::dashmap_with_clock(quota(cfg.per_account), clock.clone()),
            initial_accounts: RateLimiter::dashmap_with_clock(quota(cfg.per_ip), clock.clone()),
            initial_per_ip: RateLimiter::dashmap_with_clock(quota(cfg.per_account), clock.clone()),
            per_ip_connections: RateLimiter::dashmap_with_clock(quota(cfg.per_ip_connections), clock),
            subscribers: Arc::new(Semaphore::new(cfg.max_subscribers.get())),
            subscribers_full: ThrottledAlert::new("api_subscribers_full", SHARED_CAP_FULL),
            read_timeout: cfg.read_timeout,
            send_timeout: cfg.send_timeout,
            max_body_bytes: cfg.max_body_bytes.get(),
            quote_interval: cfg.quote_interval,
            quote_max_markets: cfg.quote_max_markets.get(),
            allowed_origins: cfg.allowed_origins.iter().cloned().collect(),
        }
    }

    /// Whether any browser origin is allowed.
    pub(crate) fn has_allowed_origins(&self) -> bool {
        !self.allowed_origins.is_empty()
    }

    /// Whether `origin` is one `api.allowed_origins` lists, exactly.
    pub(crate) fn origin_allowed(&self, origin: &str) -> bool {
        self.allowed_origins.contains(origin)
    }

    /// Counts one new connection from `ip`, before its TLS handshake.
    /// Error cases:
    /// - the client is over its connection rate
    pub fn check_connection(&self, ip: IpAddr) -> Result<(), ApiError> {
        check(&self.per_ip_connections, &client_key(ip), "connection")
    }

    /// Counts one request from `ip`.
    /// Error cases:
    /// - the client is over its rate: `RateLimited`, with the wait until it may send again
    pub fn check_ip(&self, ip: IpAddr) -> Result<(), ApiError> {
        check(&self.per_ip, &client_key(ip), "ip")
    }

    /// Counts one request that acts for `account`.
    /// Error cases:
    /// - the account is over its rate: `RateLimited`, with the wait until it may act again
    pub fn check_account(&self, account: AccountId) -> Result<(), ApiError> {
        check(&self.per_account, &account, "account")
    }

    /// Counts one session-key request acting for `account`, whatever key signed it, against the
    /// account's session bucket.
    /// Error cases:
    /// - the account's session keys are over their rate: `RateLimited`, with the wait until one
    ///   may act again
    pub(crate) fn check_session_account(&self, account: AccountId) -> Result<(), ApiError> {
        check(&self.session_accounts, &account, "session_account")
    }

    /// Counts one declaration from a never-credited account: first against its client's share, at
    /// the per-account rate, so no one client can take the whole budget, then against the budget
    /// all clients share, at the per-IP rate, so fresh signing keys cannot multiply it.
    /// Error cases:
    /// - the client is over its share, or the shared budget is exhausted
    pub(crate) fn check_initial_account(&self, ip: Option<IpAddr>) -> Result<(), ApiError> {
        if let Some(ip) = ip {
            check(&self.initial_per_ip, &client_key(ip), "initial_ip")?;
        }
        check(&self.initial_accounts, &(), "initial_account")
    }

    /// A place for one more stream subscriber, held until it disconnects.
    /// Error cases:
    /// - every place is taken: `TooManySubscribers`
    pub fn subscribe(&self) -> Result<OwnedSemaphorePermit, ApiError> {
        self.subscribers.clone().try_acquire_owned().map_err(|_| {
            self.subscribers_full.raise(&"stream subscribers");
            ApiError::TooManySubscribers
        })
    }

    /// Forgets clients and accounts whose buckets have refilled; they start full when they return.
    fn retain_recent(&self) {
        self.per_ip.retain_recent();
        self.initial_per_ip.retain_recent();
        self.session_accounts.retain_recent();
        self.per_account.retain_recent();
        self.per_ip_connections.retain_recent();
    }
}

impl ConnectionCaps {
    /// Caps of `max_total` open connections, and `max_per_client` from one client.
    pub fn new(max_total: usize, max_per_client: usize) -> Arc<Self> {
        Arc::new(Self {
            total: Arc::new(Semaphore::new(max_total)),
            per_client: Mutex::new(HashMap::new()),
            max_per_client,
            full: ThrottledAlert::new("api_connections_full", SHARED_CAP_FULL),
        })
    }

    /// A place for a new connection from `ip`, or `None` when either cap is reached.
    pub fn admit(self: &Arc<Self>, ip: IpAddr) -> Option<ConnectionSlot> {
        let total = self.admit_total()?;
        self.claim(total, ip)
    }

    /// A place under the total cap alone, for a connection whose client is not yet known, or `None`
    /// when it is reached.
    pub fn admit_total(&self) -> Option<OwnedSemaphorePermit> {
        let Ok(total) = self.total.clone().try_acquire_owned() else {
            self.full.raise(&"connections");
            return None;
        };
        Some(total)
    }

    /// Counts a connection holding `total` against its client `ip`, or `None` when the client is at
    /// its cap.
    pub fn claim(self: &Arc<Self>, total: OwnedSemaphorePermit, ip: IpAddr) -> Option<ConnectionSlot> {
        let client = client_key(ip);
        let mut per_client = self.per_client.lock().unwrap_or_else(PoisonError::into_inner);
        let open = per_client.entry(client).or_default();
        if *open >= self.max_per_client {
            debug!("connection refused: client at its connection cap");
            return None;
        }
        *open += 1;
        Some(ConnectionSlot { _total: total, caps: self.clone(), client })
    }
}

impl ThrottledAlert {
    /// An alert named `name` that logs `message`, not yet raised.
    pub(crate) fn new(name: &'static str, message: &'static str) -> Self {
        Self { name, message, last: Mutex::new(None) }
    }

    /// Raises the alert with `detail`, unless it was raised within `ALERT_EVERY`.
    pub(crate) fn raise(&self, detail: &dyn Display) {
        if self.due(Instant::now()) {
            warn!(alert = self.name, "{}: {detail}", self.message);
        }
    }

    /// Whether a refusal at `now` raises the alert, recording `now` when it does.
    pub(crate) fn due(&self, now: Instant) -> bool {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        let due = last.is_none_or(|at| now.saturating_duration_since(at) >= ALERT_EVERY);
        if due {
            *last = Some(now);
        }
        due
    }
}

/// Refuses a request with 429 when its client is over its rate.
pub async fn limit_client(
    State(limits): State<Arc<ApiLimits>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    match limits.check_ip(peer.ip()) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}

/// Reads the whole request body before the handler runs: within the read timeout, so a client that
/// stalls mid-body cannot hold its connection (408), and up to `max_body_bytes` (413).
pub async fn read_body(State(limits): State<Arc<ApiLimits>>, req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = match timeout(limits.read_timeout, to_bytes(body, limits.max_body_bytes)).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) if e.source().is_some_and(|e| e.is::<LengthLimitError>()) => {
            return ApiError::BodyTooLarge.into_response();
        }
        Ok(Err(_)) => return ApiError::BadRequest("unreadable request body".into()).into_response(),
        Err(_) => return ApiError::BodyTimeout.into_response(),
    };
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

/// Forgets refilled buckets every `RETAIN_EVERY`, so the limiters hold recent clients and accounts,
/// not every one ever seen.
/// Error cases:
/// - none; it runs until the node stops
pub async fn run_retain(limits: Arc<ApiLimits>) -> anyhow::Result<()> {
    let mut ticks = interval(RETAIN_EVERY);
    loop {
        ticks.tick().await;
        limits.retain_recent();
    }
}

/// The address a client is counted under: IPv4 as is, also when mapped into IPv6, and IPv6 by its
/// /64.
fn client_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from_bits(v6.to_bits() & !u128::from(u64::MAX))),
        v4 => v4,
    }
}

/// A bucket of `burst` requests, refilled at `per_second`.
fn per_second(limit: RateLimit) -> Quota {
    Quota::per_second(limit.per_second).allow_burst(limit.burst)
}

/// Takes one request from `key`'s bucket in `limiter`, which `limit` names in the log.
/// Error cases:
/// - the bucket is empty: `RateLimited`, with the wait until it holds a request again
fn check<K: Hash + Eq + Clone, C: Clock>(
    limiter: &KeyedLimiter<K, C>,
    key: &K,
    limit: &'static str,
) -> Result<(), ApiError> {
    limiter.check_key(key).map_err(|not_until| {
        debug!(limit, "refused: rate limited");
        ApiError::RateLimited(not_until.wait_time_from(limiter.clock().now()))
    })
}

#[cfg(test)]
mod tests;
