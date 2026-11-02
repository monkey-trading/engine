//! A delivery from the host's side: what reaches the listener, whoever sent it, and what the listener
//! does with it. `admin::secrets`' tests run the operator's tool against the same listener.

use std::sync::Arc;

use engine_core::types::AccountId;
use hpke::Kem;
use k256::ecdsa::SigningKey;
use tokio::net::TcpStream;
use tokio::task::JoinHandle;

use super::*;
use crate::config::{NodeConfig, SecretName, SecretValue, TeeProvider};
use crate::handover::evidence::{self, Peer};
use crate::handover::{Kx, receive, send};
use crate::log::CheckpointDomain;
use crate::secrets::delivery::{Delivery, Hello, Offer, Receipt, Reply, offer_report_data, receipt_report_data};
use crate::tee::MockAttestor;
use crate::tee::sealing::{MockSealer, Release, Sealer};
use crate::tee::secrets::SecretStore;
use crate::test_support::{Written, dev_config, scratch_dir};
use crate::trusted_time::tests::logs::Logs;

/// The stand-in measurement and config hash the test enclaves run.
pub(crate) const RELEASE: [u8; 48] = [0x11; 48];
pub(crate) const CONFIG: [u8; 32] = [0xc1; 32];

/// The dev config's operator's key, which `config/dev.toml` names.
pub(crate) fn operator_key() -> SigningKey {
    SigningKey::from_slice(&[0x42; 32]).unwrap()
}

/// A dev config whose fields read a chain key, a vendor key, and a webhook URL, under this test's
/// sealing directory and a free port.
pub(crate) fn config(name: &str) -> NodeConfig {
    let mut cfg = dev_config();
    let dir = scratch_dir(name);
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    cfg.tee.use_published_mock_key = false;
    cfg.tee.sealing = Some(
        toml::from_str(&format!("dir = {:?}\n[secrets]\nlisten = '127.0.0.1:{port}'", dir.display().to_string()))
            .unwrap(),
    );
    cfg.checkpoints = Some(
        toml::from_str(
            "rpc_url = 'https://rpc.example/v2/{chain_key}'\nrelayer = '0x0000000000000000000000000000000000000001'\n\
             retry_interval = '1s'\nrpc_timeout = '1s'",
        )
        .unwrap(),
    );
    cfg.feeds.venues = Some(
        toml::from_str(
            "timeout = '10s'\nreconnect_min = '1s'\nreconnect_max = '10s'\nstale_after = '10s'\n\
             min_quote_spacing = '100ms'\n[[connections]]\nvenue = 'massive'\n\
             url = 'wss://socket.massive.com/forex'\napi_key_secret = 'vendor_key'\n\
             symbols = [{ market = 1, symbol = 'C.EURUSD' }]\n",
        )
        .unwrap(),
    );
    cfg.alerts.critical = Some(toml::from_str("url_secret = 'critical_url'").unwrap());
    cfg.file_sha256 = CONFIG;
    cfg
}

/// The deployment `cfg` serves.
pub(crate) fn domain_of(cfg: &NodeConfig) -> CheckpointDomain {
    CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse().unwrap() }
}

/// A bundle at `version` holding `pairs`.
pub(crate) fn bundle(version: u64, pairs: &[(&str, &str)]) -> Bundle {
    let secrets = pairs
        .iter()
        .map(|(name, value)| (SecretName::try_from((*name).to_owned()).unwrap(), SecretValue::test(value)))
        .collect();
    Bundle { version, secrets }
}

/// Every secret `config` reads, with obviously fake values tagged with `tag`.
pub(crate) fn full(version: u64, tag: &str) -> Bundle {
    bundle(
        version,
        &[
            ("chain_key", &format!("FAKE-chain-{tag}")),
            ("vendor_key", &format!("FAKE-vendor-{tag}")),
            ("critical_url", &format!("https://hooks.example/FAKE-hook-{tag}")),
        ],
    )
}

/// A mock enclave's delivery listener, on its own task.
pub(crate) struct Enclave {
    pub cfg: NodeConfig,
    pub address: String,
    pub store: Arc<SecretStore>,
    pub task: JoinHandle<anyhow::Result<()>>,
}

impl Enclave {
    /// An enclave of `config(name)`, still starting, so it takes the key its config names.
    pub(crate) async fn start(name: &str) -> Self {
        Self::start_running(name, None).await
    }

