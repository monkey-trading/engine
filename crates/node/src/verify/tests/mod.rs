//! Tests and the dev scenario. The certificate chain, report, and key-distribution service
//! stand-in build a complete quote the verifier accepts or rejects case by case; no hardware,
//! vendor account, or network beyond loopback is involved. The stand-in chain follows the
//! vendor's shapes: the VCEK endpoint answers DER for any chip and TCB (so the verifier itself
//! must bind the certificate's signed extensions to the quote), and the VCEK carries the
//! vendor's extension OIDs for its chip id and TCB.

mod fixtures;
mod stub;

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use rcgen::{CertificateParams, Issuer, KeyPair};

use super::chain::Collateral;
use super::report::Tcb;
use super::{Options, VerifiedAttestation, ensure_signers_verified, verify_attestation, verify_attestation_files};
use crate::tee::{Attestation, AttestationBundle};
use fixtures::*;
use stub::{flaky_stub, kds_stub, options};

#[tokio::test]
async fn a_release_quote_verifies_and_names_the_release() {
    let (chain, _enclave, attestation) = fixtures();
    let stub = kds_stub(&chain).await;

    let verified = verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.unwrap();
    assert_eq!(verified.release_name, "test-release-v1");
    assert_eq!(verified.enclave_address.0, [0x5e; 20]);
    assert_eq!(verified.config_name, "node.toml");
}

#[tokio::test]
async fn a_mock_quote_never_verifies() {
    let (chain, enclave, _) = fixtures();
    let stub = kds_stub(&chain).await;
    let mut quote = b"MOCK-ATTESTATION-NOT-A-TEE:".to_vec();
    quote.extend_from_slice(&enclave.bundle.report_data());
    let attestation = attestation(&enclave.bundle, &quote);

    assert!(verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());
}

#[tokio::test]
async fn a_chain_not_reaching_a_root_never_verifies() {
    let (chain, _enclave, attestation) = fixtures();
    let stub = kds_stub(&chain).await;

    // No extra root: only the compiled-in vendor roots pin, and the test chain reaches none of them.
    let options = Options { processor: PROCESSOR, kds_base: &stub.base, root_pem: None };
    assert!(verify_attestation(&attestation, &release(), &options).await.is_err());
}

#[tokio::test]
async fn a_chain_with_a_corrupted_certificate_signature_never_verifies() {
    let enclave = enclave();
    // The VCEK names the ASK as its issuer but is signed with the ARK's key, so issuer names
    // match and only the signature check can refuse it.
    let ark_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let ark_params = ca_params("ARK-Test");
    let ark = ark_params.self_signed(&ark_key).unwrap();
    let ask_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let ask_params = ca_params("ASK-Test");
    let ask = ask_params.signed_by(&ask_key, &Issuer::from_params(&ark_params, &ark_key)).unwrap();
    let vcek_key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let vcek_params = CertificateParams::new(Vec::new()).unwrap();
    let vcek = vcek_params.signed_by(&vcek_key, &Issuer::from_params(&ask_params, &ark_key)).unwrap();
    let chain = TestChain { ark, ask, vcek, vcek_key };

    let stub = kds_stub(&chain).await;
    let attestation = attestation(
        &enclave.bundle,
        &forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id),
    );
    assert!(verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());
}

#[tokio::test]
async fn an_altered_configuration_fails_verification() {
    let (chain, enclave, _) = fixtures();
    let stub = kds_stub(&chain).await;
    // The enclave ran a config the release does not list: everything else is consistent.
    let mut bundle = enclave.bundle;
    bundle.config_sha256 = [0xc1; 32];
    let attestation =
        attestation(&bundle, &forged_report(&chain, LAUNCH_DIGEST, bundle.report_data(), enclave.chip_id));

    assert!(verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());
}

#[tokio::test]
async fn an_altered_measurement_policy_or_tcb_fails_verification() {
    let (chain, _enclave, attestation) = fixtures();
    let stub = kds_stub(&chain).await;
    for (what, measurement, policy, minimum_tcb) in [
        ("measurement", [0x1e; 48], POLICY, (3, 0, 8, 115)),
        ("policy", LAUNCH_DIGEST, POLICY + 1, (3, 0, 8, 115)),
        ("minimum TCB", LAUNCH_DIGEST, POLICY, (5, 0, 8, 115)),
        ("minimum TCB with a higher earlier component", LAUNCH_DIGEST, POLICY, (3, 0, 9, 215)),
    ] {
        let mut release = release();
        release.launch_digest = measurement;
        release.policy = policy;
        release.minimum_tcb.boot_loader = minimum_tcb.0;
        release.minimum_tcb.snp = minimum_tcb.2;
        release.minimum_tcb.microcode = minimum_tcb.3;

        assert!(
            verify_attestation(&attestation, &release, &options(&stub, &pem(&chain.ark))).await.is_err(),
            "accepted a quote with an altered {what}"
        );
    }
}

#[tokio::test]
async fn a_tampered_bundle_hash_fails_verification() {
    let (chain, enclave, _) = fixtures();
    let stub = kds_stub(&chain).await;
    // Every served field stays consistent with the tampered bundle, and the quote binds the
    // served report data; only the tampered bundle's hash differing from it can refuse this.
    let mut bundle = enclave.bundle.encode();
    bundle[80] ^= 1;
    let decoded = AttestationBundle::decode(&bundle).unwrap();
    let served_report_data = attestation(&enclave.bundle, &[]).report_data;
    let quote = forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id);
    let tampered = Attestation {
        bundle: format!("0x{}", hex::encode(&bundle)),
        enclave_address: format!("0x{}", hex::encode(decoded.enclave_address)),
        config_sha256: format!("0x{}", hex::encode(decoded.config_sha256)),
        tls_public_key_sha256: format!("0x{}", hex::encode(decoded.tls_public_key_sha256)),
        report_data: served_report_data,
        quote: format!("0x{}", hex::encode(quote)),
        provider: "sev_guest".into(),
    };

    assert!(verify_attestation(&tampered, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());
}

