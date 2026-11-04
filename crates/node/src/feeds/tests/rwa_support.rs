//! Test support for the RWA feed tests: schedules open around the clock, signed relistings that
//! move an open or a roll after setup, a record of every command the sequencer applies, and TLS in
//! front of the plaintext vendor stand-ins.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::DateTime;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::session::{Period, Schedule};
use engine_core::types::{MarketId, Millis};
use k256::ecdsa::SigningKey;
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use rustls::crypto::ring;
use rustls::pki_types::PrivatePkcs8KeyDer;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsAcceptor;

use crate::admin::sign;
use crate::clock::now_ms;
use crate::feeds::tests::fake::WAIT;
use crate::feeds::tests::scenario::Node;
use crate::sequencer::{Query, Request, StreamBody};
use crate::test_support::dev_config;

/// Every command the sequencer applied, with its stamp and events, in order.
pub type Applied = Arc<Mutex<Vec<(Millis, Command, Vec<Event>)>>>;

/// A test CA, and loopback TLS fronts that relay to plaintext stand-ins under leaves it issues for
/// `127.0.0.1`, so a feed reaches a stand-in only over TLS it verifies against this CA alone.
pub struct TlsFront {
    issuer: Issuer<'static, KeyPair>,
    /// A venue connection's TLS client config, as the node builds it but trusting only this CA.
    pub client: Arc<ClientConfig>,
}

impl TlsFront {
    /// A fresh CA and the client config that trusts it.
    pub fn new() -> Self {
        let ca_key = KeyPair::generate().expect("a CA key");
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA parameters");
        ca.distinguished_name.push(DnType::CommonName, "vendor stand-in CA");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let root = ca.self_signed(&ca_key).expect("a CA certificate");
        let mut roots = RootCertStore::empty();
        roots.add(root.der().clone()).expect("the CA's root");
        let client = ClientConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("TLS protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        TlsFront { issuer: Issuer::new(ca, ca_key), client: Arc::new(client) }
    }

    /// Serves TLS on a fresh loopback port, relaying each connection to the stand-in at `plain`, a
    /// `ws://` URL. Returns the `wss://` URL that reaches it, and the task, which stops the front
    /// when aborted.
    pub async fn relay(&self, plain: &str) -> (String, AbortHandle) {
        let key = KeyPair::generate().expect("a leaf key");
        let params = CertificateParams::new(vec!["127.0.0.1".to_owned()]).expect("leaf parameters");
        let leaf = params.signed_by(&key, &self.issuer).expect("a leaf certificate");
        let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("TLS protocol versions")
            .with_no_client_auth()
            .with_single_cert(vec![leaf.der().clone()], PrivatePkcs8KeyDer::from(key.serialize_der()).into())
            .expect("a server config");
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let target = plain.strip_prefix("ws://").expect("a ws:// stand-in").to_owned();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let url = format!("wss://{}", listener.local_addr().expect("local addr"));
        let front = tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let (acceptor, target) = (acceptor.clone(), target.clone());
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                    let Ok(mut plain) = TcpStream::connect(&target).await else { return };
                    let _ = copy_bidirectional(&mut tls, &mut plain).await;
                });
            }
        });
        (url, front.abort_handle())
    }
}

impl Node {
    /// Records every command the sequencer applies from now on.
    pub(super) fn record(&self) -> Applied {
        let applied: Applied = Arc::default();
        let (mut stream, sink) = (self.stream.resubscribe(), applied.clone());
        tokio::spawn(async move {
            while let Ok(message) = stream.recv().await {
                if let StreamBody::Command(entry) = &message.body {
                    let events = entry.events.clone().unwrap_or_default();
                    sink.lock().expect("recorder").push((entry.sequenced.ts, entry.sequenced.cmd.clone(), events));
                }
            }
        });
        applied
    }

    /// Relists `spec`, signed by the dev operator at its next nonce, and checks it applied.
    pub(super) async fn relist(&self, spec: MarketSpec) {
        let operator = dev_config().operator().expect("the dev operator");
        let key = SigningKey::from_slice(&[0x42; 32]).expect("the dev operator key");
        let (reply, nonce) = oneshot::channel();
        let query: Query = Box::new(move |engine| {
            let _ = reply.send(engine.next_request_nonce(operator.address));
        });
        self.tx.send(Request::Query(query)).await.expect("sequencer up");
        let nonce = timeout(WAIT, nonce).await.expect("nonce in time").expect("nonce");
        let listing = sign(&key, &operator, AdminCommand::UpsertMarket(spec), nonce, now_ms() + 60_000);
        let events = self.submit(Command::SignedAdmin(Box::new(listing))).await;
        assert!(events.contains(&Event::AdminApplied), "{events:?}");
    }
}

/// The listing of dev genesis market `id`.
pub fn listing(id: MarketId) -> MarketSpec {
    dev_config().genesis.markets.into_iter().find(|market| market.id == id).expect("a dev genesis market")
}

/// Every hour of the week in UTC, less a closure over `[from, to)`, if any.
pub fn around_the_clock(closed: Option<(Millis, Millis)>) -> Schedule {
    let local = |ts: Millis| DateTime::from_timestamp_millis(ts).expect("in range").naive_utc();
    let closures: Vec<Period> =
        closed.map(|(from, to)| Period { from: local(from), to: local(to) }).into_iter().collect();
    let windows = serde_json::json!([{ "open": "Mon 00:00", "close": "Thu 00:00" }, { "open": "Thu 00:00", "close": "Mon 00:00" }]);
    let schedule = serde_json::json!({ "kind": "weekly", "tz": "UTC", "windows": windows, "closures": closures });
    serde_json::from_value(schedule).expect("a weekly schedule")
}

/// Waits until `found` returns something, failing after `within`. `found` reads the record itself,
/// so no lock is held while the sequencer runs.
pub async fn wait_for<T>(within: Duration, found: impl Fn() -> Option<T>) -> T {
    let search = async {
        loop {
            if let Some(found) = found() {
                return found;
            }
            sleep(Duration::from_millis(20)).await;
        }
    };
    timeout(within, search).await.expect("found in time")
}

/// Every command applied so far.
pub fn commands(applied: &Applied) -> Vec<(Millis, Command, Vec<Event>)> {
    applied.lock().expect("recorder").clone()
}

/// Every event applied so far.
pub fn events_of(applied: &Applied) -> Vec<Event> {
    applied.lock().expect("recorder").iter().flat_map(|(_, _, events)| events.clone()).collect()
}

/// When each price command for `market` was stamped, with the contracts its quotes name.
pub fn priced(applied: &Applied, market: MarketId) -> Vec<(Millis, Vec<Option<String>>)> {
    let applied = applied.lock().expect("recorder");
    applied
        .iter()
        .filter_map(|(ts, cmd, _)| match cmd {
            Command::Prices { market: priced, quotes } if *priced == market => {
                Some((*ts, quotes.iter().map(|quote| quote.contract.clone()).collect()))
            }
            _ => None,
        })
        .collect()
}