    /// As `start`, but once its start completed running bundle `running`, if `Some`.
    pub(crate) async fn start_running(name: &str, running: Option<Option<u64>>) -> Self {
        let cfg = config(name);
        let dir = cfg.tee.sealing.as_ref().unwrap().dir.clone();
        let sealer: Box<dyn Sealer> = Box::new(MockSealer { launch_digest: RELEASE });
        let platform = sealer.platform().unwrap();
        let store = Arc::new(SecretStore::open(&dir, sealer, CONFIG).unwrap());
        let listen = "127.0.0.1:0".parse().unwrap();
        let named = cfg.named_secrets().unwrap();
        let mut listener =
            Listener::bind(&cfg, listen, platform, Box::new(MockAttestor), store.clone(), named).await.unwrap();
        if let Some(version) = running {
            listener.started(version);
        }
        let address = listener.local_addr().to_string();
        let authority = Authority::Configured(cfg.operator().unwrap().address);
        let task = tokio::spawn(listener.run(tokio::sync::watch::channel(authority).1));
        Self { cfg, address, store, task }
    }

    /// This enclave's release.
    pub(crate) fn release(&self) -> Release {
        self.store.release()
    }

    /// A connection that has said hello and read the offer.
    pub(crate) async fn connect(&self) -> Client {
        let mut stream = TcpStream::connect(&self.address).await.unwrap();
        let nonce = [0x5c; 32];
        send(&mut stream, &Hello { nonce: hex::encode(nonce) }).await.unwrap();
        let offer: Offer = receive(&mut stream).await.unwrap();
        let public_key = hex::decode(&offer.public_key).unwrap();
        Client { stream, nonce, offer, public_key, domain: domain_of(&self.cfg), release: self.release() }
    }

    /// `bundle` under the operator's signature, encrypted on a new connection.
    pub(crate) async fn deliver(&self, bundle: Bundle) -> Reply {
        let mut client = self.connect().await;
        let delivery = client.encrypt(&bundle.sign(client.domain, &operator_key()));
        client.send(&delivery).await
    }

    /// The receipt a delivery of `bundle` gets, which must be sealed.
    pub(crate) async fn sealed(&self, bundle: Bundle) -> Receipt {
        match self.deliver(bundle).await {
            Reply::Sealed { receipt, .. } => receipt,
            Reply::Refused { reason } => panic!("the delivery was refused: {reason}"),
        }
    }

    /// The reason a delivery of `bundle` is refused, which it must be.
    pub(crate) async fn refused(&self, bundle: Bundle) -> String {
        reason(self.deliver(bundle).await)
    }
}

/// A tool's connection, or a host's.
pub(crate) struct Client {
    pub stream: TcpStream,
    pub nonce: [u8; 32],
    pub offer: Offer,
    pub public_key: Vec<u8>,
    pub domain: CheckpointDomain,
    pub release: Release,
}

impl Client {
    /// `signed`, encrypted to this connection's key for this enclave's release.
    pub(crate) fn encrypt(&self, signed: &SignedBundle) -> Delivery {
        delivery::encrypt(self.domain, &self.release, &self.public_key, &self.nonce, signed).unwrap()
    }

    /// Sends `delivery` and reads the answer.
    pub(crate) async fn send(&mut self, delivery: &Delivery) -> Reply {
        send(&mut self.stream, delivery).await.unwrap();
        receive(&mut self.stream).await.unwrap()
    }
}

/// The reason in `reply`, which must be a refusal.
pub(crate) fn reason(reply: Reply) -> String {
    match reply {
        Reply::Refused { reason } => reason,
        Reply::Sealed { receipt, .. } => panic!("sealed: {receipt:?}"),
    }
}

/// What a delivery's quote proves, for a mock enclave.
fn peer_of(evidence: &crate::handover::Evidence, report_data: &[u8; 64]) -> Peer {
    evidence::verify(TeeProvider::Mock, evidence, report_data, None, None).unwrap()
}

