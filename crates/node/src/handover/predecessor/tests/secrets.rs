//! Moving the delivered secrets over loopback: with an approved upgrade, never with an election, and
//! never with a stop.

use super::*;

use k256::ecdsa::SigningKey;

use crate::handover::fetch_secrets;
use crate::secrets::Bundle;
use crate::secrets::tests::{bundle, full};

/// What the successor's dev config reads, which the test's bundle fills.
fn named(exchange: &Exchange) -> Vec<crate::config::NamedSecret> {
    let mut cfg = crate::secrets::tests::config("handover-secrets-names");
    cfg.custody.chain_id = exchange.successor.custody.chain_id;
    cfg.named_secrets().unwrap()
}

impl Exchange {
    /// The successor's side, asking for the secrets alone, signed for by `operator`.
    async fn fetch(&self, operator: AccountId) -> anyhow::Result<crate::secrets::SignedBundle> {
        let sealer = MockSealer { launch_digest: UPGRADE };
        fetch_secrets(&self.successor, &self.address, &sealer, &MockAttestor, operator, &named(self)).await
    }

    /// Has the predecessor hold `bundle`, signed by `key`.
    fn hold(&self, bundle: Bundle, key: &SigningKey) {
        self.secrets.replace(bundle.sign(self.domain(), key)).unwrap();
    }

    /// The dev config's operator, whose key `crate::secrets::tests::operator_key` is.
    fn operator(&self) -> AccountId {
        self.successor.operator().unwrap().address
    }
}

fn operator_key() -> SigningKey {
    crate::secrets::tests::operator_key()
}

#[tokio::test]
async fn an_approved_upgrade_takes_the_secrets_first_and_sequencing_goes_on() {
    let mut exchange = Exchange::start("secrets-upgrade").await;
    exchange.make_upgrade_current();
    exchange.hold(full(3, "a"), &operator_key());
    let signed = exchange.fetch(exchange.operator()).await.unwrap();
    assert_eq!(signed.bundle, full(3, "a"));
    assert_eq!(signed.signer(exchange.domain()).unwrap(), exchange.operator(), "the operator's signature came along");
    assert!(exchange.drains.try_recv().is_err(), "moving the secrets stopped sequencing");
    assert!(!exchange.run.is_finished(), "moving the secrets ended the predecessor");

    // The keys' handover still works afterwards, and is what ends it.
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    exchange.take().await.unwrap();
    assert_eq!(answer.await.unwrap(), 1, "the keys' handover stopped sequencing once");
}

#[tokio::test]
async fn a_release_the_vault_does_not_name_gets_no_secrets() {
    let mut exchange = Exchange::start("secrets-not-named").await;
    exchange.hold(full(3, "a"), &operator_key());
    let err = format!("{:#}", exchange.fetch(exchange.operator()).await.unwrap_err());
    assert!(err.contains("the vault's current release is"), "{err}");
    assert!(exchange.drains.try_recv().is_err());
}

// #359: the keys move to the elected operator's enclave, and its vendor accounts are its own.
#[tokio::test]
async fn an_election_moves_the_keys_and_none_of_the_secrets() {
    let mut exchange = Exchange::start("secrets-election").await;
    exchange.make_upgrade_current();
    exchange.hold(full(3, "incumbents-own"), &operator_key());
    exchange.rotation.latest.store(true, Ordering::SeqCst);
    exchange.rotation.finalized.store(true, Ordering::SeqCst);
    let err = format!("{:#}", exchange.fetch(exchange.operator()).await.unwrap_err());
    assert!(err.contains("an elected operator brings its own secrets"), "{err}");
    assert!(exchange.drains.try_recv().is_err(), "the refusal stopped sequencing");

    // The keys still move, and nothing of the secrets rides with them.
    let head = write_log_tail(&exchange.successor_log, 9, &exchange.old.enclave_key().unwrap());
    let answer = exchange.answer_drain(Ok(head));
    let store = exchange.take().await.unwrap();
    assert_eq!(answer.await.unwrap(), 1);
    let sealed = serde_json::to_string(&store.keys()).unwrap();
    assert!(!sealed.contains("incumbents-own"), "the handed-over keys carry a secret");
    assert!(!sealed.contains("secrets"), "the key set has grown a place for secrets: {sealed}");
}

