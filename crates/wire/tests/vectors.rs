//! Replays `vectors.json`, the known answers a browser verifier replays too: each digest is
//! recomputed from its inputs, each verification ends as recorded, and each signature is
//! re-derived from how the vector says it was made, so no vector can pass by accident.

use engine_wire::{
    Deployment, Error, FrameVerifier, Response, SignedFrame, frame_digest, parse_challenge, response_digest,
    verify_response,
};
use k256::ecdsa::SigningKey;
use serde::Deserialize;

const VECTORS: &str = include_str!("vectors.json");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Vectors {
    about: String,
    private_key: String,
    other_private_key: String,
    signer: String,
    responses: Vec<ResponseVector>,
    connections: Vec<ConnectionVector>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseVector {
    name: String,
    chain_id: u64,
    vault: String,
    challenge: String,
    time_ms: u64,
    head_seq: u64,
    status: u16,
    method: String,
    path_and_query: String,
    body: String,
    digest: String,
    signature: String,
    error: Option<String>,
    /// The valid vector whose signature this one reuses.
    #[serde(default)]
    base: Option<String>,
    /// Signed by the other key over this vector's own digest.
    #[serde(default)]
    other_key: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectionVector {
    name: String,
    chain_id: u64,
    vault: String,
    challenge: String,
    frames: Vec<SignedFrame>,
    /// Each frame's digest as a verifier of this connection computes it.
    digests: Vec<String>,
    verified: usize,
    error: Option<String>,
    /// The valid connection whose frames these are.
    #[serde(default)]
    base: Option<String>,
    /// Signed by the other key over this connection's own digests.
    #[serde(default)]
    other_key: bool,
}

#[test]
fn every_response_vector_digests_verifies_and_was_signed_as_it_says() {
    let vectors = vectors();
    let (key, other) = (key(&vectors.private_key), key(&vectors.other_private_key));
    let signer = bytes::<20>(&vectors.signer);
    assert!(!vectors.about.is_empty());
    for v in &vectors.responses {
        let (deployment, body) = (deployment(v.chain_id, &v.vault), v.body.as_bytes());
        let response = Response {
            challenge: parse_challenge(&v.challenge).expect("a 64-hex challenge"),
            time_ms: v.time_ms,
            head_seq: v.head_seq,
            status: v.status,
            method: &v.method,
            path_and_query: &v.path_and_query,
            body,
        };
        let digest = response_digest(&deployment, &response);
        assert_eq!(hex0x(&digest), v.digest, "{}: digest", v.name);
        let result = verify_response(&signer, &deployment, &response, &v.signature);
        assert_eq!(result.err().map(kind), v.error, "{}: verification", v.name);
        match (&v.base, v.other_key, &v.error) {
            (None, false, None) => assert_eq!(sign(&key, digest), v.signature, "{}: signature", v.name),
            (None, true, Some(_)) => assert_eq!(sign(&other, digest), v.signature, "{}: other key", v.name),
            (Some(base), false, Some(_)) => {
                let base = vectors.responses.iter().find(|b| &b.name == base && b.error.is_none());
                assert_eq!(base.map(|b| &b.signature), Some(&v.signature), "{}: base signature", v.name);
            }
            // A refusal of the signature's own shape, which no key produced.
            (None, false, Some(error)) => assert_eq!(error, "malformed_signature", "{}", v.name),
            _ => panic!("{}: a vector is valid, reuses a base, or is the other key's", v.name),
        }
    }
    let valid = vectors.responses.iter().filter(|v| v.error.is_none()).count();
    assert!(valid >= 3 && vectors.responses.len() > valid, "valid and refused responses");
}

#[test]
fn every_connection_vector_digests_verifies_to_its_recorded_frame_and_was_signed_as_it_says() {
    let vectors = vectors();
    let (key, other) = (key(&vectors.private_key), key(&vectors.other_private_key));
    let signer = bytes::<20>(&vectors.signer);
    for v in &vectors.connections {
        let deployment = deployment(v.chain_id, &v.vault);
        let challenge = parse_challenge(&v.challenge).expect("a 64-hex challenge");
        let digests: Vec<[u8; 32]> =
            v.frames.iter().map(|f| frame_digest(&deployment, &challenge, f.n, f.time, &f.msg)).collect();
        assert_eq!(digests.iter().map(|d| hex0x(d)).collect::<Vec<_>>(), v.digests, "{}: digests", v.name);

        let mut verifier = FrameVerifier::new(signer, deployment, challenge);
        let results: Vec<Result<(), Error>> = v.frames.iter().map(|f| verifier.verify(f)).collect();
        let verified = results.iter().take_while(|r| r.is_ok()).count();
        assert_eq!(verified, v.verified, "{}: frames verified", v.name);
        let error = results.into_iter().find_map(Result::err).map(kind);
        assert_eq!(error, v.error, "{}: error", v.name);

        match (&v.base, v.other_key) {
            (None, false) => {
                assert!(v.error.is_none(), "{}: a connection the key signed verifies", v.name);
                for (frame, digest) in v.frames.iter().zip(digests) {
                    assert_eq!(sign(&key, digest), frame.sig, "{}: frame {}", v.name, frame.n);
                }
            }
            (None, true) => {
                for (frame, digest) in v.frames.iter().zip(digests) {
                    assert_eq!(sign(&other, digest), frame.sig, "{}: frame {}", v.name, frame.n);
                }
            }
            (Some(base), false) => {
                let base = vectors.connections.iter().find(|b| &b.name == base && b.error.is_none());
                let base = base.unwrap_or_else(|| panic!("{}: no valid base connection", v.name));
                assert!(v.frames.iter().all(|f| base.frames.contains(f)), "{}: frames from the base", v.name);
            }
            (Some(_), true) => panic!("{}: a vector reuses a base or is the other key's", v.name),
        }
    }
}

// ── Helpers ──

fn vectors() -> Vectors {
    serde_json::from_str(VECTORS).expect("vectors.json parses")
}

fn key(hex: &str) -> SigningKey {
    SigningKey::from_slice(&bytes::<32>(hex)).unwrap()
}

fn deployment(chain_id: u64, vault: &str) -> Deployment {
    Deployment { chain_id, vault: bytes(vault) }
}

fn bytes<const N: usize>(hex: &str) -> [u8; N] {
    let mut out = [0; N];
    hex::decode_to_slice(hex.strip_prefix("0x").expect("0x hex"), &mut out).unwrap();
    out
}

fn hex0x(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

/// `key`'s recoverable signature over `digest`, as the node writes it.
fn sign(key: &SigningKey, digest: [u8; 32]) -> String {
    let (signature, recovery_id) = key.sign_prehash_recoverable(&digest);
    format!("0x{}{:02x}", hex::encode(signature.to_bytes()), 27 + recovery_id.to_byte())
}

/// The vectors' name for an error.
fn kind(error: Error) -> String {
    match error {
        Error::MalformedSignature => "malformed_signature",
        Error::WrongSigner => "wrong_signer",
        Error::UnsupportedVersion(_) => "unsupported_version",
        Error::OutOfSequence { .. } => "out_of_sequence",
        Error::ConnectionFailed => "connection_failed",
    }
    .to_owned()
}