#[tokio::test]
async fn an_operator_signed_delivery_is_sealed_under_a_quoted_receipt() {
    let enclave = Enclave::start("secrets-delivery").await;
    let mut client = enclave.connect().await;
    let release = enclave.release();
    // The offer's quote proves the release, the one-time key, and what the enclave holds.
    let bound = offer_report_data(
        client.domain,
        &client.nonce,
        &client.public_key,
        &release.config_sha256,
        &client.offer.status,
    );
    assert_eq!(peer_of(&client.offer.evidence, &bound).launch_digest, RELEASE);
    assert_eq!(client.offer.status.sealed_version, None);
    let names: Vec<String> = client.offer.status.missing.iter().map(ToString::to_string).collect();
    assert_eq!(names, ["chain_key", "critical_url", "vendor_key"]);

    let delivery = client.encrypt(&full(1, "a").sign(client.domain, &operator_key()));
    let Reply::Sealed { receipt, evidence } = client.send(&delivery).await else { panic!("refused") };
    let expected = Receipt { version: 1, already_sealed: false, missing: Vec::new(), restart_required: false };
    assert_eq!(receipt, expected);
    let bound = receipt_report_data(client.domain, &client.nonce, &delivery, &receipt);
    assert_eq!(peer_of(&evidence, &bound).launch_digest, RELEASE);
    assert_eq!(enclave.store.current().unwrap().bundle, full(1, "a"));

    // A status read afterwards reports it, and changes nothing.
    let status = enclave.connect().await.offer.status;
    assert_eq!((status.sealed_version, status.missing), (Some(1), Vec::new()));
    assert_eq!(enclave.store.version(), Some(1));
}

// The rotated secret cannot come back: each connection has its own key, so no recorded ciphertext
// opens on a later one, and a recorded one is all a host has.
#[tokio::test]
async fn a_recorded_delivery_replayed_after_a_rotation_does_not_decrypt() {
    let enclave = Enclave::start("secrets-replay").await;
    let mut first = enclave.connect().await;
    let recorded = first.encrypt(&full(1, "old").sign(first.domain, &operator_key()));
    assert!(matches!(first.send(&recorded).await, Reply::Sealed { .. }));
    enclave.sealed(full(2, "rotated")).await;

    // Replayed on a new connection, whose key the recorded ciphertext was never encrypted to.
    let mut replay = enclave.connect().await;
    assert_ne!(replay.public_key, first.public_key, "a connection reused the one-time key");
    let reason = reason(replay.send(&recorded).await);
    assert!(reason.contains("does not decrypt"), "{reason}");
    assert_eq!(enclave.store.current().unwrap().bundle, full(2, "rotated"));
}

#[tokio::test]
async fn a_bundle_for_another_key_release_deployment_or_connection_does_not_open() {
    let enclave = Enclave::start("secrets-other").await;
    let domain = domain_of(&enclave.cfg);
    let signed = full(1, "a").sign(domain, &operator_key());
    let (_, other_key) = Kx::gen_keypair();
    let other_key = hpke::Serializable::to_bytes(&other_key).to_vec();
    let other_code = Release { launch_digest: [0x22; 48], ..enclave.release() };
    let other_config = Release { config_sha256: [0xc2; 32], ..enclave.release() };
    let other_vault = CheckpointDomain { vault: AccountId([0x0b; 20]), ..domain };
    let other_chain = CheckpointDomain { chain_id: 1, ..domain };
    for (what, open_for) in [
        ("another one-time key", (domain, enclave.release(), Some(other_key), None)),
        ("another release's code", (domain, other_code, None, None)),
        ("another release's config", (domain, other_config, None, None)),
        ("another vault", (other_vault, enclave.release(), None, None)),
        ("another chain", (other_chain, enclave.release(), None, None)),
        ("another connection's nonce", (domain, enclave.release(), None, Some([0x5d; 32]))),
    ] {
        let mut client = enclave.connect().await;
        let (domain, release, key, nonce) = open_for;
        let key = key.unwrap_or_else(|| client.public_key.clone());
        let delivery = delivery::encrypt(domain, &release, &key, &nonce.unwrap_or(client.nonce), &signed).unwrap();
        let reason = reason(client.send(&delivery).await);
        assert!(reason.contains("does not decrypt"), "{what}: {reason}");
        assert_eq!(enclave.store.current(), None, "{what} was sealed");
    }
}

