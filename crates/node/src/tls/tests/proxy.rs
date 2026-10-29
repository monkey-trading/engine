//! The PROXY protocol v2 header of a trusted pass-through proxy: only it names the client, only
//! from the configured ranges, and every limit then counts that client.

use super::*;

use axum::routing::get;
use ppp::v2::{Builder, Command, Protocol, Version};
use tokio::time::sleep;

use crate::config::ProxyProtocolSection;
use crate::limits::limit_client;
use crate::tls::proxy::client_behind;
use crate::tls::tests::acme::Unverified;

#[tokio::test]
async fn a_trusted_proxys_header_names_the_client_handlers_see() {
    let node = serve_echo(behind_proxy()).await;
    for (claimed, seen) in [
        ("203.0.113.7:5555", "203.0.113.7:5555"),
        ("[2001:db8::1]:4000", "[2001:db8::1]:4000"),
        // A dual-stack proxy may name an IPv4 client in its IPv6 form; limits count it as the IPv4 one.
        ("[::ffff:203.0.113.8]:6000", "203.0.113.8:6000"),
    ] {
        let mut conn = node.connect_after(&header(claimed)).await.unwrap();
        assert_eq!(http(&mut conn, "GET", "/v1/health", "").await, (200, seen.to_owned()), "{claimed}");
    }
}

#[tokio::test]
async fn a_local_header_is_the_proxys_own_connection() {
    let node = serve_echo(behind_proxy()).await;
    // The header's addresses are ignored: handlers see the socket peer, the proxy.
    let mut conn = node.connect_after(&local("203.0.113.7:5555")).await.unwrap();
    let (status, who) = http(&mut conn, "GET", "/v1/health", "").await;
    assert_eq!(status, 200);
    assert!(who.starts_with("127.0.0.1:"), "{who}");
}

#[tokio::test]
async fn a_local_connection_counts_against_the_proxys_caps_and_rates_alone() {
    let mut api = behind_proxy();
    api.max_connections_per_ip = NonZeroUsize::new(1).unwrap();
    let node = TestNode::start(api, V4).await;
    let held = node.connect_after(&local("203.0.113.7:1")).await.unwrap();
    assert!(node.connect_after(&local("203.0.113.9:1")).await.is_err(), "a second connection of the proxy");
    assert!(node.connect_after(&header("203.0.113.7:1")).await.is_ok(), "the named client is not charged");
    drop(held);

    let mut api = behind_proxy();
    api.per_ip_connections = rate(1, 1);
    let node = TestNode::start(api, V4).await;
    node.connect_after(&local("203.0.113.7:1")).await.unwrap();
    assert!(node.connect_after(&local("203.0.113.7:2")).await.is_err(), "over the proxy's connection rate");
    node.connect_after(&header("203.0.113.7:1")).await.unwrap();
}

#[tokio::test]
async fn a_direct_client_is_its_socket_peer_while_a_proxy_range_is_configured() {
    let mut api = api();
    api.proxy_protocol = Some(ProxyProtocolSection { trusted: vec!["10.0.0.0/8".parse().unwrap()] });
    let node = serve_echo(api).await;
    let mut conn = node.connect(V4).await.unwrap();
    let (status, who) = http(&mut conn, "GET", "/v1/health", "").await;
    assert_eq!(status, 200);
    assert!(who.starts_with("127.0.0.1:"), "{who}");
}

#[tokio::test]
async fn request_limits_count_each_client_the_header_names() {
    let mut api = behind_proxy();
    api.per_ip = rate(1, 2);
    let node = TestNode::start(api, V4).await;
    let first: Vec<_> = [node.served_as("203.0.113.7:1").await, node.served_as("203.0.113.7:2").await].into();
    assert_eq!(first, [true, true], "the burst of the first client");
    assert!(!node.served_as("203.0.113.7:3").await, "its third request");
    assert!(node.served_as("203.0.113.9:1").await, "another client behind the same proxy has its own bucket");
    // IPv6 clients count by their /64.
    assert!(node.served_as("[2001:db8:0:1::1]:1").await);
    assert!(node.served_as("[2001:db8:0:1::2]:1").await);
    assert!(!node.served_as("[2001:db8:0:1::3]:1").await, "one /64 is one client");
    assert!(node.served_as("[2001:db8:0:2::1]:1").await, "another /64");
}

