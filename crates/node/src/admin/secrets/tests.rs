//! The operator's tool against the enclave's listener, and what it refuses before it encrypts.

use super::*;

use engine_verify::fixtures::{
    LAUNCH_DIGEST, POLICY, REPORTED_TCB, collateral_of, crl_revoking, forged_report, pem_wrap, test_chain,
};
use tokio::net::TcpListener;

use crate::handover::evidence::Quoting;
use crate::secrets::delivery::{Delivery, Receipt};
use crate::secrets::tests::{CONFIG, Enclave, RELEASE, bundle, config, domain_of, full, operator_key};
use crate::tee::MockAttestor;
use crate::tee::sealing::Platform;
use crate::test_support::scratch_dir;

/// A release manifest for the test enclaves' stand-in measurement, listing `config`.
fn manifest(name: &str, launch_digest: [u8; 48], config: [u8; 32]) -> PathBuf {
    let dir = scratch_dir(&format!("{name}-manifest"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("release.toml");
    let text = format!(
        "name = 'test-release'\nlaunch_digest = '0x{}'\npolicy = '0x0'\n\
         [minimum_tcb]\nboot_loader = 0\ntee = 0\nsnp = 0\nmicrocode = 0\n\
         [config_hashes]\n'node.toml' = '0x{}'\n",
        hex::encode(launch_digest),
        hex::encode(config)
    );
    std::fs::write(&path, text).unwrap();
    path
}

fn peer(enclave_address: &str, release: PathBuf) -> PeerArgs {
    PeerArgs { to: enclave_address.to_owned(), release, root: None, allow_mock: true }
}

/// A bundle file holding `bundle`'s secrets at `version`.
fn bundle_file(name: &str, version: u64, tag: &str) -> PathBuf {
    let dir = scratch_dir(&format!("{name}-bundle"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("secrets.toml");
    let text = format!(
        "version = {version}\n[secrets]\nchain_key = 'FAKE-chain-{tag}'\nvendor_key = 'FAKE-vendor-{tag}'\n\
         critical_url = 'https://hooks.example/FAKE-hook-{tag}'\n"
    );
    std::fs::write(&path, text).unwrap();
    path
}

#[tokio::test]
async fn the_tool_delivers_to_the_release_its_manifest_names() {
    let enclave = Enclave::start("tool-delivers").await;
    let args = DeliverArgs {
        peer: peer(&enclave.address, manifest("tool-delivers", RELEASE, CONFIG)),
        secrets_file: bundle_file("tool-delivers", 1, "a"),
        partial: false,
    };
    deliver(&enclave.cfg, &operator_key(), &args).await.unwrap();
    assert_eq!(enclave.store.current().unwrap().bundle, full(1, "a"));

    // The retry after a lost receipt succeeds, and a rotation needs a higher version.
    deliver(&enclave.cfg, &operator_key(), &args).await.unwrap();
    let rotated = DeliverArgs {
        peer: peer(&enclave.address, manifest("tool-delivers-2", RELEASE, CONFIG)),
        secrets_file: bundle_file("tool-delivers-2", 2, "b"),
        partial: false,
    };
    deliver(&enclave.cfg, &operator_key(), &rotated).await.unwrap();
    assert_eq!(enclave.store.current().unwrap().bundle, full(2, "b"));
}

// A bundle for another config or release: the tool refuses before anything is encrypted.
#[tokio::test]
async fn the_tool_sends_nothing_to_an_enclave_its_manifest_does_not_name() {
    let enclave = Enclave::start("tool-refuses").await;
    let mut other_config = config("tool-refuses-config");
    other_config.file_sha256 = [0xc2; 32];
    for (what, cfg, release, why) in [
        (
            "another release's measurement",
            &enclave.cfg,
            manifest("tool-other-code", [0x22; 48], CONFIG),
            "launch measurement",
        ),
        (
            "a manifest that lists no such config",
            &enclave.cfg,
            manifest("tool-other-list", RELEASE, [0xc2; 32]),
            "lists no config",
        ),
        (
            "a node running another config",
            &other_config,
            manifest("tool-other-cfg", RELEASE, [0xc2; 32]),
            "the enclave runs config",
        ),
    ] {
        let args = DeliverArgs {
            peer: peer(&enclave.address, release),
            secrets_file: bundle_file(&format!("tool-refuses-{}", what.replace(' ', "-")), 1, "a"),
            partial: false,
        };
        let err = format!("{:#}", deliver(cfg, &operator_key(), &args).await.unwrap_err());
        assert!(err.contains(why), "{what}: {err}");
        assert_eq!(enclave.store.current(), None, "{what}: something was delivered");
    }
    // Without --allow-mock a mock quote never passes.
    let mut args = peer(&enclave.address, manifest("tool-no-mock", RELEASE, CONFIG));
    args.allow_mock = false;
    let Err(err) = connect(&enclave.cfg, &args).await else { panic!("a mock enclave passed") };
    assert!(format!("{err:#}").contains("never hands over to a mock one"), "{err:#}");
}

#[tokio::test]
async fn the_tool_refuses_a_bundle_the_config_cannot_use_and_one_older_than_the_enclave_holds() {
    let enclave = Enclave::start("tool-older").await;
    enclave.sealed(full(5, "five")).await;
    let older = DeliverArgs {
        peer: peer(&enclave.address, manifest("tool-older", RELEASE, CONFIG)),
        secrets_file: bundle_file("tool-older", 4, "four"),
        partial: false,
    };
    let err = deliver(&enclave.cfg, &operator_key(), &older).await.unwrap_err();
    assert!(format!("{err:#}").contains("attests it holds version 5"), "{err:#}");

    let dir = scratch_dir("tool-typo-bundle");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("secrets.toml");
    std::fs::write(&path, "version = 6\n[secrets]\ntypo_key = 'FAKE-value'\n").unwrap();
    let typo = DeliverArgs {
        peer: peer(&enclave.address, manifest("tool-typo", RELEASE, CONFIG)),
        secrets_file: path,
        partial: false,
    };
    let err = format!("{:#}", deliver(&enclave.cfg, &operator_key(), &typo).await.unwrap_err());
    assert!(err.contains("typo_key") && !err.contains("FAKE"), "{err}");
    assert_eq!(enclave.store.version(), Some(5));
}

// The whole sealed bundle goes, so a bundle lacking what the node holds for would hold it at its next
// restart: refused unless the operator means it, while one lacking a vendor key or webhook is not.
#[tokio::test]
async fn a_bundle_lacking_what_the_node_holds_for_needs_partial() {
    let enclave = Enclave::start("tool-partial").await;
    let dir = scratch_dir("tool-partial-bundle");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("secrets.toml");
    std::fs::write(&path, "version = 1\n[secrets]\nvendor_key = 'FAKE-vendor'\n").unwrap();
    let release = manifest("tool-partial", RELEASE, CONFIG);
    let refused =
        DeliverArgs { peer: peer(&enclave.address, release.clone()), secrets_file: path.clone(), partial: false };
    let err = format!("{:#}", deliver(&enclave.cfg, &operator_key(), &refused).await.unwrap_err());
    assert!(err.contains("chain_key") && err.contains("--partial"), "{err}");
    assert_eq!(enclave.store.current(), None);

    let meant =
        DeliverArgs { peer: peer(&enclave.address, release.clone()), secrets_file: path.clone(), partial: true };
    deliver(&enclave.cfg, &operator_key(), &meant).await.unwrap();
    assert_eq!(enclave.store.version(), Some(1));

    std::fs::write(&path, "version = 2\n[secrets]\nchain_key = 'FAKE-chain'\n").unwrap();
    let optional = DeliverArgs { peer: peer(&enclave.address, release), secrets_file: path, partial: false };
    deliver(&enclave.cfg, &operator_key(), &optional).await.unwrap();
    assert_eq!(enclave.store.version(), Some(2));
}

#[test]
fn what_the_host_writes_cannot_rewrite_the_operators_terminal() {
    assert_eq!(shown("refused\r\u{1b}[2Ksealed: version 2"), "refused[2Ksealed: version 2");
    assert_eq!(shown(&"x".repeat(4096)).len(), 1024);
}

// A parser's error quotes what it could not read, which the host wrote.
#[tokio::test]
async fn a_message_the_tool_cannot_read_does_not_reach_the_terminal_raw() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _: Hello = receive(&mut stream).await.unwrap();
        send(&mut stream, &serde_json::json!({ "\r\u{1b}[2Ksealed: version 9": 1 })).await.unwrap();
    });
    let cfg = config("tool-raw-field");
    let args = DeliverArgs {
        peer: peer(&address, manifest("tool-raw-field", RELEASE, CONFIG)),
        secrets_file: bundle_file("tool-raw-field", 1, "a"),
        partial: false,
    };
    let err = format!("{:#}", deliver(&cfg, &operator_key(), &args).await.unwrap_err());
    assert!(!err.chars().any(char::is_control), "{err:?}");
}

// A key the enclave does not take gets as far as the enclave, which refuses it.
#[tokio::test]
async fn the_enclave_refuses_a_key_the_operator_does_not_hold() {
    let enclave = Enclave::start("tool-key").await;
    let args = DeliverArgs {
        peer: peer(&enclave.address, manifest("tool-key", RELEASE, CONFIG)),
        secrets_file: bundle_file("tool-key", 1, "a"),
        partial: false,
    };
    let stranger = SigningKey::from_slice(&[0x77; 32]).unwrap();
    let err = deliver(&enclave.cfg, &stranger, &args).await.unwrap_err();
    assert!(format!("{err:#}").contains("not the operator key in effect"), "{err:#}");
    assert_eq!(enclave.store.current(), None);
}

/// A genuine offer from a mock enclave for `nonce`, and the secret half left out: what a host holds
/// after recording one.
async fn offer_for(nonce: [u8; 32], status: Status) -> (Offer, CheckpointDomain) {
    let cfg = config("tool-offer");
    let domain = domain_of(&cfg);
    let public_key = vec![7u8; 32];
    let bound = offer_report_data(domain, &nonce, &public_key, &CONFIG, &status);
    let evidence = mock_evidence(RELEASE, bound).await;
    let offer = Offer { public_key: hex::encode(&public_key), config_sha256: hex::encode(CONFIG), status, evidence };
    (offer, domain)
}

/// A mock enclave's evidence over `report_data`.
async fn mock_evidence(launch_digest: [u8; 48], report_data: [u8; 64]) -> Evidence {
    let platform = Platform { launch_digest, policy: 0, tcb: 0 };
    let quoting = Quoting { provider: TeeProvider::Mock, processor: None, kds_base: "https://kds.test" };
    evidence::produce(&quoting, &MockAttestor, &platform, report_data).await.unwrap()
}

// What a host can do to an offer in flight, each of which the quote's binding catches.
#[tokio::test]
async fn an_offer_the_quote_does_not_bind_is_refused() {
    let nonce = [0x5c; 32];
    let status = Status { sealed_version: Some(3), running_version: Some(3), missing: Vec::new() };
    let (genuine, domain) = offer_for(nonce, status.clone()).await;
    let path = manifest("tool-offer-manifest", RELEASE, CONFIG);
    let release = load_release(&path).unwrap();
    let check = |offer: &Offer, nonce: &[u8; 32], domain: CheckpointDomain| {
        check_offer(offer, domain, nonce, CONFIG, &release, TeeProvider::Mock, None, 0).map(|_| ())
    };
    check(&genuine, &nonce, domain).unwrap();

    // A recorded offer, answering another connection's nonce.
    let err = check(&genuine, &[0x5d; 32], domain).unwrap_err();
    assert!(format!("{err:#}").contains("binds other data"), "{err:#}");
    // Another deployment's.
    let other = CheckpointDomain { chain_id: 1, ..domain };
    assert!(check(&genuine, &nonce, other).is_err(), "another deployment's offer passed");
    // The host's own key in the enclave's offer.
    let swapped = Offer { public_key: hex::encode([9u8; 32]), ..clone_offer(&genuine) };
    assert!(check(&swapped, &nonce, domain).is_err(), "a swapped key passed");
    // A status claiming a higher version than the enclave holds, to hide a rollback, or a lower one.
    for claim in [Some(2), Some(4), None] {
        let mut lied = clone_offer(&genuine);
        lied.status.sealed_version = claim;
        assert!(check(&lied, &nonce, domain).is_err(), "a rewritten status passed: {claim:?}");
    }
    // Another config's enclave naming this config's hash.
    let mut other_config = clone_offer(&genuine);
    other_config.config_sha256 = hex::encode([0xc2; 32]);
    let err = check(&other_config, &nonce, domain).unwrap_err();
    assert!(format!("{err:#}").contains("the enclave runs config"), "{err:#}");
}

fn clone_offer(offer: &Offer) -> Offer {
    serde_json::from_value(serde_json::to_value(offer).unwrap()).unwrap()
}

// A hardware quote, on the stand-in vendor chain.
#[test]
fn a_hardware_quote_must_match_the_manifest_and_a_mock_one_never_does() {
    let chip = [0x5a; 64];
    let chain = test_chain(chip, REPORTED_TCB);
    let report_data = [0x33; 64];
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let evidence: Evidence = serde_json::from_value(serde_json::json!({
        "provider": "sev_guest",
        "quote": hex::encode(forged_report(&chain, LAUNCH_DIGEST, report_data, chip)),
        "collateral": String::from_utf8(collateral.to_json()).unwrap(),
    }))
    .unwrap();
    let root = pem_wrap(&collateral.root, "CERTIFICATE");
    let release = |digest: [u8; 48], policy: u64, tcb: engine_verify::MinimumTcb| ReleaseManifest {
        name: "hardware".into(),
        launch_digest: digest,
        guest_features: engine_verify::fixtures::GUEST_FEATURES,
        policy,
        minimum_tcb: tcb,
        config_hashes: [("node.toml".to_owned(), CONFIG)].into_iter().collect(),
    };
    let met = engine_verify::MinimumTcb {
        boot_loader: REPORTED_TCB.boot_loader,
        tee: REPORTED_TCB.tee,
        snp: REPORTED_TCB.snp,
        microcode: REPORTED_TCB.microcode,
    };
    let own = TeeProvider::ConfigfsTsm;
    let root_pem = Some(root.as_str());
    let now = 1_800_000_000;
    check_quote(own, &evidence, &report_data, &release(LAUNCH_DIGEST, POLICY, met), root_pem, now).unwrap();

    for (what, manifest, why) in [
        ("another measurement", release([0x22; 48], POLICY, met), "launch measurement"),
        ("another policy", release(LAUNCH_DIGEST, POLICY ^ 1, met), "guest policy"),
        (
            "a higher minimum TCB",
            release(LAUNCH_DIGEST, POLICY, engine_verify::MinimumTcb { snp: met.snp + 1, ..met }),
            "below the release's",
        ),
    ] {
        let err = check_quote(own, &evidence, &report_data, &manifest, root_pem, now).unwrap_err().to_string();
        assert!(err.contains(why), "{what}: {err}");
    }
    let good = release(LAUNCH_DIGEST, POLICY, met);
    assert!(check_quote(own, &evidence, &[0u8; 64], &good, root_pem, now).is_err(), "it bound other data");
    assert!(check_quote(own, &evidence, &report_data, &good, None, now).is_err(), "an unpinned root");
}

/// A stand-in enclave that gives a genuine offer, reads the delivery, and answers with what `reply`
/// makes of it: a host's forgery.
async fn forging_enclave(
    reply: impl FnOnce(Delivery, [u8; 32]) -> futures_util::future::BoxFuture<'static, Reply> + Send + 'static,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let hello: Hello = receive(&mut stream).await.unwrap();
        let nonce: [u8; 32] = hex::decode(hello.nonce).unwrap().try_into().unwrap();
        let (_, public) = <crate::handover::Kx as hpke::Kem>::gen_keypair();
        let public_key = hpke::Serializable::to_bytes(&public).to_vec();
        let status = Status { sealed_version: None, running_version: None, missing: Vec::new() };
        let domain = domain_of(&config("forging-enclave"));
        let bound = offer_report_data(domain, &nonce, &public_key, &CONFIG, &status);
        let evidence = mock_evidence(RELEASE, bound).await;
        let offer = Offer { public_key: hex::encode(public_key), config_sha256: hex::encode(CONFIG), status, evidence };
        send(&mut stream, &offer).await.unwrap();
        let delivery: Delivery = receive(&mut stream).await.unwrap();
        send(&mut stream, &reply(delivery, nonce).await).await.unwrap();
    });
    address
}

