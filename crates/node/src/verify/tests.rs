//! Tests and the dev scenario. The certificate chain, report, and key-distribution service stand-in
//! build a complete quote the verifier accepts or rejects case by case; no hardware, vendor
//! account, or network beyond loopback is involved.

use std::collections::BTreeSet;

use anyhow::Context as _;
use axum::Router;
use axum::extract::{Path, Query};
use axum::routing::get;
use rcgen::{BasicConstraints, Certificate, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::chain::Collateral;
use super::report::{REPORT_LEN, Tcb};
use super::{Options, ReleaseManifest, VerifiedAttestation, ensure_signers_verified, verify_attestation};
use crate::tee::{Attestation, AttestationBundle};

/// A stand-in vendor chain with the real shape: a self-signed root, an intermediate, and the
/// chip key that signs reports, all ECDSA P-384.
struct TestChain {
    ark: Certificate,
    ask: Certificate,
    vcek: Certificate,
    vcek_key: KeyPair,
}

const PROCESSOR: &str = "TestProc";
const POLICY: u64 = 0x30000;
const REPORTED_TCB: Tcb = Tcb { boot_loader: 4, tee: 0, snp: 8, microcode: 205 };

fn test_chain() -> TestChain {
    let ark_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let ark_params = ca_params("ARK-Test");
    let ark = ark_params.self_signed(&ark_key).unwrap();

    let ask_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let ask_params = ca_params("ASK-Test");
    let ask = ask_params.signed_by(&ask_key, &Issuer::from_params(&ark_params, &ark_key)).unwrap();

    let vcek_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let mut vcek_params = CertificateParams::new(Vec::new()).unwrap();
    vcek_params.distinguished_name.push(DnType::CommonName, "VCEK-Test");
    vcek_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    let vcek = vcek_params.signed_by(&vcek_key, &Issuer::from_params(&ask_params, &ask_key)).unwrap();
    TestChain { ark, ask, vcek, vcek_key }
}

fn ca_params(common_name: &str) -> CertificateParams {
    let mut params = CertificateParams::new(Vec::new()).unwrap();
    params.distinguished_name.push(DnType::CommonName, common_name);
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::DigitalSignature];
    params
}

