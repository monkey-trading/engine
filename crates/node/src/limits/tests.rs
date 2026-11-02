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
    let rate = RateLimit { per_second: NonZeroU32::new(per_second).unwrap(), burst: NonZeroU32::new(burst).unwrap() };
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
        heartbeat_interval: Duration::from_secs(5),
        quote_interval: Duration::from_millis(100),
        quote_max_markets: NonZeroUsize::new(4).unwrap(),
        stats_interval: Duration::from_secs(60),
        allowed_origins: Vec::new(),
        tls: None,
        proxy_protocol: None,
    }
}