#[tokio::test]
async fn a_quote_whose_report_data_differs_from_the_served_one_fails_verification() {
    let (chain, enclave, _) = fixtures();
    let stub = kds_stub(&chain).await;
    // The served bundle and its hash are consistent; only the signed quote binds other data.
    let other_bundle = AttestationBundle { config_sha256: [0xc1; 32], ..enclave.bundle };
    let quote = forged_report(&chain, LAUNCH_DIGEST, other_bundle.report_data(), enclave.chip_id);
    let attestation = attestation(&enclave.bundle, &quote);

    assert!(verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());
}

#[tokio::test]
async fn a_corrupted_report_signature_never_verifies() {
    let (chain, enclave, _) = fixtures();
    let stub = kds_stub(&chain).await;
    // Flip a byte the signature covers, after signing; nothing else is inconsistent yet.
    let mut report = forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id);
    report[0x50] ^= 1;
    let attestation = attestation(&enclave.bundle, &report);

    assert!(verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());
}

#[tokio::test]
async fn a_vcek_for_another_chip_fails_verification() {
    let enclave = enclave();
    // A mirror answers any request: the certificate's signed hardware id, not the stub's
    // willingness, must refuse a VCEK for a different chip.
    let mut other_chip = [0x9d; 64];
    other_chip[0] ^= 1;
    let chain = test_chain(other_chip, REPORTED_TCB);
    let stub = kds_stub(&chain).await;
    let attestation = attestation(
        &enclave.bundle,
        &forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id),
    );

    assert!(verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());
}

#[tokio::test]
async fn a_vcek_for_another_tcb_fails_verification() {
    let enclave = enclave();
    let chain =
        test_chain(enclave.chip_id, Tcb { boot_loader: REPORTED_TCB.boot_loader, tee: 0, snp: 8, microcode: 204 });
    let stub = kds_stub(&chain).await;
    let attestation = attestation(
        &enclave.bundle,
        &forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id),
    );

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

#[tokio::test]
async fn collateral_fetches_retry_transient_failures_and_stop_at_a_persistent_one() {
    // Fail twice, then answer with a valid issuer chain: the fetch succeeds on the third attempt.
    let (chain, _enclave, _attestation) = fixtures();
    let pem_chain = format!("{}{}", pem(&chain.ask), pem(&chain.ark));
    let recovering = flaky_stub(2, pem_chain.clone()).await;
    let collateral = Collateral::new(&recovering.base, PROCESSOR).unwrap();
    collateral.issuer_chain().await.expect("the third attempt succeeds");
    assert_eq!(recovering.attempts.load(Ordering::SeqCst), 3);

    // Fail always: the fetch gives up after exactly the attempt limit.
    let failing = flaky_stub(usize::MAX, pem_chain).await;
    let collateral = Collateral::new(&failing.base, PROCESSOR).unwrap();
    collateral.issuer_chain().await.expect_err("a persistent failure must fail");
    assert_eq!(failing.attempts.load(Ordering::SeqCst), 3);
}

/// The dev scenario: a release quote passes verification and names the release; a mock quote, an
/// unlisted configuration, an altered measurement, and an unattested checkpoint signer fail. Runs
/// without hardware or vendor access; `scripts/dev-attestation-verifier.sh` runs it, and CI runs
/// the script. The scenario loads its attestation and manifest from files, the same path the CLI
/// takes.
#[tokio::test]
#[ignore = "dev scenario, run by scripts/dev-attestation-verifier.sh"]
async fn dev_scenario_attestation_verifier() -> anyhow::Result<()> {
    let mut other_enclave = enclave();
    other_enclave.bundle.config_sha256 = [0xc1; 32];
    let (chain, fixture, fixture_attestation) = fixtures();
    let stub = kds_stub(&chain).await;
    let ark_pem = pem(&chain.ark);
    let opts = options(&stub, &ark_pem);

    let dir = std::env::temp_dir().join(format!("attestation-verifier-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let attestation_path = dir.join("attestation.json");
    let release_path = dir.join("release.toml");
    std::fs::write(&attestation_path, serde_json::to_string(&fixture_attestation)?)?;
    std::fs::write(&release_path, release_toml())?;

    // The release's quote verifies and names the release, the enclave address, and the config.
    let verified = verify_attestation_files(&attestation_path, &release_path, &opts)
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
    mock_quote.extend_from_slice(&fixture.bundle.report_data());
    assert!(verify_attestation(&attestation(&fixture.bundle, &mock_quote), &release(), &opts).await.is_err());
    assert!(
        verify_attestation(
            &attestation(
                &other_enclave.bundle,
                &forged_report(&chain, LAUNCH_DIGEST, other_enclave.bundle.report_data(), other_enclave.chip_id),
            ),
            &release(),
            &opts,
        )
        .await
        .is_err()
    );

    // An altered measurement fails.
    assert!(
        verify_attestation(
            &attestation(
                &fixture.bundle,
                &forged_report(&chain, [0x1e; 48], fixture.bundle.report_data(), fixture.chip_id)
            ),
            &release(),
            &opts,
        )
        .await
        .is_err()
    );

    // A checkpoint signer the attestation does not cover fails.
    let mut unattested = BTreeSet::new();
    unattested.insert(engine_core::types::AccountId([0x42; 20]));
    assert!(ensure_signers_verified(&unattested, &verified).is_err());

    std::fs::remove_dir_all(&dir).ok();
    println!("attestation verifier dev scenario passed");
    Ok(())
}