/// A certificate as ASCII PEM, which the key-distribution stand-in serves and `--root` takes; the
/// crate builds without a PEM writer, so the DER wraps here.
fn pem(cert: &Certificate) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(cert.der());
    let body = encoded
        .as_bytes()
        .chunks(64)
        .map(|chunk| std::str::from_utf8(chunk).expect("base64 is ASCII"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("-----BEGIN CERTIFICATE-----\n{body}\n-----END CERTIFICATE-----\n")
}

/// The fixture's enclave identity and configuration: what a real quote would bind.
struct Enclave {
    bundle: AttestationBundle,
    chip_id: [u8; 64],
}

fn enclave() -> Enclave {
    Enclave {
        bundle: AttestationBundle {
            enclave_address: [0x5e; 20],
            config_sha256: [0xc0; 32],
            tls_public_key_sha256: [0x71; 32],
        },
        chip_id: [0x9d; 64],
    }
}

/// A report the test VCEK signs over the fixture's report data and the given release fields.
fn forged_report(chain: &TestChain, measurement: [u8; 48], report_data: [u8; 64], chip_id: [u8; 64]) -> Vec<u8> {
    let mut report = vec![0u8; REPORT_LEN];
    report[0x00..0x04].copy_from_slice(&2u32.to_le_bytes());
    report[0x08..0x10].copy_from_slice(&POLICY.to_le_bytes());
    report[0x34..0x38].copy_from_slice(&1u32.to_le_bytes());
    report[0x38..0x40].copy_from_slice(&REPORTED_TCB.to_packed().to_le_bytes());
    report[0x50..0x90].copy_from_slice(&report_data);
    report[0x90..0xC0].copy_from_slice(&measurement);
    report[0x180..0x188].copy_from_slice(&REPORTED_TCB.to_packed().to_le_bytes());
    report[0x1A0..0x1E0].copy_from_slice(&chip_id);
    let rng = ring::rand::SystemRandom::new();
    let key = ring::signature::EcdsaKeyPair::from_pkcs8(
        &ring::signature::ECDSA_P384_SHA384_FIXED_SIGNING,
        &chain.vcek_key.serialize_der(),
        &rng,
    )
    .unwrap();
    let signature = key.sign(&rng, &report[..0x2A0]).unwrap();
    // `R` and `S` are 72-byte fields; the value sits right-justified in each.
    report[0x2A0 + 24..0x2A0 + 72].copy_from_slice(&signature.as_ref()[..48]);
    report[0x2E8 + 24..0x2E8 + 72].copy_from_slice(&signature.as_ref()[48..]);
    report
}

/// The attestation JSON a node with this quote would serve for `bundle`.
fn attestation(bundle: &AttestationBundle, quote: &[u8]) -> Attestation {
    let report_data = bundle.report_data();
    Attestation {
        provider: "sev_guest".into(),
        enclave_address: format!("0x{}", hex::encode(bundle.enclave_address)),
        config_sha256: format!("0x{}", hex::encode(bundle.config_sha256)),
        tls_public_key_sha256: format!("0x{}", hex::encode(bundle.tls_public_key_sha256)),
        bundle: format!("0x{}", hex::encode(bundle.encode())),
        report_data: format!("0x{}", hex::encode(report_data)),
        quote: format!("0x{}", hex::encode(quote)),
    }
}

const LAUNCH_DIGEST: [u8; 48] = [0x1d; 48];

/// The release manifest the fixture verifies against.
fn release() -> ReleaseManifest {
    toml::from_str(&format!(
        r#"
name = "test-release-v1"
launch_digest = "0x{}"
policy = "0x{:016x}"

[minimum_tcb]
boot_loader = {}
tee = 0
snp = 8
microcode = 115

[config_hashes]
"node.toml" = "0x{}"
"#,
        hex::encode(LAUNCH_DIGEST),
        POLICY,
        REPORTED_TCB.boot_loader,
        hex::encode([0xc0; 32])
    ))
    .unwrap()
}

/// A key-distribution service stand-in that issues the test chain: the VCEK only for the fixture's
/// chip and reported TCB, and the issuer chain only for the fixture's processor.
struct KdsStub {
    base: String,
    _task: JoinHandle<()>,
}

async fn kds_stub(chain: &TestChain, enclave: &Enclave) -> KdsStub {
    let app = Router::new()
        .route("/vcek/v1/{processor}/cert_chain", {
            let ark = pem(&chain.ark);
            let ask = pem(&chain.ask);
            get(move |Path(processor): Path<String>| {
                let ark = ark.clone();
                let ask = ask.clone();
                async move {
                    if processor != PROCESSOR {
                        return (axum::http::StatusCode::NOT_FOUND, String::new());
                    }
                    (axum::http::StatusCode::OK, format!("{ask}{ark}"))
                }
            })
        })
        .route("/vcek/v1/{processor}/{hwid}", {
            let vcek = pem(&chain.vcek);
            let chip = hex::encode(enclave.chip_id);
            get(move |Path((processor, hwid)): Path<(String, String)>, Query(query): Query<Vec<(String, String)>>| {
                let vcek = vcek.clone();
                let chip = chip.clone();
                async move {
                    let tcb = format!(
                        "blSPL={}&teeSPL={}&snpSPL={}&ucodeSPL={}",
                        REPORTED_TCB.boot_loader, REPORTED_TCB.tee, REPORTED_TCB.snp, REPORTED_TCB.microcode
                    );
                    let query: String =
                        query.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
                    if processor != PROCESSOR || hwid != chip || query != tcb {
                        return (axum::http::StatusCode::NOT_FOUND, String::new());
                    }
                    (axum::http::StatusCode::OK, vcek)
                }
            })
        });
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binding the stub");
    let base = format!("http://{}", listener.local_addr().unwrap());
    let _task = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serving the stub");
    });
    KdsStub { base, _task }
}

fn options<'a>(stub: &'a KdsStub, root: &'a str) -> Options<'a> {
    Options { processor: PROCESSOR, kds_base: &stub.base, root_pem: Some(root) }
}