// The tool reports a seal only on a quote over this delivery's receipt.
#[tokio::test]
async fn a_forged_receipt_is_refused() {
    let receipt = |version| Receipt { version, already_sealed: false, missing: Vec::new(), restart_required: false };
    let cases: Vec<(&str, u64, u64, &str)> = vec![
        ("a quote over another version's receipt", 1, 2, "receipt's quote does not verify"),
        ("a genuine quote over a receipt for another version", 2, 2, "not this bundle"),
    ];
    for (what, claimed, quoted, why) in cases {
        let address = forging_enclave(move |delivery, nonce| {
            Box::pin(async move {
                let domain = domain_of(&config("forged-receipt"));
                let bound = receipt_report_data(domain, &nonce, &delivery, &receipt(quoted));
                Reply::Sealed { receipt: receipt(claimed), evidence: mock_evidence(RELEASE, bound).await }
            })
        })
        .await;
        let cfg = config("forged-receipt-tool");
        let args = DeliverArgs {
            peer: peer(&address, manifest(&format!("forged-{quoted}-{claimed}"), RELEASE, CONFIG)),
            secrets_file: bundle_file(&format!("forged-{quoted}-{claimed}"), 1, "a"),
            partial: false,
        };
        let err = format!("{:#}", deliver(&cfg, &operator_key(), &args).await.unwrap_err());
        assert!(err.contains(why), "{what}: {err}");
    }
    // Another measurement's quote over the right receipt.
    let address = forging_enclave(move |delivery, nonce| {
        Box::pin(async move {
            let domain = domain_of(&config("forged-measurement"));
            let bound = receipt_report_data(domain, &nonce, &delivery, &receipt(1));
            Reply::Sealed { receipt: receipt(1), evidence: mock_evidence([0x22; 48], bound).await }
        })
    })
    .await;
    let cfg = config("forged-measurement-tool");
    let args = DeliverArgs {
        peer: peer(&address, manifest("forged-measurement", RELEASE, CONFIG)),
        secrets_file: bundle_file("forged-measurement", 1, "a"),
        partial: false,
    };
    let err = format!("{:#}", deliver(&cfg, &operator_key(), &args).await.unwrap_err());
    assert!(err.contains("receipt's quote does not verify"), "{err}");
}

