//! Certificate and revocation-list signatures, checked with the RustCrypto implementations so the
//! native and browser builds verify with the same code. Two algorithms are accepted, the ones the
//! vendor's chains and the test chain use: RSASSA-PSS with SHA-384 (the vendor's roots,
//! intermediates, VCEK certificates, and revocation lists) and ECDSA on P-384 with SHA-384.

use p384::ecdsa::signature::Verifier as _;
use rsa::pkcs1::DecodeRsaPublicKey as _;
use rsa::signature::Verifier as _;
use rsa::traits::PublicKeyParts as _;
use x509_parser::asn1_rs::BitString;
use x509_parser::error::X509Error;
use x509_parser::oid_registry::{
    OID_NIST_EC_P384, OID_NIST_HASH_SHA384, OID_PKCS1_RSASSAPSS, OID_SIG_ECDSA_WITH_SHA384,
};
use x509_parser::signature_algorithm::RsaSsaPssParams;
use x509_parser::x509::{AlgorithmIdentifier, SubjectPublicKeyInfo};

/// The smallest RSA modulus accepted, in bits. The largest is the `rsa` crate's own limit, 4096
/// bits: the size of the vendor's root and intermediate keys.
const MIN_RSA_BITS: usize = 2048;

/// Checks `signature`, made with `algorithm`, over `signed` (a certificate's or list's
/// to-be-signed bytes) against `signer`, the issuer's public key. RSASSA-PSS uses SHA-384 for the
/// digest and MGF1, with a 48-byte salt.
/// Error cases:
/// - `SignatureUnsupportedAlgorithm`: the algorithm is neither RSASSA-PSS with SHA-384 nor ECDSA
///   with SHA-384 on a P-384 key
/// - `SignatureVerificationError`: the key or signature does not parse, the RSA key is under 2048
///   or over 4096 bits, or the signature does not verify
pub(crate) fn verify_signed(
    signer: &SubjectPublicKeyInfo<'_>,
    algorithm: &AlgorithmIdentifier<'_>,
    signature: &BitString<'_>,
    signed: &[u8],
) -> Result<(), X509Error> {
    let key = &signer.subject_public_key.data;
    if algorithm.algorithm == OID_PKCS1_RSASSAPSS {
        let params = algorithm.parameters.as_ref().ok_or(X509Error::SignatureUnsupportedAlgorithm)?;
        let params = RsaSsaPssParams::try_from(params).map_err(|_| X509Error::SignatureUnsupportedAlgorithm)?;
        if *params.hash_algorithm_oid() != OID_NIST_HASH_SHA384 {
            return Err(X509Error::SignatureUnsupportedAlgorithm);
        }
        let key = rsa::RsaPublicKey::from_pkcs1_der(key).map_err(|_| X509Error::SignatureVerificationError)?;
        if key.n().bits() < MIN_RSA_BITS {
            return Err(X509Error::SignatureVerificationError);
        }
        let signature =
            rsa::pss::Signature::try_from(&signature.data[..]).map_err(|_| X509Error::SignatureVerificationError)?;
        rsa::pss::VerifyingKey::<rsa::sha2::Sha384>::new(key)
            .verify(signed, &signature)
            .map_err(|_| X509Error::SignatureVerificationError)
    } else if algorithm.algorithm == OID_SIG_ECDSA_WITH_SHA384 {
        let curve = signer.algorithm.parameters.as_ref().and_then(|params| params.as_oid().ok());
        if curve != Some(OID_NIST_EC_P384) {
            return Err(X509Error::SignatureUnsupportedAlgorithm);
        }
        let key = p384::ecdsa::VerifyingKey::from_sec1_bytes(key).map_err(|_| X509Error::SignatureVerificationError)?;
        let signature =
            p384::ecdsa::Signature::from_der(&signature.data).map_err(|_| X509Error::SignatureVerificationError)?;
        key.verify(signed, &signature).map_err(|_| X509Error::SignatureVerificationError)
    } else {
        Err(X509Error::SignatureUnsupportedAlgorithm)
    }
}
