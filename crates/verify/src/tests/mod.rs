//! Verification with no IO: the fixture chain and report build complete quotes, the collateral is
//! passed in as a node would relay it, and every date is checked at the `now` each test chooses.

mod chain;
mod releases;
mod signature;

use crate::fixtures::*;
use crate::{Attestation, RelayedCollateral, ReleaseManifest, VerifiedAttestation, verify_attestation};

#[test]
fn a_release_quote_verifies_names_the_release_and_carries_its_validity_window() {
    let (chain, enclave, attestation) = fixtures();
    let verified = verify(&attestation, &[release()], &collateral_of(&chain, &crl_revoking(&chain, &[])), now())
        .expect("the release quote verifies");

    assert_eq!(verified.release_name, "test-release-v1");
    assert_eq!(verified.enclave_address, [0x5e; 20]);
    assert_eq!(verified.config_name, "node.toml");
    assert_eq!(verified.tls_public_key_sha256, enclave.bundle.tls_public_key_sha256);
    // The list runs 2024-01-01 to 2049-01-01 and the certificates 1975-01-01 to 4096-01-01
    // (rcgen's default): the window is where both are valid.
    assert_eq!(verified.validity.from, ymd(2024, 1, 1));
    assert_eq!(verified.validity.until, ymd(2049, 1, 1));
}

#[test]
fn the_verdict_holds_exactly_inside_its_validity_window() {
    let (chain, _enclave, attestation) = fixtures();
    // The list starts after the certificates were issued, so the list bounds both ends.
    let crl = crl_signed(&chain, &[], (2027, 6, 1), (2027, 6, 2), None);
    let collateral = collateral_of(&chain, &crl);
    let window = verify(&attestation, &[release()], &collateral, ymd(2027, 6, 1) + 3600).unwrap().validity;
    assert_eq!((window.from, window.until), (ymd(2027, 6, 1), ymd(2027, 6, 2)));

    for (at, verifies) in
        [(window.from - 1, false), (window.from, true), (window.until, true), (window.until + 1, false)]
    {
        let result = verify(&attestation, &[release()], &collateral, at);
        assert_eq!(result.is_ok(), verifies, "at {at}: {:?}", result.err());
    }
}

#[test]
fn a_certificate_that_ends_before_the_list_ends_the_window() {
    let enclave = enclave();
    let chain = test_chain_with(enclave.chip_id, REPORTED_TCB, Variants { expired_vcek: true, ..Variants::default() });
    let attestation = attestation(
        &enclave.bundle,
        &forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id),
    );
    // The VCEK ends 2020-01-01, inside the list's 2019 to 2049.
    let collateral = collateral_of(&chain, &crl_signed(&chain, &[], (2019, 1, 1), (2049, 1, 1), None));
    let window = verify(&attestation, &[release()], &collateral, ymd(2019, 6, 1)).unwrap().validity;
    assert_eq!((window.from, window.until), (ymd(2019, 1, 1), ymd(2020, 1, 1)));
    let err = verify(&attestation, &[release()], &collateral, window.until + 1).unwrap_err();
    assert!(err.to_string().contains("outside its validity period"), "{err:#}");
}

#[test]
fn a_time_outside_the_certificate_range_is_refused() {
    let (chain, _enclave, attestation) = fixtures();
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let err = verify(&attestation, &[release()], &collateral, i64::MAX).unwrap_err();
    assert_eq!(err.to_string(), format!("the time {} is out of range", i64::MAX));
}

#[test]
fn a_mock_quote_never_verifies_and_reads_only_on_the_mock_path() {
    let (chain, enclave, real) = fixtures();
    let mut quote = crate::MOCK_QUOTE_MARKER.to_vec();
    quote.extend_from_slice(&enclave.bundle.report_data());
    let mut mock = attestation(&enclave.bundle, &quote);
    mock.provider = "mock".into();

    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let err = verify(&mock, &[release()], &collateral, now()).unwrap_err();
    assert_eq!(err.to_string(), "the quote is not an SEV-SNP attestation report; a mock quote never verifies");

    let read = crate::verify_mock_attestation(&mock).expect("a mock quote of the served bundle reads");
    assert_eq!(read.encode(), enclave.bundle.encode());
    // A hardware quote is not a mock one, and neither is a mock quote of other report data.
    assert!(crate::verify_mock_attestation(&real).is_err());
    let mut other = crate::MOCK_QUOTE_MARKER.to_vec();
    other.extend_from_slice(&[0u8; 64]);
    let err = crate::verify_mock_attestation(&attestation(&enclave.bundle, &other)).err().expect("other report data");
    assert_eq!(err.to_string(), "the quote is not a mock quote for the report data");
}

#[test]
fn a_corrupted_report_signature_fails_with_the_verifiers_message() {
    let (chain, enclave, _) = fixtures();
    let mut report = forged_report(&chain, LAUNCH_DIGEST, enclave.bundle.report_data(), enclave.chip_id);
    report[0x2A0] ^= 1;
    let collateral = collateral_of(&chain, &crl_revoking(&chain, &[]));
    let err = verify(&attestation(&enclave.bundle, &report), &[release()], &collateral, now()).unwrap_err();
    assert_eq!(format!("{err:#}"), "report signature does not verify against the VCEK: signature error");
}

#[test]
fn a_revoked_intermediate_or_a_foreign_vcek_fails_verification() {
    let (chain, enclave, attestation) = fixtures();
    let ask_serial = rcgen::SerialNumber::from_slice(&serial_of(chain.ask.der()));
    let revoked = collateral_of(&chain, &crl_revoking(&chain, &[ask_serial]));
    let err = verify(&attestation, &[release()], &revoked, now()).unwrap_err();
    assert!(err.to_string().contains("is revoked"), "{err:#}");

    let mut other_chip = enclave.chip_id;
    other_chip[0] ^= 1;
    let foreign = RelayedCollateral {
        vcek: vcek_for(&chain, other_chip, REPORTED_TCB).der().to_vec(),
        ..collateral_of(&chain, &crl_revoking(&chain, &[]))
    };
    let err = verify(&attestation, &[release()], &foreign, now()).unwrap_err();
    assert_eq!(err.to_string(), "VCEK hardware id does not match the quote's chip id");
}

// ── Helpers ──

/// Verifies against the fixture chain's root.
fn verify(
    attestation: &Attestation,
    releases: &[ReleaseManifest],
    collateral: &RelayedCollateral,
    now: i64,
) -> anyhow::Result<VerifiedAttestation> {
    verify_attestation(attestation, releases, collateral, Some(&pem_wrap(&collateral.root, "CERTIFICATE")), now)
}

/// A fixed time inside every fixture's window: 2027-01-01.
fn now() -> i64 {
    ymd(2027, 1, 1)
}

/// Unix seconds at midnight UTC on the date.
fn ymd(year: i32, month: u8, day: u8) -> i64 {
    rcgen::date_time_ymd(year, month, day).unix_timestamp()
}

/// A certificate's serial content bytes, as a revocation list entry would carry them.
fn serial_of(der: &[u8]) -> Vec<u8> {
    use x509_parser::prelude::FromDer as _;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der).unwrap();
    cert.serial.to_bytes_be()
}