// A host that swallows the answer leaves the operator told so, and a retry is safe.
#[tokio::test]
async fn a_dropped_receipt_says_a_retry_is_safe() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let enclave = Enclave::start("tool-dropped").await;
    let upstream = enclave.address.clone();
    // A relay that forwards everything up to the delivery, then cuts the connection on the way back.
    tokio::spawn(async move {
        let (mut downstream, _) = listener.accept().await.unwrap();
        let mut upstream = TcpStream::connect(upstream).await.unwrap();
        let hello: Hello = receive(&mut downstream).await.unwrap();
        send(&mut upstream, &hello).await.unwrap();
        let offer: Offer = receive(&mut upstream).await.unwrap();
        send(&mut downstream, &offer).await.unwrap();
        let delivery: Delivery = receive(&mut downstream).await.unwrap();
        send(&mut upstream, &delivery).await.unwrap();
        let _reply: Reply = receive(&mut upstream).await.unwrap();
    });
    let release = manifest("tool-dropped", RELEASE, CONFIG);
    let file = bundle_file("tool-dropped", 1, "a");
    let args = DeliverArgs { peer: peer(&address, release.clone()), secrets_file: file.clone(), partial: false };
    let err = format!("{:#}", deliver(&enclave.cfg, &operator_key(), &args).await.unwrap_err());
    assert!(err.contains("a retry with the same version is safe"), "{err}");
    assert_eq!(enclave.store.version(), Some(1), "the enclave sealed it");

    let retry = DeliverArgs { peer: peer(&enclave.address, release), secrets_file: file, partial: false };
    deliver(&enclave.cfg, &operator_key(), &retry).await.unwrap();
    assert_eq!(enclave.store.current().unwrap().bundle, full(1, "a"));
}

