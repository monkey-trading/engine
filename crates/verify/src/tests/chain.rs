//! The chain's signatures, its revocation list and its names: the vendor's own RSASSA-PSS
//! certificates verify, only the two accepted algorithms are tried, the root-issued list is enforced
//! against the intermediate it issued, never against the VCEK, whose serial is unique only within its
//! own issuer's scope, and the VCEK's product name is one printable IA5String of the chain's line.

use rcgen::{KeyPair, SerialNumber};

use super::{now, serial_of, ymd};
use crate::chain::{QuoteChain, Roots, check_revocation, check_signed_by};
use crate::fixtures::{
    REPORTED_TCB, TestChain, VCEK_SERIAL, ca_params, crl_revoking, crl_signed, der_ia5, fixtures, pem, vcek_extensions,
};

#[test]
fn the_pinned_vendor_intermediates_verify_against_their_roots() {
    // The vendor's real ASK certificates are RSA-4096 and sign with RSASSA-PSS/SHA-384, the
    // path the fixture chain cannot build; this is that path with the vendor's own keys.
    for (ask, root) in [
        (include_str!("ask-milan.pem"), include_str!("../roots/ark-milan.pem")),
        (include_str!("ask-genoa.pem"), include_str!("../roots/ark-genoa.pem")),
        (include_str!("ask-turin.pem"), include_str!("../roots/ark-turin.pem")),
    ] {
        let ask = der(ask);
        let root = der(root);
        check_signed_by(&ask, &root).expect("the vendor intermediate verifies against its root");
        check_signed_by(&root, &ask).expect_err("the root does not verify against the intermediate");
    }
}

#[test]
fn a_vendor_intermediate_with_an_altered_byte_fails_its_signature() {
    let root = der(include_str!("../roots/ark-milan.pem"));
    let ask = der(include_str!("ask-milan.pem"));
    // The last byte is the RSASSA-PSS signature's; the middle of the intermediate's own public key
    // is in the signed part, where the issuer name stays as it was, so only the signature refuses.
    let spki = {
        use x509_parser::prelude::FromDer as _;
        let (_, cert) = x509_parser::certificate::X509Certificate::from_der(&ask).unwrap();
        cert.public_key().raw.to_vec()
    };
    let key_start = ask.windows(spki.len()).position(|window| window == spki).unwrap();
    for index in [ask.len() - 1, key_start + spki.len() / 2] {
        let mut altered = ask.clone();
        altered[index] ^= 1;
        let err = check_signed_by(&altered, &root).unwrap_err();
        assert!(err.to_string().ends_with("SignatureVerificationError"), "byte {index}: {err:#}");
    }
}

#[test]
fn a_certificate_signed_with_another_algorithm_is_refused_as_unsupported() {
    // ECDSA on P-256 with SHA-256 is neither accepted algorithm, however valid the signature.
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    let params = ca_params("P256-Root");
    let root = params.self_signed(&key).unwrap();
    let err = check_signed_by(root.der(), root.der()).unwrap_err();
    assert!(err.to_string().ends_with("SignatureUnsupportedAlgorithm"), "{err:#}");
}

#[test]
fn the_revocation_list_must_be_fresh_issued_by_the_root_and_signed_by_it() {
    let (chain, _enclave, _attestation) = fixtures();
    let quote = quote_chain(&chain);

    // An expired list and a not-yet-valid list are both refused.
    let expired = crl_signed(&chain, &[], (2024, 1, 1), (2025, 1, 1), None);
    assert!(check_revocation(&crl_der(expired), &quote, at(now())).is_err());
    let early = crl_signed(&chain, &[], (2030, 1, 1), (2049, 1, 1), None);
    assert!(check_revocation(&crl_der(early), &quote, at(now())).is_err());

    // A list signed by a key that is not the root's is refused.
    let other = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    let wrong_key = crl_signed(&chain, &[], (2024, 1, 1), (2049, 1, 1), Some(&other));
    assert!(check_revocation(&crl_der(wrong_key), &quote, at(now())).is_err());
    // A list issued under another name but signed with the root's key is refused too, so only
    // the issuer-name check can reject it, not the signature check.
    let other_params = ca_params("Other-Root");
    let foreign = rcgen::CertificateRevocationListParams {
        this_update: rcgen::date_time_ymd(2024, 1, 1),
        next_update: rcgen::date_time_ymd(2049, 1, 1),
        crl_number: SerialNumber::from(1),
        issuing_distribution_point: None,
        revoked_certs: Vec::new(),
        key_identifier_method: other_params.key_identifier_method.clone(),
    }
    .signed_by(&rcgen::Issuer::from_params(&other_params, &chain.ark_key))
    .unwrap();
    assert!(check_revocation(foreign.der().as_ref(), &quote, at(now())).is_err());

    // A current list from the root passes and reports its span.
    let current = check_revocation(&crl_der(crl_revoking(&chain, &[])), &quote, at(now())).unwrap();
    assert_eq!((current.from, current.until), (ymd(2024, 1, 1), ymd(2049, 1, 1)));
}

