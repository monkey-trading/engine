//! The public API end to end over TLS: a client that pins the attested key, the request, body,
//! connection, and subscriber limits, the timeouts, and shutdown. Rate buckets refill once an hour
//! here, so no test depends on how fast it runs.

use super::*;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use engine_core::Fixed;
use engine_core::auth::ClientAuth;
use engine_core::command::{OrderKind, OrderRequest};
use engine_core::types::{AccountId, Side};
use governor::Quota;
use governor::clock::DefaultClock;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant as TokioInstant;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream as ClientTls;
use webpki::EndEntityCert;

use crate::api::{ApiState, MAX_BACKFILLS, router};
use crate::clock::SystemClock;
use crate::config::RateLimit;
use crate::history::Reader;
use crate::limits::ApiLimits;
use crate::log::Manifest;
use crate::sequencer::{Request, StreamMsg};
use crate::tee::{Attestation, AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::dev_config;

const V4: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const V6: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);

/// A node's API on a free port, with a sequencer that answers every command with no events and
/// stops once every sender to it is gone.
struct TestNode {
    port: u16,
    attestation: Attestation,
    stream: broadcast::Sender<Arc<StreamMsg>>,
    server: JoinHandle<anyhow::Result<()>>,
    sequencer: JoinHandle<()>,
    key: TlsKey,
}