#[tokio::test]
async fn connection_caps_and_rates_count_each_client_the_header_names() {
    let mut api = behind_proxy();
    api.max_connections_per_ip = NonZeroUsize::new(1).unwrap();
    api.per_ip_connections = rate(1, 3);
    let node = TestNode::start(api, V4).await;
    let held = node.connect_after(&header("203.0.113.7:1")).await.unwrap();
    assert!(node.connect_after(&header("203.0.113.7:2")).await.is_err(), "a second connection of one client");
    let other = node.connect_after(&header("203.0.113.9:1")).await;
    assert!(other.is_ok(), "the proxy's own address is not the client");
    drop((held, other));

    // A fresh node: a client's third connection in the hour is over its rate.
    let mut api = behind_proxy();
    api.per_ip_connections = rate(1, 2);
    let node = TestNode::start(api, V4).await;
    for port in 1..=2 {
        node.connect_after(&header(&format!("203.0.113.7:{port}"))).await.unwrap();
    }
    assert!(node.connect_after(&header("203.0.113.7:3")).await.is_err(), "over the client's connection rate");
    node.connect_after(&header("203.0.113.9:1")).await.unwrap();
}

#[tokio::test]
async fn a_trusted_peer_without_a_valid_header_is_closed() {
    let node = TestNode::start(slow_timeout(behind_proxy()), V4).await;
    let no_client = |family_protocol: u8, command: u8| {
        let mut bytes = header("203.0.113.7:1");
        (bytes[12], bytes[13]) = (0x20 | command, family_protocol);
        bytes
    };
    let mut too_long = header("203.0.113.7:1");
    too_long[14..16].copy_from_slice(&2000u16.to_be_bytes());
    let cases: [(&str, Vec<u8>); 6] = [
        ("no header at all", Vec::new()),
        ("a version 1 header", b"PROXY TCP4 203.0.113.7 192.0.2.1 5555 443\r\n".to_vec()),
        ("a header over the read cap", too_long),
        ("an unspecified address family", no_client(0x01, 0x01)),
        ("a datagram header", no_client(0x12, 0x01)),
        ("a unix address family", no_client(0x31, 0x01)),
    ];
    for (name, prefix) in cases {
        let conn = timeout(Duration::from_secs(5), node.connect_after(&prefix)).await.expect("closed, not held open");
        assert!(conn.is_err(), "{name}");
    }
    assert!(node.served_as("203.0.113.7:1").await, "a valid header is served meanwhile");
}

#[tokio::test]
async fn a_header_the_proxy_cuts_short_closes_the_connection() {
    let node = TestNode::start(slow_timeout(behind_proxy()), V4).await;
    let mut tcp = TcpStream::connect((V4, node.port)).await.unwrap();
    tcp.write_all(&header("203.0.113.7:1")[..20]).await.unwrap();
    tcp.shutdown().await.unwrap();
    let closed = timeout(Duration::from_secs(5), tcp.read(&mut [0u8; 1])).await.expect("closed, not held open");
    assert!(matches!(closed, Ok(0) | Err(_)));
    assert!(node.served_as("203.0.113.7:1").await);
}