#[tokio::test]
async fn a_release_quote_verifies_and_names_the_release() {
    let chain = test_chain();
    let enclave = enclave();
    let stub = kds_stub(&chain, &enclave).await;
    let attestation = attestation(
        &enclave.bundle,
        &forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id),
    );

    let verified = verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.unwrap();
    assert_eq!(verified.release_name, "test-release-v1");
    assert_eq!(verified.enclave_address.0, [0x5e; 20]);
    assert_eq!(verified.config_name, "node.toml");
}

#[tokio::test]
async fn a_mock_quote_never_verifies() {
    let chain = test_chain();
    let enclave = enclave();
    let stub = kds_stub(&chain, &enclave).await;
    let mut quote = b"MOCK-ATTESTATION-NOT-A-TEE:".to_vec();
    quote.extend_from_slice(&enclave.bundle.report_data());
    let attestation = attestation(&enclave.bundle, &quote);

    assert!(verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());
}

#[tokio::test]
async fn a_chain_not_reaching_a_root_never_verifies() {
    let chain = test_chain();
    let enclave = enclave();
    let stub = kds_stub(&chain, &enclave).await;
    let attestation = attestation(
        &enclave.bundle,
        &forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id),
    );

    // No extra root: only the compiled-in vendor roots pin, and the test chain reaches none of them.
    let options = Options { processor: PROCESSOR, kds_base: &stub.base, root_pem: None };
    assert!(verify_attestation(&attestation, &release(), &options).await.is_err());
}

#[tokio::test]
async fn an_altered_configuration_fails_verification() {
    let chain = test_chain();
    let enclave = enclave();
    let stub = kds_stub(&chain, &enclave).await;
    // The enclave ran a config the release does not list: everything else is consistent.
    let mut bundle = enclave.bundle;
    bundle.config_sha256 = [0xc1; 32];
    let attestation =
        attestation(&bundle, &forged_report(&chain, LAUNCH_DIGEST, bundle.report_data(), enclave.chip_id));

    assert!(verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());
}

#[tokio::test]
async fn an_altered_measurement_policy_or_tcb_fails_verification() {
    let chain = test_chain();
    let enclave = enclave();
    let stub = kds_stub(&chain, &enclave).await;
    for (what, measurement, policy, minimum_tcb) in [
        ("measurement", [0x1e; 48], POLICY, (4, 0, 8, 115)),
        ("policy", LAUNCH_DIGEST, POLICY + 1, (4, 0, 8, 115)),
        ("minimum TCB", LAUNCH_DIGEST, POLICY, (4, 0, 9, 115)),
    ] {
        let mut release = release();
        release.launch_digest = measurement;
        release.policy = policy;
        release.minimum_tcb.snp = minimum_tcb.2;
        let attestation = attestation(
            &enclave.bundle,
            &forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id),
        );

        assert!(
            verify_attestation(&attestation, &release, &options(&stub, &pem(&chain.ark))).await.is_err(),
            "accepted a quote with an altered {what}"
        );
    }
}

#[tokio::test]
async fn a_tampered_bundle_or_report_data_fails_verification() {
    let chain = test_chain();
    let enclave = enclave();
    let stub = kds_stub(&chain, &enclave).await;
    let quote = forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id);
    let base = attestation(&enclave.bundle, &quote);

    // The served bundle no longer hashes to the served report data.
    let mut bundle = enclave.bundle.encode();
    bundle[30] ^= 1;
    let mut tampered = base.clone();
    tampered.bundle = format!("0x{}", hex::encode(bundle));
    assert!(verify_attestation(&tampered, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());

    // The served report data no longer matches the quote's.
    let mut report_data = enclave.bundle.report_data();
    report_data[40] ^= 1;
    let mut tampered = base.clone();
    tampered.report_data = format!("0x{}", hex::encode(report_data));
    assert!(verify_attestation(&tampered, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());

    // A served field disagrees with the bundle the report data binds.
    let mut tampered = base;
    tampered.config_sha256 = format!("0x{}", hex::encode([0xc3; 32]));
    assert!(verify_attestation(&tampered, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());
}

#[tokio::test]
async fn the_vcek_must_match_the_quotes_chip_and_tcb() {
    let chain = test_chain();
    let enclave = enclave();
    let stub = kds_stub(&chain, &enclave).await;
    // A different chip id: the stand-in refuses the VCEK issue, as the vendor's service would.
    let mut other_chip = enclave.chip_id;
    other_chip[0] ^= 1;
    let attestation =
        attestation(&enclave.bundle, &forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), other_chip));

    assert!(verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());
}

