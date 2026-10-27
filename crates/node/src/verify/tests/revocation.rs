//! Revocation-list tests: the root-issued list is enforced against the intermediate it issued,
//! never against the VCEK, whose serial is unique only within its own issuer's scope.

use rcgen::{KeyPair, SerialNumber};

use super::super::chain::{QuoteChain, Roots, check_revocation};
use super::super::verify_attestation;
use super::fixtures::{ca_params, crl_revoking, crl_signed, fixtures, pem, release};
use super::stub::{kds_stub_with, options};

#[tokio::test]
async fn a_revoked_intermediate_fails_verification() {
    let (chain, _enclave, attestation) = fixtures();
    let ask_serial = SerialNumber::from_slice(&serial_of(chain.ask.der()));
    let stub = kds_stub_with(&chain, false, std::slice::from_ref(&ask_serial)).await;

    assert!(verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.is_err());
}

#[tokio::test]
async fn a_root_list_naming_the_vcek_serial_verifies() {
    let (chain, _enclave, attestation) = fixtures();
    // The root revokes an old intermediate that happened to share the VCEK's serial; certificate
    // serials are unique only within an issuing CA, so the root-issued list cannot name the VCEK.
    // The CRL entry carries the serial with DER sign padding, as a vendor's list would.
    let padded = SerialNumber::from_slice(&[0x00, 0x8A, 0x11, 0xCE]);
    let stub = kds_stub_with(&chain, false, std::slice::from_ref(&padded)).await;

    assert!(verify_attestation(&attestation, &release(), &options(&stub, &pem(&chain.ark))).await.is_ok());
}

#[test]
fn the_revocation_list_must_be_fresh_issued_by_the_root_and_signed_by_it() {
    let (chain, _enclave, _attestation) = fixtures();
    let mut roots = Roots::vendor();
    roots.add_pem(&pem(&chain.ark)).unwrap();
    let quote =
        QuoteChain::verify(chain.vcek.der().to_vec(), chain.ask.der().to_vec(), chain.ark.der().to_vec(), &roots)
            .unwrap();
    let crl_der = |crl: String| {
        let mut blocks = x509_parser::pem::Pem::iter_from_buffer(crl.as_bytes());
        blocks.next().unwrap().unwrap().contents
    };

    // An expired list and a not-yet-valid list are both refused.
    let expired = crl_signed(&chain, &[], (2024, 1, 1), (2025, 1, 1), None);
    assert!(check_revocation(&crl_der(expired), &quote).is_err());
    let early = crl_signed(&chain, &[], (2030, 1, 1), (2049, 1, 1), None);
    assert!(check_revocation(&crl_der(early), &quote).is_err());

    // A list signed by a key that is not the root's is refused, as is one issued by another name.
    let other = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let wrong_key = crl_signed(&chain, &[], (2024, 1, 1), (2049, 1, 1), Some(&other));
    assert!(check_revocation(&crl_der(wrong_key), &quote).is_err());
    let other_params = ca_params("Other-Root");
    let foreign = rcgen::CertificateRevocationListParams {
        this_update: rcgen::date_time_ymd(2024, 1, 1),
        next_update: rcgen::date_time_ymd(2049, 1, 1),
        crl_number: SerialNumber::from(1),
        issuing_distribution_point: None,
        revoked_certs: Vec::new(),
        key_identifier_method: other_params.key_identifier_method.clone(),
    }
    .signed_by(&rcgen::Issuer::from_params(&other_params, &other))
    .unwrap();
    assert!(check_revocation(foreign.der().as_ref(), &quote).is_err());
}

#[test]
fn the_root_list_scopes_revoked_serials_to_the_intermediate() {
    let (chain, _enclave, _attestation) = fixtures();
    let mut roots = Roots::vendor();
    roots.add_pem(&pem(&chain.ark)).unwrap();
    let quote =
        QuoteChain::verify(chain.vcek.der().to_vec(), chain.ask.der().to_vec(), chain.ark.der().to_vec(), &roots)
            .unwrap();
    let crl_der = |crl: String| {
        let mut blocks = x509_parser::pem::Pem::iter_from_buffer(crl.as_bytes());
        blocks.next().unwrap().unwrap().contents
    };

    // A list naming the intermediate — the certificate the root issued — is enforced.
    let ask_serial = SerialNumber::from_slice(&serial_of(chain.ask.der()));
    let revoked_ask = crl_revoking(&chain, std::slice::from_ref(&ask_serial));
    assert!(check_revocation(&crl_der(revoked_ask), &quote).is_err());

    // The same list under the VCEK's serial is not: the root never issued the VCEK, so the
    // entry can only be the old intermediate's serial, not this certificate's.
    let vcek_serial = SerialNumber::from_slice(&serial_of(chain.vcek.der()));
    let revoked_vcek = crl_revoking(&chain, std::slice::from_ref(&vcek_serial));
    assert!(check_revocation(&crl_der(revoked_vcek), &quote).is_ok());
}

/// A certificate's serial content bytes, as a revocation list entry would carry them.
fn serial_of(der: &[u8]) -> Vec<u8> {
    use x509_parser::prelude::FromDer as _;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der).unwrap();
    cert.serial.to_bytes_be()
}