#[tokio::test]
async fn only_an_unusable_header_raises_the_alert() {
    let mut over_cap = header("203.0.113.7:1");
    over_cap[14..16].copy_from_slice(&2000u16.to_be_bytes());
    let cases: [(&str, Vec<u8>, Option<&str>, bool); 7] = [
        ("a valid header", header("203.0.113.7:5555"), Some("203.0.113.7:5555"), false),
        ("a LOCAL header", local("203.0.113.7:5555"), Some("127.0.0.1:1"), false),
        ("no bytes", Vec::new(), None, true),
        ("the signature alone", header("203.0.113.7:1")[..12].to_vec(), None, true),
        ("a header cut short", header("203.0.113.7:1")[..20].to_vec(), None, true),
        ("a header over the read cap", over_cap, None, true),
        ("a TLS hello", vec![0x16, 3, 1, 0, 200, 1, 0, 0, 196, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0], None, true),
    ];
    for (name, bytes, client, alerts) in cases {
        let alert = ThrottledAlert::new("test", "test");
        let proxy: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let got = client_behind(&mut bytes.as_slice(), proxy, &alert).await;
        assert_eq!(got, client.map(|c| c.parse().unwrap()), "{name}");
        assert_eq!(!alert.due(Instant::now()), alerts, "{name}");
    }
}

#[tokio::test]
async fn a_header_from_an_address_outside_the_trusted_ranges_is_refused() {
    let elsewhere = Some(ProxyProtocolSection { trusted: vec!["10.0.0.0/8".parse().unwrap()] });
    let node = TestNode::start(slow_timeout(ApiSection { proxy_protocol: elsewhere, ..api() }), V4).await;
    let forged = timeout(Duration::from_secs(5), node.connect_after(&header("198.51.100.1:1"))).await;
    assert!(forged.expect("closed, not held open").is_err(), "the loopback is not the proxy");
    let local = timeout(Duration::from_secs(5), node.connect_after(&local("198.51.100.1:1"))).await;
    assert!(local.expect("closed, not held open").is_err(), "nor is its LOCAL header accepted");
    let version_1 = node.connect_after(b"PROXY TCP4 198.51.100.1 192.0.2.1 5555 443\r\n").await;
    assert!(version_1.is_err());
    node.connect(V4).await.unwrap();

    // The same without any proxy configured.
    let node = TestNode::start(slow_timeout(api()), V4).await;
    assert!(node.connect_after(&header("198.51.100.1:1")).await.is_err());
    node.connect(V4).await.unwrap();
}

#[tokio::test]
async fn a_connection_awaiting_its_header_holds_a_place_under_the_total_cap_until_the_timeout() {
    let mut api = behind_proxy();
    api.max_connections = NonZeroUsize::new(1).unwrap();
    api.handshake_timeout = Duration::from_millis(200);
    let node = TestNode::start(api, V4).await;
    let mut silent = TcpStream::connect((V4, node.port)).await.unwrap();
    assert!(node.connect_after(&header("203.0.113.7:1")).await.is_err(), "the silent connection holds the place");

    let closed = timeout(Duration::from_secs(5), silent.read(&mut [0u8; 1])).await;
    assert_eq!(closed.expect("closed by the server").unwrap(), 0);
    for _ in 0..500 {
        if node.connect_after(&header("203.0.113.7:1")).await.is_ok() {
            return;
        }
        sleep(Duration::from_millis(10)).await;
    }
    panic!("no place was freed");
}

#[tokio::test]
async fn a_validation_through_the_proxy_is_answered_like_any_other() {
    const DOMAIN: &str = "api.engine.test";
    let node = TestNode::start(behind_proxy(), V4).await;
    let _answer = node.key.answer(DOMAIN, [7; 32]).unwrap();
    let alpn = vec![crate::tls::ACME_TLS_ALPN.to_vec()];
    assert!(node.validate_after(&header("203.0.113.7:1"), &alpn).await.is_ok(), "through the proxy");
    assert!(node.validate_after(&[], &alpn).await.is_err(), "without its header, a proxy peer is closed");
}

// ── Helpers ──

/// The proxy: the loopback every test connects from.
fn trusted_loopback() -> Option<ProxyProtocolSection> {
    Some(ProxyProtocolSection { trusted: vec!["127.0.0.1/32".parse().unwrap(), "::1/128".parse().unwrap()] })
}

/// An `[api]` section behind a proxy at the loopback.
fn behind_proxy() -> ApiSection {
    ApiSection { proxy_protocol: trusted_loopback(), ..api() }
}

