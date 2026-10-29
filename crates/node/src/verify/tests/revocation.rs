//! Revocation-list tests through the fetched collateral: the root-issued list is enforced against
//! the intermediate it issued, never against the VCEK, whose serial is unique only within its own
//! issuer's scope.

use engine_verify::fixtures::{fixtures, pem, release};
use rcgen::SerialNumber;

use super::super::verify_attestation;
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

/// A certificate's serial content bytes, as a revocation list entry would carry them.
fn serial_of(der: &[u8]) -> Vec<u8> {
    use x509_parser::prelude::FromDer as _;
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(der).unwrap();
    cert.serial.to_bytes_be()
}