// Anyone can encrypt to the offered key, the host included: only the signature admits a bundle.
#[tokio::test]
async fn a_bundle_another_key_signed_or_altered_after_signing_is_refused() {
    let enclave = Enclave::start("secrets-signers").await;
    let domain = domain_of(&enclave.cfg);
    let stranger = SigningKey::from_slice(&[0x77; 32]).unwrap();

    let mut client = enclave.connect().await;
    let delivery = client.encrypt(&full(1, "host").sign(domain, &stranger));
    let reason_given = reason(client.send(&delivery).await);
    assert!(reason_given.contains("not the operator key in effect"), "{reason_given}");

    // Signed for another deployment: the signature recovers another key here.
    let other = CheckpointDomain { chain_id: 1, ..domain };
    let mut client = enclave.connect().await;
    let delivery = client.encrypt(&full(1, "a").sign(other, &operator_key()));
    assert!(reason(client.send(&delivery).await).contains("not the operator key in effect"));

    // The operator's signature over other contents.
    let mut altered = full(1, "a").sign(domain, &operator_key());
    altered.bundle = full(1, "host");
    let mut client = enclave.connect().await;
    let delivery = client.encrypt(&altered);
    assert!(reason(client.send(&delivery).await).contains("not the operator key in effect"));

    assert_eq!(enclave.store.current(), None);
}

#[tokio::test]
async fn an_older_or_conflicting_version_is_refused_and_a_retry_after_a_lost_receipt_is_not() {
    let enclave = Enclave::start("secrets-versions-wire").await;
    // A host that drops the receipt: the enclave sealed it, and the tool cannot tell.
    let mut lost = enclave.connect().await;
    let delivery = lost.encrypt(&full(3, "three").sign(lost.domain, &operator_key()));
    send(&mut lost.stream, &delivery).await.unwrap();
    drop(lost);
    // The next connection waits its turn behind the dropped one, so the seal has happened.
    let retried = enclave.sealed(full(3, "three")).await;
    assert!(retried.already_sealed, "{retried:?}");

    let older = enclave.refused(full(2, "two")).await;
    assert!(older.contains("older than the sealed version 3"), "{older}");
    let conflicting = enclave.refused(full(3, "other")).await;
    assert!(conflicting.contains("other secrets"), "{conflicting}");
    assert_eq!(enclave.store.current().unwrap().bundle, full(3, "three"));

    let newer = enclave.sealed(full(4, "four")).await;
    assert_eq!((newer.version, newer.already_sealed), (4, false));
}

#[tokio::test]
async fn a_bundle_the_config_cannot_use_is_refused_before_sealing_and_a_partial_one_says_what_it_lacks() {
    let enclave = Enclave::start("secrets-contents").await;
    for (what, pairs, why) in [
        ("an unknown name", vec![("typo_key", "FAKE-value")], "no field of this config reads"),
        ("an empty value", vec![("chain_key", "")], "empty"),
        ("a stray newline", vec![("vendor_key", "FAKE-value\n")], "printable"),
        ("a webhook that is no URL", vec![("critical_url", "FAKE-not-a-url")], "alerts.critical"),
        ("a plain webhook off this machine", vec![("critical_url", "http://hooks.example/FAKE")], "must be an https"),
    ] {
        let refused = enclave.refused(bundle(1, &pairs)).await;
        assert!(refused.contains(why), "{what}: {refused}");
        assert!(!refused.contains("FAKE"), "{what}: the refusal quotes a value: {refused}");
        assert_eq!(enclave.store.current(), None, "{what} was sealed");
    }

    let partial = enclave.sealed(bundle(1, &[("chain_key", "FAKE-chain")])).await;
    let names: Vec<String> = partial.missing.iter().map(ToString::to_string).collect();
    assert_eq!(names, ["critical_url", "vendor_key"]);
}

