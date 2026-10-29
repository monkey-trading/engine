//! Limits on the public API: request and connection rates per client, request rates per account,
//! request body size and read time, open connections, stream subscribers, and the browser origins
//! allowed. A client is its socket peer's IP, never a forwarding header, which the untrusted host
//! could forge; an IPv6 client counts by its /64, the block one host is usually given. Limiters live in enclave memory
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
            initial_accounts: RateLimiter::dashmap_with_clock(quota(cfg.per_ip), clock.clone()),
            initial_per_ip: RateLimiter::dashmap_with_clock(quota(cfg.per_account), clock.clone()),
            per_ip_connections: RateLimiter::dashmap_with_clock(quota(cfg.per_ip_connections), clock),
            subscribers: Arc::new(Semaphore::new(cfg.max_subscribers.get())),
            subscribers_full: ThrottledAlert::new("api_subscribers_full", SHARED_CAP_FULL),
            read_timeout: cfg.read_timeout,
            send_timeout: cfg.send_timeout,
            max_body_bytes: cfg.max_body_bytes.get(),
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
        let Ok(total) = self.total.clone().try_acquire_owned() else {
            self.full.raise(&"connections");
            return None;
        };
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
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::num::{NonZeroU32, NonZeroUsize};

    use governor::clock::FakeRelativeClock;

    #[test]
    fn a_client_over_its_burst_waits_for_the_refill() {
        let clock = FakeRelativeClock::default();
        let limits = ApiLimits::with_quota(&section(2, 3), per_second, clock.clone());
        let ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        for _ in 0..3 {
            limits.check_ip(ip).unwrap();
        }
        let Err(ApiError::RateLimited(wait)) = limits.check_ip(ip) else { panic!("a fourth request in a burst of 3") };
        assert_eq!(wait, Duration::from_millis(500));
        // Another client has its own bucket.
        limits.check_ip(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8))).unwrap();

        clock.advance(Duration::from_millis(499));
        assert!(limits.check_ip(ip).is_err());
        clock.advance(Duration::from_millis(1));
        limits.check_ip(ip).unwrap();
        assert!(limits.check_ip(ip).is_err());
    }

    #[test]
    fn one_client_takes_only_its_share_of_the_fresh_account_budget_all_clients_share() {
        let clock = FakeRelativeClock::default();
        let mut cfg = section(1, 3);
        cfg.per_account = RateLimit { per_second: NonZeroU32::new(1).unwrap(), burst: NonZeroU32::new(1).unwrap() };
        let limits = ApiLimits::with_quota(&cfg, per_second, clock.clone());
        let client = |n| Some(IpAddr::from(Ipv4Addr::new(203, 0, 113, n)));
        limits.check_initial_account(client(1)).unwrap();
        // Over its share, a client's fresh keys spend none of the shared budget.
        for _ in 0..3 {
            assert!(matches!(limits.check_initial_account(client(1)), Err(ApiError::RateLimited(_))));
        }
        limits.check_initial_account(client(2)).unwrap();
        limits.check_initial_account(client(3)).unwrap();
        assert!(matches!(limits.check_initial_account(client(4)), Err(ApiError::RateLimited(_))));
        assert!(matches!(limits.check_initial_account(None), Err(ApiError::RateLimited(_))));
        clock.advance(Duration::from_secs(1));
        limits.check_initial_account(client(1)).unwrap();
        assert!(matches!(limits.check_initial_account(client(4)), Err(ApiError::RateLimited(_))));
    }

    #[test]
    fn accounts_have_their_own_buckets() {
        let limits = ApiLimits::with_quota(&section(1, 1), per_second, FakeRelativeClock::default());
        let (a, b) = (AccountId([1; 20]), AccountId([2; 20]));
        limits.check_account(a).unwrap();
        assert!(matches!(limits.check_account(a), Err(ApiError::RateLimited(_))));
        limits.check_account(b).unwrap();
        // The per-account bucket is separate from the client's.
        limits.check_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
    }

    #[test]
    fn connections_and_requests_have_separate_buckets() {
        let limits = ApiLimits::with_quota(&section(1, 1), per_second, FakeRelativeClock::default());
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        limits.check_connection(ip).unwrap();
        assert!(matches!(limits.check_connection(ip), Err(ApiError::RateLimited(_))));
        limits.check_ip(ip).unwrap();
    }

    #[test]
    fn refilled_buckets_are_forgotten() {
        let clock = FakeRelativeClock::default();
        let limits = ApiLimits::with_quota(&section(10, 10), per_second, clock.clone());
        limits.check_ip(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        limits.check_account(AccountId([1; 20])).unwrap();
        limits.check_connection(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        let held = |l: &ApiLimits<_>| (l.per_ip.len(), l.per_account.len(), l.per_ip_connections.len());
        limits.retain_recent();
        assert_eq!(held(&limits), (1, 1, 1));
        // A bucket one request below full refills in 100ms.
        clock.advance(Duration::from_secs(1));
        limits.retain_recent();
        assert_eq!(held(&limits), (0, 0, 0));
    }

    #[test]
    fn one_ipv6_64_shares_its_buckets_and_connection_places() {
        let limits = ApiLimits::with_quota(&section(1, 1), per_second, FakeRelativeClock::default());
        let (a, b) = ("2001:db8:1:2::a".parse().unwrap(), "2001:db8:1:2::b".parse().unwrap());
        limits.check_ip(a).unwrap();
        assert!(limits.check_ip(b).is_err(), "a request from the same /64");
        limits.check_connection(a).unwrap();
        assert!(limits.check_connection(b).is_err(), "a connection from the same /64");

        let caps = ConnectionCaps::new(4, 1);
        let _slot = caps.admit(a).unwrap();
        assert!(caps.admit(b).is_none(), "a place for the same /64");
        assert!(caps.admit("2001:db8:1:3::a".parse().unwrap()).is_some(), "another /64");
    }

    #[test]
    fn subscribers_are_capped_until_one_leaves() {
        let limits = ApiLimits::with_quota(&section(1, 1), per_second, FakeRelativeClock::default());
        let first = limits.subscribe().unwrap();
        let _second = limits.subscribe().unwrap();
        assert!(matches!(limits.subscribe(), Err(ApiError::TooManySubscribers)));
        drop(first);
        let _third = limits.subscribe().unwrap();
    }

    #[test]
    fn ipv6_clients_count_by_their_64_and_mapped_ipv4_as_ipv4() {
        let ip = |s: &str| s.parse::<IpAddr>().unwrap();
        assert_eq!(client_key(ip("2001:db8:1:2:aaaa::1")), client_key(ip("2001:db8:1:2:ffff::9")));
        assert_eq!(client_key(ip("2001:db8:1:2:aaaa::1")), ip("2001:db8:1:2::"));
        assert_ne!(client_key(ip("2001:db8:1:2::1")), client_key(ip("2001:db8:1:3::1")));
        assert_eq!(client_key(ip("::ffff:203.0.113.7")), ip("203.0.113.7"));
        assert_eq!(client_key(ip("203.0.113.7")), ip("203.0.113.7"));
    }

    #[test]
    fn connections_are_capped_per_client_and_in_total() {
        let caps = ConnectionCaps::new(3, 2);
        let (a, b) = (IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), IpAddr::V4(Ipv4Addr::new(203, 0, 113, 8)));
        let a1 = caps.admit(a).unwrap();
        let _a2 = caps.admit(a).unwrap();
        assert!(caps.admit(a).is_none(), "a third connection from one client");
        let _b1 = caps.admit(b).unwrap();
        assert!(caps.admit(b).is_none(), "a fourth connection in total");

        // Closing one gives its place back.
        drop(a1);
        let _b2 = caps.admit(b).unwrap();
        assert!(caps.admit(a).is_none(), "the total is full again");
    }

    #[test]
    fn an_alert_is_raised_once_a_minute() {
        let alert = ThrottledAlert::new("test", "test");
        let start = Instant::now();
        assert!(alert.due(start));
        assert!(!alert.due(start + Duration::from_secs(59)));
        assert!(alert.due(start + ALERT_EVERY));
        assert!(!alert.due(start + ALERT_EVERY + Duration::from_secs(1)));
    }

    #[test]
    fn a_client_with_no_connection_left_is_forgotten() {
        let caps = ConnectionCaps::new(4, 4);
        let slot = caps.admit(IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        assert_eq!(caps.per_client.lock().unwrap().len(), 1);
        drop(slot);
        assert!(caps.per_client.lock().unwrap().is_empty());
    }

    // ── Helpers ──

    /// An `[api]` section with the same rate per client and per account, and two subscribers.
    fn section(per_second: u32, burst: u32) -> ApiSection {
        let rate =
            RateLimit { per_second: NonZeroU32::new(per_second).unwrap(), burst: NonZeroU32::new(burst).unwrap() };
        ApiSection {
            max_connections: NonZeroUsize::new(8).unwrap(),
            max_connections_per_ip: NonZeroUsize::new(4).unwrap(),
            handshake_timeout: Duration::from_secs(1),
            read_timeout: Duration::from_secs(1),
            max_body_bytes: NonZeroUsize::new(1024).unwrap(),
            per_ip: rate,
            per_account: rate,
            per_ip_connections: rate,
            max_subscribers: NonZeroUsize::new(2).unwrap(),
            send_timeout: Duration::from_secs(1),
            allowed_origins: Vec::new(),
        }
    }
}