// Governance can throw the switch inside the finality window: the release was named at finality, so
// the state there counts, and so does the newest, whichever way it was thrown.
#[tokio::test]
async fn a_rotation_switch_on_at_either_block_moves_nothing() {
    for (what, latest, finalized) in [("at finality", false, true), ("at the latest block", true, false)] {
        let exchange = Exchange::start(&format!("secrets-switch-{latest}")).await;
        exchange.make_upgrade_current();
        exchange.hold(full(3, "a"), &operator_key());
        exchange.rotation.latest.store(latest, Ordering::SeqCst);
        exchange.rotation.finalized.store(finalized, Ordering::SeqCst);
        let err = format!("{:#}", exchange.fetch(exchange.operator()).await.unwrap_err());
        assert!(err.contains("an elected operator brings its own secrets"), "{what}: {err}");
    }
}

// Not knowing whether this is an election sends nothing.
#[tokio::test]
async fn a_rotation_switch_that_cannot_be_read_moves_nothing() {
    let exchange = Exchange::start("secrets-switch-unread").await;
    exchange.make_upgrade_current();
    exchange.hold(full(3, "a"), &operator_key());
    exchange.rotation.fails.store(true, Ordering::SeqCst);
    let err = format!("{:#}", exchange.fetch(exchange.operator()).await.unwrap_err());
    assert!(err.contains("rotation switch") && err.contains("keeps its secrets"), "{err}");
}

#[tokio::test]
async fn a_release_with_no_secrets_says_so() {
    let mut exchange = Exchange::start("secrets-none").await;
    exchange.make_upgrade_current();
    let err = format!("{:#}", exchange.fetch(exchange.operator()).await.unwrap_err());
    assert!(err.contains("holds no delivered secrets"), "{err}");
    assert!(exchange.drains.try_recv().is_err());
}

// A new release whose config names a chain key cannot go on without its bundle: it holds, asks again,
// and takes it once the vault names its release.
#[tokio::test]
async fn a_holding_start_takes_the_bundle_once_the_vault_names_its_release() {
    let exchange = Exchange::start("secrets-retry").await;
    exchange.hold(full(3, "a"), &operator_key());
    let (mut cfg, _) = successor_config("secrets-retry-holding");
    cfg.checkpoints = Some(
        toml::from_str(
            "rpc_url = 'https://rpc.example/v2/{chain_key}'\nrelayer = '0x0000000000000000000000000000000000000001'\n\
             retry_interval = '1s'\nrpc_timeout = '1s'",
        )
        .unwrap(),
    );
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    cfg.tee.sealing.as_mut().unwrap().secrets = Some(toml::from_str(&format!("listen = '127.0.0.1:{port}'")).unwrap());
    let start = crate::KeyStart {
        predecessor: Some(exchange.address.clone()),
        mock_launch_digest: Some(UPGRADE),
        ..Default::default()
    };
    let mut starting = tokio::spawn(async move {
        let metrics = Arc::new(crate::metrics::Metrics::new());
        let (_, alerts) = crate::alerts::channel(crate::alerts::Routes::default(), metrics.clone());
        let mut telemetry = crate::telemetry::Telemetry { metrics, alerts, pages_critical: false };
        let delivered = crate::node::secrets::startup(&mut cfg, &mut telemetry, &start).await;
        (cfg, delivered)
    });
    assert!(timeout(Duration::from_millis(300), &mut starting).await.is_err(), "it went on without its chain key");

    exchange.make_upgrade_current();
    let (cfg, delivered) = timeout(Duration::from_secs(10), starting).await.expect("it still holds").unwrap();
    assert_eq!(delivered.unwrap().store.unwrap().version(), Some(3));
    let rpc = cfg.checkpoints.as_ref().unwrap().rpc_url.expose().unwrap();
    assert_eq!(rpc.path(), "/v2/FAKE-chain-a");
}

// What answers cannot be checked yet: the operator's signature is what the successor takes.
#[tokio::test]
async fn a_bundle_another_key_signed_is_refused_whoever_sends_it() {
    let exchange = Exchange::start("secrets-host-bundle").await;
    exchange.make_upgrade_current();
    let host = SigningKey::from_slice(&[0x77; 32]).unwrap();
    exchange.hold(full(3, "host"), &host);
    let err = format!("{:#}", exchange.fetch(exchange.operator()).await.unwrap_err());
    assert!(err.contains("not this config's operator"), "{err}");
}

// What the operator signed stays whole, so the signature still verifies at the next upgrade.
#[tokio::test]
async fn a_bundle_chosen_under_another_config_moves_whole_with_its_signature() {
    let exchange = Exchange::start("secrets-new-config").await;
    exchange.make_upgrade_current();
    let chosen = bundle(3, &[("chain_key", "FAKE-chain"), ("dropped_by_the_new_config", "FAKE-old")]);
    exchange.hold(chosen.clone(), &operator_key());
    let signed = exchange.fetch(exchange.operator()).await.unwrap();
    assert_eq!(signed.bundle, chosen);
    assert_eq!(signed.signer(exchange.domain()).unwrap(), exchange.operator());
}