#[tokio::test]
async fn a_malformed_or_abandoned_connection_ends_alone_and_the_listener_goes_on() {
    let enclave = Enclave::start("secrets-malformed").await;
    // No hello at all, and a hello that is not a nonce.
    drop(TcpStream::connect(&enclave.address).await.unwrap());
    let mut stream = TcpStream::connect(&enclave.address).await.unwrap();
    send(&mut stream, &Hello { nonce: "not hex".into() }).await.unwrap();
    assert!(receive::<Offer>(&mut stream).await.is_err(), "offered on a bad nonce");

    // Offers that are never answered, and deliveries that are not ciphertext.
    drop(enclave.connect().await);
    for (encapped_key, ciphertext) in [("zz", "00"), ("00", "zz"), ("00", "00")] {
        let mut client = enclave.connect().await;
        let delivery = Delivery { encapped_key: encapped_key.into(), ciphertext: ciphertext.into() };
        assert!(!reason(client.send(&delivery).await).is_empty());
    }
    // An encrypted message that is not a bundle says so without repeating it.
    let mut client = enclave.connect().await;
    let info = [
        b"engine.secrets.v1".as_slice(),
        &client.domain.chain_id.to_be_bytes(),
        &client.domain.vault.0,
        &client.release.id(),
    ]
    .concat();
    let (encapped, ciphertext) =
        delivery::seal(&info, &client.public_key, &client.nonce, br#"{"FAKE-junk": 1}"#).unwrap();
    let delivery = Delivery { encapped_key: hex::encode(encapped), ciphertext: hex::encode(ciphertext) };
    let reason_given = reason(client.send(&delivery).await);
    assert!(reason_given.contains("does not parse") && !reason_given.contains("FAKE"), "{reason_given}");

    assert!(!enclave.task.is_finished());
    enclave.sealed(full(1, "a")).await;
}

#[tokio::test]
async fn a_running_node_seals_a_rotation_and_says_it_needs_a_restart() {
    let (logs, _guard) = Logs::capture();
    let enclave = Enclave::start_running("secrets-mid-run", Some(Some(1))).await;
    let status = enclave.connect().await.offer.status;
    assert_eq!((status.sealed_version, status.running_version), (None, Some(1)));

    // The bundle it runs needs nothing: nothing changes.
    let same = enclave.sealed(full(1, "a")).await;
    assert!(!same.restart_required, "{same:?}");
    assert_eq!(logs.warnings("secrets_restart_required"), 0);

    let rotated = enclave.sealed(full(2, "b")).await;
    assert!(rotated.restart_required, "{rotated:?}");
    assert_eq!(enclave.store.version(), Some(2), "the rotation is sealed at once");
    assert_eq!(logs.warnings("secrets_restart_required"), 1);
    let status = enclave.connect().await.offer.status;
    assert_eq!((status.sealed_version, status.running_version), (Some(2), Some(1)), "what runs did not change");
}

#[tokio::test]
async fn a_node_that_started_without_secrets_also_says_a_first_bundle_needs_a_restart() {
    let (logs, _guard) = Logs::capture();
    let enclave = Enclave::start_running("secrets-mid-run-first", Some(None)).await;
    assert!(enclave.sealed(full(1, "a")).await.restart_required);
    assert_eq!(logs.warnings("secrets_restart_required"), 1);
}

#[tokio::test]
async fn a_node_still_starting_needs_no_restart_for_a_bundle() {
    let (logs, _guard) = Logs::capture();
    let enclave = Enclave::start("secrets-still-starting").await;
    assert!(!enclave.sealed(full(1, "a")).await.restart_required);
    assert_eq!(logs.warnings("secrets_restart_required"), 0);
}

// Every path a value can travel, at the most verbose level: what is logged, what a refusal says, and
// what `Debug` prints.
#[tokio::test]
async fn no_value_reaches_a_log_a_refusal_or_debug_output_at_any_level() {
    let written = Written::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(written.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let enclave = Enclave::start_running("secrets-no-leak", Some(None)).await;
    let domain = domain_of(&enclave.cfg);
    let mut reasons = vec![
        enclave.refused(bundle(1, &[("typo_key", "FAKE-unknown")])).await,
        enclave.refused(bundle(1, &[("vendor_key", "FAKE-with space")])).await,
        enclave.refused(bundle(1, &[("critical_url", "FAKE-not-a-url")])).await,
        enclave.refused(bundle(1, &[("critical_url", "ftp://FAKE-host/FAKE-path")])).await,
    ];
    let mut client = enclave.connect().await;
    let delivery = client.encrypt(&full(1, "x").sign(domain, &SigningKey::from_slice(&[0x77; 32]).unwrap()));
    reasons.push(reason(client.send(&delivery).await));
    enclave.sealed(full(1, "x")).await;
    reasons.push(enclave.refused(full(1, "y")).await);
    reasons.push(enclave.refused(full(0, "z")).await);
    enclave.sealed(full(2, "z")).await;

    let signed = full(3, "x").sign(domain, &operator_key());
    let printed = [
        format!("{:?}", signed),
        format!("{:?}", signed.bundle),
        format!("{:?}", signed.bundle.secrets),
        format!("{:?}", enclave.connect().await.offer),
        format!("{:?}", enclave.connect().await.encrypt(&signed)),
    ];
    for text in reasons.iter().chain(&printed) {
        assert!(!text.contains("FAKE"), "{text}");
    }
    let logged = written.text();
    assert!(logged.contains("sealed a delivered secrets bundle"), "the capture missed the node's lines: {logged}");
    assert!(!logged.contains("FAKE"), "{logged}");
}