#[test]
fn the_root_list_scopes_revoked_serials_to_the_intermediate() {
    let (chain, _enclave, _attestation) = fixtures();
    let quote = quote_chain(&chain);

    // A list naming the intermediate — the certificate the root issued — is enforced.
    let ask_serial = SerialNumber::from_slice(&serial_of(chain.ask.der()));
    let revoked_ask = crl_revoking(&chain, std::slice::from_ref(&ask_serial));
    assert!(check_revocation(&crl_der(revoked_ask), &quote, at(now())).is_err());

    // The same list under the VCEK's serial is not: the root never issued the VCEK, so the
    // entry can only be the old intermediate's serial, not this certificate's. The entry carries
    // the serial with DER sign padding, as a vendor's list would.
    let vcek_serial = SerialNumber::from_slice(&serial_of(chain.vcek.der()));
    let padded = SerialNumber::from_slice(&[0x00, 0x8A, 0x11, 0xCE]);
    for serial in [vcek_serial, padded] {
        let revoked_vcek = crl_revoking(&chain, std::slice::from_ref(&serial));
        assert!(check_revocation(&crl_der(revoked_vcek), &quote, at(now())).is_ok());
    }
}

#[test]
fn a_vcek_product_name_must_be_one_printable_ia5_string_of_its_chains_line() {
    let (chain, enclave, _attestation) = fixtures();
    let name = |content: Option<Vec<u8>>| {
        let vcek = vcek_with_product_name(&chain, enclave.chip_id, content);
        let linked = QuoteChain::link(vcek, chain.ask.der().to_vec(), chain.ark.der().to_vec(), at(now())).unwrap();
        linked.product_name().map_err(|err| err.to_string())
    };
    assert_eq!(name(Some(der_ia5("Genoa-A2"))), Ok("Genoa-A2".to_owned()));
    let mut long_form = vec![0x16, 0x81, 8];
    long_form.extend_from_slice(b"Genoa-A2");
    for (content, refusal) in [
        (None, "VCEK carries no extension 1.3.6.1.4.1.3704.1.2"),
        (
            Some(vec![0x0C, 8, b'G', b'e', b'n', b'o', b'a', b'-', b'A', b'2']),
            "VCEK product name extension is not a DER IA5String",
        ),
        (Some(vec![0x16]), "VCEK product name extension is truncated"),
        (
            Some(vec![0x16, 9, b'G', b'e', b'n', b'o', b'a', b'-', b'A', b'2']),
            "VCEK product name extension has a malformed length",
        ),
        (
            Some(vec![0x16, 7, b'G', b'e', b'n', b'o', b'a', b'-', b'A', b'2']),
            "VCEK product name extension has a malformed length",
        ),
        (Some(long_form), "VCEK product name extension has a malformed length"),
        (Some(der_ia5("Genoa A2")), "VCEK product name is not printable ASCII"),
        (Some(der_ia5("Genoa-A2\0")), "VCEK product name is not printable ASCII"),
        (
            Some(der_ia5("Milan-B0")),
            "the VCEK names product Milan-B0, but its chain's root is ARK-Genoa, not ARK-Milan",
        ),
    ] {
        assert_eq!(name(content), Err(refusal.to_owned()));
    }
}

// ── Helpers ──

/// A VCEK `chain`'s intermediate issues for `chip_id` whose product name extension holds `content`
/// as signed, or that carries none.
fn vcek_with_product_name(chain: &TestChain, chip_id: [u8; 64], content: Option<Vec<u8>>) -> Vec<u8> {
    let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
    params.serial_number = Some(SerialNumber::from(VCEK_SERIAL));
    params.distinguished_name.push(rcgen::DnType::CommonName, "VCEK-Test");
    params.custom_extensions = vcek_extensions(chip_id, REPORTED_TCB, 0, false);
    match content {
        Some(content) => {
            params.custom_extensions[1] =
                rcgen::CustomExtension::from_oid_content(&[1, 3, 6, 1, 4, 1, 3704, 1, 2], content)
        }
        None => drop(params.custom_extensions.remove(1)),
    }
    let key = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
    params.signed_by(&key, &rcgen::Issuer::from_params(&chain.ask_params, &chain.ask_key)).unwrap().der().to_vec()
}

/// The fixture chain, pinned to its own root.
fn quote_chain(chain: &crate::fixtures::TestChain) -> QuoteChain {
    let mut roots = Roots::vendor();
    roots.add_pem(&pem(&chain.ark)).unwrap();
    QuoteChain::verify(chain.vcek.der().to_vec(), chain.ask.der().to_vec(), chain.ark.der().to_vec(), &roots, at(now()))
        .unwrap()
}

fn at(secs: i64) -> x509_parser::time::ASN1Time {
    x509_parser::time::ASN1Time::from_timestamp(secs).unwrap()
}

/// The first PEM block's DER.
fn der(pem: &str) -> Vec<u8> {
    let mut blocks = x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes());
    blocks.next().unwrap().unwrap().contents
}

fn crl_der(crl: String) -> Vec<u8> {
    der(&crl)
}