#[tokio::test]
async fn the_status_read_reports_what_the_enclave_attests_and_delivers_nothing() {
    let enclave = Enclave::start("tool-status").await;
    enclave.sealed(bundle(9, &[("chain_key", "FAKE-chain")])).await;
    let attested =
        connect(&enclave.cfg, &peer(&enclave.address, manifest("tool-status", RELEASE, CONFIG))).await.unwrap();
    assert_eq!(attested.status.sealed_version, Some(9));
    let names: Vec<String> = attested.status.missing.iter().map(ToString::to_string).collect();
    assert_eq!(names, ["critical_url", "vendor_key"]);
    status(&enclave.cfg, &peer(&enclave.address, manifest("tool-status-2", RELEASE, CONFIG))).await.unwrap();
    assert_eq!(enclave.store.version(), Some(9));
}

// `--allow-mock` answers a mock enclave's quote, which proves nothing; a production build refuses
// it before anything is read or connected. Runs only in the production-feature lane, like the
// node's other production refusals.
#[cfg(feature = "production")]
#[tokio::test]
async fn a_production_build_refuses_allow_mock_before_it_reads_or_connects() {
    let cfg: NodeConfig = toml::from_str(&std::fs::read_to_string(crate::test_support::DEV_CONFIG).unwrap()).unwrap();
    let args =
        PeerArgs { to: "127.0.0.1:1".into(), release: PathBuf::from("release.toml"), root: None, allow_mock: true };
    let err = match connect(&cfg, &args).await {
        Ok(_) => panic!("a production build took --allow-mock"),
        Err(err) => err,
    };
    assert!(format!("{err:#}").contains("--allow-mock is refused in a production build"), "{err:#}");
}
