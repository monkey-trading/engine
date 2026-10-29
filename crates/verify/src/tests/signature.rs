//! Each branch of the certificate signature check, called directly: the two accepted algorithms
//! verify, and every refusal returns its own error.

use rand::SeedableRng as _;
use rsa::pkcs1::EncodeRsaPublicKey as _;
use rsa::signature::{RandomizedSigner as _, SignatureEncoding as _};
use x509_parser::asn1_rs::{Any, BitString, FromDer as _};
use x509_parser::certificate::X509Certificate;
use x509_parser::error::X509Error;
use x509_parser::oid_registry::{OID_PKCS1_RSASSAPSS, OID_SIG_ECDSA_WITH_SHA384};
use x509_parser::x509::{AlgorithmIdentifier, SubjectPublicKeyInfo};

use crate::signature::verify_signed;

/// RSASSA-PSS parameters naming SHA-256: SEQUENCE { [0] SEQUENCE { sha256, NULL } }.
const PSS_SHA256_PARAMS: &[u8] =
    &[0x30, 0x11, 0xa0, 0x0f, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00];

/// What a case checks, the key, algorithm, signature, and signed bytes it passes, and the outcome.
type Case<'a> = (
    &'a str,
    &'a SubjectPublicKeyInfo<'a>,
    &'a AlgorithmIdentifier<'a>,
    &'a BitString<'a>,
    &'a [u8],
    Result<(), X509Error>,
);

#[test]
fn each_signature_branch_verifies_or_refuses_with_its_own_error() {
    // The vendor's intermediate, RSASSA-PSS/SHA-384 by its root: the accepted RSA case.
    let root = der(include_str!("../roots/ark-milan.pem"));
    let ask = der(include_str!("ask-milan.pem"));
    let (root, ask) = (cert(&root), cert(&ask));
    let pss = &ask.signature_algorithm;
    let unparameterised = AlgorithmIdentifier { algorithm: OID_PKCS1_RSASSAPSS, parameters: None };
    let sha256_params = Any::from_der(PSS_SHA256_PARAMS).unwrap().1;
    let pss_sha256 = AlgorithmIdentifier { algorithm: OID_PKCS1_RSASSAPSS, parameters: Some(sha256_params) };

    // A P-384 certificate, and a P-256 one whose key the SHA-384 ECDSA identifier is put on.
    let (p384_key, p256_key) = (key(&rcgen::PKCS_ECDSA_P384_SHA384), key(&rcgen::PKCS_ECDSA_P256_SHA256));
    let p384 = rcgen::CertificateParams::new(Vec::new()).unwrap().self_signed(&p384_key).unwrap();
    let p256 = rcgen::CertificateParams::new(Vec::new()).unwrap().self_signed(&p256_key).unwrap();
    let (p384, p256) = (cert(p384.der()), cert(p256.der()));
    let ecdsa_sha384 = AlgorithmIdentifier { algorithm: OID_SIG_ECDSA_WITH_SHA384, parameters: None };

    let null_params = Any::from_der(&[0x05, 0x00]).unwrap().1;
    let pss_null = AlgorithmIdentifier { algorithm: OID_PKCS1_RSASSAPSS, parameters: Some(null_params) };
    let point = &p384.public_key().subject_public_key.data;
    let truncated = SubjectPublicKeyInfo {
        algorithm: p384.public_key().algorithm.clone(),
        subject_public_key: BitString::new(0, &point[..point.len() / 2]),
        raw: &[],
    };
    let (rsa_signed, rsa_signature) = (ask.tbs_certificate.as_ref(), &ask.signature_value);
    let (ec_signed, ec_signature) = (p384.tbs_certificate.as_ref(), &p384.signature_value);
    let mut altered = ec_signed.to_vec();
    altered[ec_signed.len() / 2] ^= 1;

    let unsupported = Err(X509Error::SignatureUnsupportedAlgorithm);
    let refused = Err(X509Error::SignatureVerificationError);
    #[rustfmt::skip]
    let cases: [Case<'_>; 11] = [
        ("RSASSA-PSS with SHA-384", root.public_key(), pss, rsa_signature, rsa_signed, Ok(())),
        ("RSASSA-PSS with no parameters", root.public_key(), &unparameterised, rsa_signature, rsa_signed, unsupported.clone()),
        ("RSASSA-PSS with NULL parameters", root.public_key(), &pss_null, rsa_signature, rsa_signed, unsupported.clone()),
        ("RSASSA-PSS with SHA-256", root.public_key(), &pss_sha256, rsa_signature, rsa_signed, unsupported.clone()),
        ("an RSA signature under an EC key", p384.public_key(), pss, rsa_signature, rsa_signed, refused.clone()),
        ("ECDSA SHA-384 on P-384", p384.public_key(), &ecdsa_sha384, ec_signature, ec_signed, Ok(())),
        ("an altered ECDSA signed part", p384.public_key(), &ecdsa_sha384, ec_signature, &altered, refused.clone()),
        ("ECDSA SHA-384 on P-256", p256.public_key(), &ecdsa_sha384, ec_signature, ec_signed, unsupported.clone()),
        ("ECDSA SHA-256", p256.public_key(), &p256.signature_algorithm, ec_signature, ec_signed, unsupported.clone()),
        ("a P-384 point that does not parse", &truncated, &ecdsa_sha384, ec_signature, ec_signed, refused.clone()),
        ("an ECDSA signature that is not DER", p384.public_key(), &ecdsa_sha384, rsa_signature, ec_signed, refused),
    ];
    for (what, key, algorithm, signature, signed, expected) in cases {
        assert_eq!(verify_signed(key, algorithm, signature, signed), expected, "{what}");
    }
}

#[test]
fn an_rsa_key_verifies_from_2048_to_4096_bits() {
    let ask = der(include_str!("ask-milan.pem"));
    let root = der(include_str!("../roots/ark-milan.pem"));
    let (ask, root) = (cert(&ask), cert(&root));
    let rsa_encryption = &root.public_key().algorithm;
    let mut rng = rand::rngs::StdRng::seed_from_u64(2048);
    let signed = b"the to-be-signed bytes";
    let refused = Err(X509Error::SignatureVerificationError);
    for (bits, expected) in [(2047, refused.clone()), (2048, Ok(())), (4096, Ok(())), (4097, refused)] {
        let private = rsa::RsaPrivateKey::new(&mut rng, bits).unwrap();
        let public = private.to_public_key().to_pkcs1_der().unwrap();
        let signature =
            rsa::pss::SigningKey::<rsa::sha2::Sha384>::new(private).sign_with_rng(&mut rng, signed).to_vec();
        let spki = SubjectPublicKeyInfo {
            algorithm: rsa_encryption.clone(),
            subject_public_key: BitString::new(0, public.as_bytes()),
            raw: &[],
        };
        let signature = BitString::new(0, &signature);
        assert_eq!(verify_signed(&spki, &ask.signature_algorithm, &signature, signed), expected, "{bits} bits");
    }
}

// ── Helpers ──

/// The first PEM block's DER.
fn der(pem: &str) -> Vec<u8> {
    let mut blocks = x509_parser::pem::Pem::iter_from_buffer(pem.as_bytes());
    blocks.next().unwrap().unwrap().contents
}

fn cert(der: &[u8]) -> X509Certificate<'_> {
    use x509_parser::prelude::FromDer as _;
    X509Certificate::from_der(der).unwrap().1
}

fn key(algorithm: &'static rcgen::SignatureAlgorithm) -> rcgen::KeyPair {
    rcgen::KeyPair::generate_for(algorithm).unwrap()
}