/// An `[api]` section whose handshake timeout is far past the 5 seconds a test waits, so a refusal
/// that waits for the timeout fails the test.
fn slow_timeout(api: ApiSection) -> ApiSection {
    ApiSection { handshake_timeout: Duration::from_secs(60), ..api }
}

/// A v2 LOCAL header, which carries addresses a receiver must ignore.
fn local(client: &str) -> Vec<u8> {
    let mut bytes = header(client);
    bytes[12] = 0x20;
    bytes
}

/// A v2 header for a TCP client at `client`.
fn header(client: &str) -> Vec<u8> {
    let client: SocketAddr = client.parse().unwrap();
    // Where the proxy accepted the client, in the client's family; the node never reads it.
    let proxy_side: SocketAddr = if client.is_ipv4() { "192.0.2.1:443" } else { "[2001:db8::fe]:443" }.parse().unwrap();
    Builder::with_addresses(Version::Two | Command::Proxy, Protocol::Stream, (client, proxy_side)).build().unwrap()
}

impl TestNode {
    /// A TLS connection from the loopback that sends `prefix` first.
    /// Error cases:
    /// - the connection or handshake fails
    async fn connect_after(&self, prefix: &[u8]) -> io::Result<ClientTls<TcpStream>> {
        let provider = Arc::new(default_provider());
        let pin = Some(attested_tls_key(&self.attestation));
        let verifier = Arc::new(PinnedKey { pin, seen: Arc::default(), provider: provider.clone() });
        let config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        let mut tcp = TcpStream::connect((V4, self.port)).await?;
        tcp.write_all(prefix).await?;
        TlsConnector::from(Arc::new(config)).connect(ServerName::try_from("engine").unwrap(), tcp).await
    }

    /// Whether a request on a connection from the proxy naming `client` gets a 200.
    async fn served_as(&self, client: &str) -> bool {
        match self.connect_after(&header(client)).await {
            Ok(mut conn) => http(&mut conn, "GET", "/v1/health", "").await.0 == 200,
            Err(_) => false,
        }
    }
}

/// Serves `/v1/health`, which echoes the `ConnectInfo` a handler sees, under the request limit.
async fn serve_echo(api: ApiSection) -> TestNode {
    let tls = TlsIdentity::generate().unwrap();
    let bundle = AttestationBundle {
        enclave_address: EnclaveKey::generate().address(),
        config_sha256: [0; 32],
        tls_public_key_sha256: tls.public_key_sha256,
    };
    let attestation = attest_bundle(&MockAttestor, &bundle).unwrap();
    let listener = TcpListener::bind(SocketAddr::new(V4, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hourly = |rate: RateLimit| Quota::with_period(Duration::from_secs(3600)).unwrap().allow_burst(rate.burst);
    let limits = Arc::new(ApiLimits::with_quota(&api, hourly, DefaultClock::default()));
    let who = get(|ConnectInfo(client): ConnectInfo<SocketAddr>| async move { client.to_string() });
    let app = Router::new()
        .route("/v1/health", who)
        .layer(axum::middleware::from_fn_with_state(limits.clone(), limit_client));
    let server = tokio::spawn(serve(listener, tls.acceptors, app, limits, api));
    let (stream, _) = broadcast::channel(16);
    let sequencer = tokio::spawn(async {});
    TestNode { port, attestation, stream, server, sequencer, key: tls.key }
}

impl TestNode {
    /// A handshake for `api.engine.test` offering `alpn`, accepting any certificate, after `prefix`.
    /// Error cases:
    /// - the connection or handshake fails
    async fn validate_after(&self, prefix: &[u8], alpn: &[Vec<u8>]) -> io::Result<()> {
        let provider = Arc::new(default_provider());
        let verifier = Arc::new(Unverified(provider.clone()));
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        config.alpn_protocols = alpn.to_vec();
        let mut tcp = TcpStream::connect((V4, self.port)).await?;
        tcp.write_all(prefix).await?;
        TlsConnector::from(Arc::new(config)).connect(ServerName::try_from("api.engine.test").unwrap(), tcp).await?;
        Ok(())
    }
}