/// Checks what a client that verified the attestation checks: that the server's certificate key
/// hashes to the attested value, if one is pinned. Records the hash either way.
#[derive(Debug)]
struct PinnedKey {
    pin: Option<[u8; 32]>,
    seen: Arc<Mutex<Option<[u8; 32]>>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedKey {
    /// Accepts the certificate when its key matches the pin, or when nothing is pinned.
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let cert = EndEntityCert::try_from(end_entity).map_err(|e| rustls::Error::General(e.to_string()))?;
        let hash: [u8; 32] = Sha256::digest(cert.subject_public_key_info()).into();
        *self.seen.lock().unwrap() = Some(hash);
        match self.pin {
            Some(pin) if pin != hash => Err(rustls::Error::General("key does not match the attestation".into())),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    /// Checks the handshake signature with the certificate's key.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    /// Checks the handshake signature with the certificate's key.
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    /// Every scheme the provider verifies.
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[tokio::test]
async fn the_certificate_key_is_the_attested_one() {
    let node = TestNode::start(api(), V4).await;
    let seen = Arc::new(Mutex::new(None));
    let mut conn = node.connect_with(V4, None, seen.clone()).await.unwrap();
    let (status, body) = http(&mut conn, "GET", "/v1/attestation", "").await;
    assert_eq!(status, 200);

    let served: serde_json::Value = serde_json::from_str(&body).unwrap();
    let field = |name: &str| hex::decode(served[name].as_str().unwrap().trim_start_matches("0x")).unwrap();
    let (bundle, report_data) = (field("bundle"), field("report_data"));
    let presented = seen.lock().unwrap().expect("the handshake presented a certificate");
    assert_eq!(report_data[..32], Sha256::digest(&bundle)[..], "report data bytes 0..32 hash the served bundle");
    assert_eq!(bundle[73..105], presented, "bundle bytes 73..105 are the presented key's hash");
    assert_eq!(served["tls_public_key_sha256"], format!("0x{}", hex::encode(presented)));
    assert!(served["quote"].as_str().unwrap().ends_with(&hex::encode(&report_data)));

    // A client pinned to the attested key connects; one pinned to another key does not.
    node.connect(V4).await.unwrap();
    let wrong = node.connect_with(V4, Some([0; 32]), Arc::default()).await;
    assert!(wrong.is_err(), "a pin that is not the attested key");
}

#[tokio::test]
async fn a_flooded_client_gets_429_while_another_is_served() {
    let mut api = api();
    api.per_ip = rate(1, 5);
    // Both loopbacks reach one dual-stack socket; IPv4 arrives mapped into IPv6.
    let node = TestNode::start(api, IpAddr::V6(Ipv6Addr::UNSPECIFIED)).await;

    let mut statuses = Vec::new();
    for _ in 0..8 {
        let mut conn = node.connect(V4).await.unwrap();
        let (status, head) = http_head(&mut conn, "GET", "/v1/health", "").await;
        statuses.push(status);
        if status == 429 {
            let retry_after =
                head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("retry-after: ").map(str::to_owned));
            // The bucket refills in an hour, less the time the test has run.
            let secs: u64 = retry_after.expect("a 429 says when to retry").parse().unwrap();
            assert!((1..=3600).contains(&secs), "retry after {secs}s");
        }
    }
    assert_eq!(statuses, [200, 200, 200, 200, 200, 429, 429, 429], "the burst is served, the rest refused");

    let mut other = node.connect(V6).await.unwrap();
    assert_eq!(http(&mut other, "GET", "/v1/health", "").await.0, 200);
    let mut flooder = node.connect(V4).await.unwrap();
    assert_eq!(http(&mut flooder, "GET", "/v1/markets", "").await.0, 429, "the limit is per client, not per route");
}

#[tokio::test]
async fn an_account_over_its_rate_gets_429() {
    let mut api = api();
    api.per_account = rate(1, 2);
    let node = TestNode::start(api, V4).await;
    let (a, b) = (order(AccountId([1; 20])), order(AccountId([2; 20])));
    let mut statuses = Vec::new();
    for body in [&a, &a, &a, &b] {
        let mut conn = node.connect(V4).await.unwrap();
        statuses.push(http(&mut conn, "POST", "/v1/orders", body).await.0);
    }
    assert_eq!(statuses, [200, 200, 429, 200]);
}

#[tokio::test]
async fn the_body_cap_admits_its_size_and_refuses_one_byte_more() {
    let node = TestNode::start(api(), V4).await;
    let json = format!(r#"{{"account":"{}","amount":"1"}}"#, AccountId([1; 20]));
    // A route that parses its body, and one that ignores it, which only the read cap guards.
    for (method, path) in [("POST", "/v1/withdrawals"), ("GET", "/v1/health")] {
        for (len, status) in [(1024, 200), (1025, 413)] {
            // Trailing spaces pad the JSON to `len` bytes.
            let body = format!("{json:<len$}");
            let mut conn = node.connect(V4).await.unwrap();
            assert_eq!(http(&mut conn, method, path, &body).await.0, status, "{method} {path} with {len} bytes");
        }
    }
}

#[tokio::test]
async fn connections_over_the_per_client_cap_are_closed_until_one_closes() {
    let mut api = api();
    api.max_connections_per_ip = NonZeroUsize::new(2).unwrap();
    let node = TestNode::start(api, V4).await;
    let first = node.connect(V4).await.unwrap();
    let _second = node.connect(V4).await.unwrap();
    assert!(node.connect(V4).await.is_err(), "a third connection from one client");

    drop(first);
    node.connect_eventually(V4).await;
}

#[tokio::test]
async fn a_stalled_handshake_is_closed_at_the_timeout() {
    let mut api = api();
    api.max_connections_per_ip = NonZeroUsize::new(1).unwrap();
    api.handshake_timeout = Duration::from_millis(200);
    let node = TestNode::start(api, V4).await;
    let mut stalled = TcpStream::connect((V4, node.port)).await.unwrap();
    assert!(node.connect(V4).await.is_err(), "the stalled connection holds the only place");

    let closed = timeout(Duration::from_secs(5), stalled.read(&mut [0u8; 1])).await;
    assert_eq!(closed.expect("closed by the server").unwrap(), 0);
    node.connect_eventually(V4).await;
}

#[tokio::test]
async fn a_client_over_its_connection_rate_is_refused_while_another_connects() {
    let mut api = api();
    api.per_ip_connections = rate(1, 3);
    let node = TestNode::start(api, IpAddr::V6(Ipv6Addr::UNSPECIFIED)).await;
    for _ in 0..3 {
        node.connect(V4).await.unwrap();
    }
    assert!(node.connect(V4).await.is_err(), "a fourth new connection from one client");
    node.connect(V6).await.unwrap();
}

#[tokio::test]
async fn a_body_that_stalls_gets_408_and_frees_its_connection() {
    let mut api = api();
    api.max_connections_per_ip = NonZeroUsize::new(1).unwrap();
    api.read_timeout = Duration::from_millis(200);
    let node = TestNode::start(api, V4).await;
    let mut conn = node.connect(V4).await.unwrap();
    let head =
        "POST /v1/withdrawals HTTP/1.1\r\nhost: engine\r\ncontent-type: application/json\r\ncontent-length: 10\r\n\r\n";
    conn.write_all(head.as_bytes()).await.unwrap();

    let response = timeout(Duration::from_secs(5), read_until_closed(&mut conn)).await.expect("answered and closed");
    assert!(response.starts_with("HTTP/1.1 408"), "{response}");
    node.connect_eventually(V4).await;
}

#[tokio::test]
async fn an_unreadable_body_gets_400() {
    let node = TestNode::start(api(), V4).await;
    let mut conn = node.connect(V4).await.unwrap();
    let request = "POST /v1/withdrawals HTTP/1.1\r\nhost: engine\r\ntransfer-encoding: chunked\r\n\r\nnot-a-size\r\n";
    conn.write_all(request.as_bytes()).await.unwrap();
    let response = timeout(Duration::from_secs(5), read_until_closed(&mut conn)).await.expect("answered and closed");
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
}

#[tokio::test]
async fn a_client_that_stops_reading_responses_gives_up_its_place() {
    let mut api = api();
    api.max_connections_per_ip = NonZeroUsize::new(1).unwrap();
    api.send_timeout = Duration::from_millis(200);
    let node = TestNode::start(api, V4).await;
    let mut conn = node.connect(V4).await.unwrap();
    // Pipelines requests and never reads a response, until the server closes the connection.
    let requests = "GET /v1/health HTTP/1.1\r\nhost: engine\r\n\r\n".repeat(1000);
    let flood = async { while conn.write_all(requests.as_bytes()).await.is_ok() {} };
    timeout(Duration::from_secs(30), flood).await.expect("the server closed the connection");
    node.connect_eventually(V4).await;
}

#[tokio::test]
async fn stopping_the_server_closes_its_connections() {
    let mut api = api();
    api.read_timeout = Duration::from_secs(60);
    let node = TestNode::start(api, V4).await;
    let mut idle = node.connect(V4).await.unwrap();

    node.server.abort();
    // The sequencer stops only once no connection holds a sender to it.
    timeout(Duration::from_secs(5), node.sequencer).await.expect("every sender dropped").unwrap();
    timeout(Duration::from_secs(5), read_until_closed(&mut idle)).await.expect("the connection closed");
}

#[tokio::test]
async fn a_connection_that_sends_no_request_is_closed_at_the_read_timeout() {
    let mut api = api();
    api.max_connections_per_ip = NonZeroUsize::new(1).unwrap();
    api.read_timeout = Duration::from_millis(200);
    let node = TestNode::start(api, V4).await;
    let mut idle = node.connect(V4).await.unwrap();
    assert!(node.connect(V4).await.is_err(), "the idle connection holds the only place");

    // The server may drop the connection without a TLS close_notify.
    let closed = timeout(Duration::from_secs(5), idle.read(&mut [0u8; 1])).await.expect("closed by the server");
    assert!(
        matches!(&closed, Ok(0)) || matches!(&closed, Err(e) if e.kind() == ErrorKind::UnexpectedEof),
        "{closed:?}"
    );
    node.connect_eventually(V4).await;
}

#[tokio::test(start_paused = true)]
async fn an_accept_failure_backs_off_and_alerts_unless_the_client_reset() {
    let alert = ThrottledAlert::new("test", "test");
    let started = TokioInstant::now();
    back_off(io::Error::from(ErrorKind::ConnectionReset), &alert).await;
    assert_eq!(started.elapsed(), Duration::ZERO, "a client's reset is not waited out");
    assert!(alert.due(Instant::now()), "nor alerted");

    let alert = ThrottledAlert::new("test", "test");
    back_off(io::Error::other("too many open files"), &alert).await;
    assert_eq!(started.elapsed(), Duration::from_secs(1));
    assert!(!alert.due(Instant::now()), "the failure was alerted");
}

mod acme;
mod proxy;
mod stream;

// ── Helpers ──

impl TestNode {
    /// Serves the API on a free port of `ip` under `api`'s limits.
    async fn start(api: ApiSection, ip: IpAddr) -> Self {
        let tls = TlsIdentity::generate().unwrap();
        let bundle = AttestationBundle {
            enclave_address: EnclaveKey::generate().address(),
            config_sha256: [0; 32],
            tls_public_key_sha256: tls.public_key_sha256,
        };
        let attestation = attest_bundle(&MockAttestor, &bundle).unwrap();
        let listener = TcpListener::bind(SocketAddr::new(ip, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, mut rx) = mpsc::channel(64);
        let sequencer = tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                if let Request::Submit { reply: Some(reply), .. } = req {
                    let _ = reply.send(Vec::new());
                }
            }
        });
        let (stream, _) = broadcast::channel(16);
        let hourly = |rate: RateLimit| Quota::with_period(Duration::from_secs(3600)).unwrap().allow_burst(rate.burst);
        let limits = Arc::new(ApiLimits::with_quota(&api, hourly, DefaultClock::default()));
        let state = ApiState {
            protocol: engine_core::protocol::ProtocolVersion::V23,
            access: None,
            tx,
            stream: stream.clone(),
            attestation: Arc::new(attestation.clone()),
            auth: ClientAuth::Unsigned,
            operator: dev_config().operator().unwrap(),
            clock: Arc::new(SystemClock),
            mock_custody: true,
            payouts_enabled: true,
            log_dir: PathBuf::new(),
            manifest: watch::channel(Manifest::default()).1,
            backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
            limits: limits.clone(),
            history: Reader::new(PathBuf::new(), watch::channel(true).1),
            marks: crate::test_support::marks_reader(),
            stats: crate::test_support::stats(),
            emission_budget: None,
            collateral: None,
            listing: Default::default(),
            signing: crate::test_support::signing(),
        };
        let server = tokio::spawn(serve(listener, tls.acceptors, router(state), limits, api));
        Self { port, attestation, stream, server, sequencer, key: tls.key }
    }

    /// A TLS connection from `from`, pinned to the attested key.
    /// Error cases:
    /// - the connection or handshake fails
    async fn connect(&self, from: IpAddr) -> io::Result<ClientTls<TcpStream>> {
        self.connect_with(from, Some(attested_tls_key(&self.attestation)), Arc::default()).await
    }

    /// A TLS connection from `from` that checks the server's key against `pin` and records its hash
    /// in `seen`.
    /// Error cases:
    /// - the connection or handshake fails
    async fn connect_with(
        &self,
        from: IpAddr,
        pin: Option<[u8; 32]>,
        seen: Arc<Mutex<Option<[u8; 32]>>>,
    ) -> io::Result<ClientTls<TcpStream>> {
        let provider = Arc::new(default_provider());
        let verifier = Arc::new(PinnedKey { pin, seen, provider: provider.clone() });
        let config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        let tcp = TcpStream::connect((from, self.port)).await?;
        // A connection over a cap is closed before the handshake, so this fails.
        TlsConnector::from(Arc::new(config)).connect(ServerName::try_from("engine").unwrap(), tcp).await
    }

    /// Connects from `from` once the server has freed a place, trying a bounded number of times.
    async fn connect_eventually(&self, from: IpAddr) {
        for _ in 0..500 {
            if self.connect(from).await.is_ok() {
                return;
            }
        }
        panic!("no place was freed");
    }
}

/// An `[api]` section with generous limits, for a test to tighten the one it checks.
fn api() -> ApiSection {
    ApiSection {
        max_connections: NonZeroUsize::new(64).unwrap(),
        max_connections_per_ip: NonZeroUsize::new(64).unwrap(),
        handshake_timeout: Duration::from_secs(5),
        read_timeout: Duration::from_secs(5),
        max_body_bytes: NonZeroUsize::new(1024).unwrap(),
        per_ip: rate(1, 5000),
        per_account: rate(1, 5000),
        per_ip_connections: rate(1, 5000),
        max_subscribers: NonZeroUsize::new(8).unwrap(),
        send_timeout: Duration::from_secs(5),
        heartbeat_interval: Duration::from_secs(5),
        quote_interval: Duration::from_millis(100),
        quote_max_markets: NonZeroUsize::new(4).unwrap(),
        stats_interval: Duration::from_secs(60),
        allowed_origins: Vec::new(),
        tls: None,
        proxy_protocol: None,
    }
}

/// A bucket of `burst`, refilled at `per_second` (once an hour under `TestNode`).
fn rate(per_second: u32, burst: u32) -> RateLimit {
    RateLimit { per_second: NonZeroU32::new(per_second).unwrap(), burst: NonZeroU32::new(burst).unwrap() }
}

/// A market buy order from `account`, as JSON.
fn order(account: AccountId) -> String {
    let kind = OrderKind::Market { quoted: Fixed::from_int(100), max_slippage_bps: None };
    let req = OrderRequest {
        account,
        client_id: 1,
        market: 1,
        side: Side::Buy,
        qty: Fixed::ONE,
        kind,
        reduce_only: false,
        auth: None,
    };
    serde_json::to_string(&req).unwrap()
}

/// Sends one request on `conn` and returns the status and body.
async fn http(conn: &mut ClientTls<TcpStream>, method: &str, path: &str, body: &str) -> (u16, String) {
    let (status, response) = http_head(conn, method, path, body).await;
    let body = response.split_once("\r\n\r\n").map(|(_, body)| body.to_owned()).unwrap_or_default();
    (status, body)
}

/// Sends one request on `conn`, asking the server to close it after, and returns the status and
/// the whole response.
async fn http_head(conn: &mut ClientTls<TcpStream>, method: &str, path: &str, body: &str) -> (u16, String) {
    let request = format!(
        "{method} {path} HTTP/1.1\r\nhost: engine\r\nconnection: close\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n\r\n{body}",
        body.len()
    );
    conn.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    conn.read_to_end(&mut response).await.unwrap();
    let response = String::from_utf8(response).unwrap();
    (response[9..12].parse().unwrap(), response)
}

/// The TLS key hash a verifier pins: bundle bytes 73..105, once the bundle's SHA-256 matches report
/// data bytes 0..32.
fn attested_tls_key(attestation: &Attestation) -> [u8; 32] {
    let bundle = hex::decode(attestation.bundle.trim_start_matches("0x")).unwrap();
    let report_data = hex::decode(attestation.report_data.trim_start_matches("0x")).unwrap();
    assert_eq!(report_data[..32], Sha256::digest(&bundle)[..], "the bundle is the attested one");
    bundle[73..105].try_into().unwrap()
}

/// Everything the server sends on `conn` until it closes the connection, with or without a TLS
/// close_notify.
async fn read_until_closed(conn: &mut ClientTls<TcpStream>) -> String {
    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = conn.read(&mut buf).await {
        received.extend_from_slice(&buf[..n]);
    }
    String::from_utf8(received).unwrap()
}