// A host that flips the flag makes the quote bind another request, in either direction.
#[tokio::test]
async fn a_host_cannot_turn_a_request_for_the_keys_into_one_for_the_secrets_or_back() {
    let mut exchange = Exchange::start("secrets-flip").await;
    exchange.make_upgrade_current();
    exchange.hold(full(3, "a"), &operator_key());

    // A request for the keys, flipped: the quote binds the keys' tag.
    let (mut stream, _, mut request) = exchange.connect(exchange.successor.file_sha256).await;
    request.secrets_only = true;
    send(&mut stream, &request).await.unwrap();
    let reply: Reply = timeout(Duration::from_secs(5), receive(&mut stream)).await.expect("no answer in time").unwrap();
    assert!(matches!(reply, Reply::Refused { .. }), "secrets for a request the quote does not bind");

    // A request for the secrets, flipped: no stop.
    let mut stream = TcpStream::connect(&exchange.address).await.unwrap();
    let challenge: Challenge = receive(&mut stream).await.unwrap();
    let challenge: [u8; 32] = hex::decode(challenge.nonce).unwrap().try_into().unwrap();
    let (_, public) = <Kx as hpke::Kem>::gen_keypair();
    let public = hpke::Serializable::to_bytes(&public).to_vec();
    let config = exchange.successor.file_sha256;
    let bound = crate::handover::secrets::request_report_data(exchange.domain(), &challenge, &public, &config);
    let platform = Platform { launch_digest: UPGRADE, policy: 0, tcb: 0 };
    let quoting = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
    let evidence = evidence::produce(&quoting, &MockAttestor, &platform, bound).await.unwrap();
    let flipped = Request {
        public_key: hex::encode(&public),
        config_sha256: hex::encode(config),
        log_tip: None,
        secrets_only: false,
        evidence,
    };
    send(&mut stream, &flipped).await.unwrap();
    let reply: Reply = timeout(Duration::from_secs(5), receive(&mut stream)).await.expect("no answer in time").unwrap();
    assert!(matches!(reply, Reply::Refused { .. }), "keys for a request the quote does not bind");
    assert!(exchange.drains.try_recv().is_err(), "the flip stopped sequencing");
    assert!(!exchange.run.is_finished());
}

#[tokio::test]
async fn a_moved_bundle_opens_only_with_the_requests_key_and_binding() {
    let exchange = Exchange::start("secrets-binding").await;
    let domain = exchange.domain();
    let (secret, public) = <Kx as hpke::Kem>::gen_keypair();
    let public = hpke::Serializable::to_bytes(&public).to_vec();
    let request = [0x44u8; 64];
    let info = crate::handover::secrets::hpke_info(domain);
    let (encapped, ciphertext) = crate::secrets::delivery::seal(&info, &public, &request, b"FAKE-bundle").unwrap();
    assert_eq!(
        crate::secrets::delivery::open(&info, &secret, &request, &encapped, &ciphertext).unwrap(),
        b"FAKE-bundle"
    );
    let (other_secret, _) = <Kx as hpke::Kem>::gen_keypair();
    assert!(crate::secrets::delivery::open(&info, &other_secret, &request, &encapped, &ciphertext).is_err());
    assert!(crate::secrets::delivery::open(&info, &secret, &[0x45u8; 64], &encapped, &ciphertext).is_err());
    let other_info = crate::handover::secrets::hpke_info(CheckpointDomain { chain_id: 1, ..domain });
    assert!(crate::secrets::delivery::open(&other_info, &secret, &request, &encapped, &ciphertext).is_err());
    // No delivery ciphertext opens as a moved bundle, nor the reverse: the two contexts differ.
    let release = Release { launch_digest: UPGRADE, config_sha256: exchange.successor.file_sha256 };
    let signed = full(1, "a").sign(domain, &operator_key());
    let nonce = [0x5c; 32];
    let delivered = crate::secrets::delivery::encrypt(domain, &release, &public, &nonce, &signed).unwrap();
    assert!(
        crate::secrets::delivery::open(
            &info,
            &secret,
            &nonce,
            &hex::decode(delivered.encapped_key).unwrap(),
            &hex::decode(delivered.ciphertext).unwrap()
        )
        .is_err()
    );
}