#[test]
fn signers_pass_only_when_they_match_the_verified_enclave_address() {
    let enclave = enclave();
    let verified = VerifiedAttestation {
        release_name: "test-release-v1".into(),
        enclave_address: engine_core::types::AccountId(enclave.bundle.enclave_address),
        config_name: "node.toml".into(),
    };
    let mut signers = BTreeSet::new();
    signers.insert(engine_core::types::AccountId(enclave.bundle.enclave_address));
    ensure_signers_verified(&signers, &verified).expect("the attested signer passes");

    signers.insert(engine_core::types::AccountId([0x42; 20]));
    assert!(ensure_signers_verified(&signers, &verified).is_err(), "an unattested signer passed");
}

/// The dev scenario: a release quote passes verification and names the release; a mock quote, an
/// unlisted configuration, an altered measurement, and an unattested checkpoint signer fail. Runs
/// without hardware or vendor access; `scripts/dev-attestation-verifier.sh` runs it, and CI runs
/// the script.
#[tokio::test]
#[ignore = "dev scenario, run by scripts/dev-attestation-verifier.sh"]
async fn dev_scenario_attestation_verifier() -> anyhow::Result<()> {
    let chain = test_chain();
    let mut other_enclave = enclave();
    other_enclave.bundle.config_sha256 = [0xc1; 32];
    let enclave = enclave();
    let stub = kds_stub(&chain, &enclave).await;
    let ark_pem = pem(&chain.ark);
    let opts = options(&stub, &ark_pem);
    let release = release();

    // The release's quote verifies and names the release, the enclave address, and the config.
    let verified = verify_attestation(
        &attestation(
            &enclave.bundle,
            &forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id),
        ),
        &release,
        &opts,
    )
    .await
    .context("the release quote must verify")?;
    assert_eq!(verified.release_name, "test-release-v1");
    assert_eq!(verified.enclave_address.0, [0x5e; 20]);
    assert_eq!(verified.config_name, "node.toml");
    println!(
        "verified: release {}, enclave {}, config {}",
        verified.release_name, verified.enclave_address, verified.config_name
    );
    let mut signers = BTreeSet::new();
    signers.insert(verified.enclave_address);
    ensure_signers_verified(&signers, &verified).context("the attested signer must pass")?;

    // A mock quote fails, and so does a quote for a configuration the release does not list.
    let mut mock_quote = b"MOCK-ATTESTATION-NOT-A-TEE:".to_vec();
    mock_quote.extend_from_slice(&enclave.bundle.report_data());
    assert!(verify_attestation(&attestation(&enclave.bundle, &mock_quote), &release, &opts).await.is_err());
    assert!(
        verify_attestation(
            &attestation(
                &other_enclave.bundle,
                &forged_report(&chain, LAUNCH_DIGEST, other_enclave.bundle.report_data(), other_enclave.chip_id),
            ),
            &release,
            &opts,
        )
        .await
        .is_err()
    );

    // An altered measurement fails.
    assert!(
        verify_attestation(
            &attestation(
                &enclave.bundle,
                &forged_report(&chain, [0x1e; 48], enclave.bundle.report_data(), enclave.chip_id)
            ),
            &release,
            &opts,
        )
        .await
        .is_err()
    );

    // A checkpoint signer the attestation does not cover fails.
    let mut unattested = BTreeSet::new();
    unattested.insert(engine_core::types::AccountId([0x42; 20]));
    assert!(ensure_signers_verified(&unattested, &verified).is_err());

    println!("attestation verifier dev scenario passed");
    Ok(())
}

/// Collateral fetches retry through transient failures and stop at a persistent one.
#[tokio::test]
async fn collateral_fetches_have_a_bounded_retry() {
    // Nothing listens on this address: every attempt fails, and the error names the fetch.
    let collateral = Collateral::new("http://127.0.0.1:1", PROCESSOR).unwrap();
    let err = collateral.issuer_chain().await.expect_err("an unavailable service must fail");
    assert!(err.to_string().contains("fetching") || err.to_string().contains("answered"), "{err:#}");
}
