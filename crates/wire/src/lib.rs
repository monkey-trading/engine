//! Signed answers from the attested enclave: the digests its key signs over each API response and
//! stream frame, and the checks a client runs on them. The node signs these digests and clients
//! verify them, so both read one definition. The crate does no IO and needs no runtime, so a browser
//! can build it too.
//!
//! A response digest is keccak-256 over, in order and at fixed widths: `engine.response.v1`, the
//! chain id (u64 big-endian), the vault (20 bytes), the client's challenge (32 bytes), the node's
//! time in milliseconds (u64 BE), the log head's seq (u64 BE), the status (u16 BE), SHA-256 of the
//! method, a zero byte, and the path and query, and SHA-256 of the body as sent. A frame digest is
//! keccak-256 over `engine.frame.v1`, the chain id, the vault, the challenge, the frame's place `n`
//! on its connection (u64 BE), the node's time (u64 BE), and SHA-256 of the message. No other digest
//! the enclave key signs starts with either tag. A signature is 65-byte recoverable secp256k1 ECDSA,
//! `r ‖ s ‖ v` with `v` in {27, 28}, written as `0x` hex.
//!
//! A client binds an answer to its own request with a fresh random challenge: the
//! `X-Engine-Challenge` header of a request, or the `challenge` parameter of a stream connection.
//! It verifies against the challenge it sent, never the one echoed back, so a recorded response
//! answers no other request and a recorded frame fits no other connection or place. Without a
//! challenge the node signs 32 zero bytes: the answer still proves where it came from, but not that
//! it is fresh.

use k256::ecdsa::signature::hazmat::PrehashVerifier;
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sha3::{Digest, Keccak256};

/// The response digest's domain tag.
pub const RESPONSE_TAG: &[u8] = b"engine.response.v1";
/// The frame digest's domain tag.
pub const FRAME_TAG: &[u8] = b"engine.frame.v1";
/// The envelope version a signed frame carries in `v`.
pub const FRAME_VERSION: u8 = 1;
/// The request header that carries the client's challenge, which the signed response echoes.
pub const CHALLENGE_HEADER: &str = "x-engine-challenge";
/// The response header that carries the enclave key's signature.
pub const SIGNATURE_HEADER: &str = "x-engine-signature";
/// The response header that carries the node's time, in milliseconds since the Unix epoch.
pub const TIME_HEADER: &str = "x-engine-time";
/// The response header that carries the log head's seq when the response was made.
pub const SEQ_HEADER: &str = "x-engine-seq";

/// The custody deployment a signature is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deployment {
    pub chain_id: u64,
    pub vault: [u8; 20],
}

/// What a response signature covers besides the deployment.
#[derive(Clone, Copy, Debug)]
pub struct Response<'a> {
    pub challenge: [u8; 32],
    pub time_ms: u64,
    pub head_seq: u64,
    pub status: u16,
    pub method: &'a str,
    /// The request target as sent: the path, then `?` and the query when there is one.
    pub path_and_query: &'a str,
    /// The body as sent; empty for a `HEAD` request.
    pub body: &'a [u8],
}

/// One message on a signed stream connection. `msg` is the unsigned stream's message as a JSON
/// string, so the bytes the signature covers survive a JSON round trip.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedFrame {
    pub v: u8,
    pub n: u64,
    pub time: u64,
    pub msg: String,
    pub sig: String,
}

/// Checks one connection's frames in order. Once a frame fails, every later one does too: a
/// connection that has dropped, repeated, or altered a frame is closed, not trusted again.
pub struct FrameVerifier {
    signer: [u8; 20],
    deployment: Deployment,
    challenge: [u8; 32],
    /// The `n` the next frame must carry; `None` once a frame has failed.
    next: Option<u64>,
}

/// Why an answer does not verify.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the signature is not 0x and 65 bytes of hex ending in v 27 or 28")]
    MalformedSignature,
    #[error("the signature is not the attested key's over these bytes")]
    WrongSigner,
    #[error("frame version {0} is not {FRAME_VERSION}")]
    UnsupportedVersion(u8),
    #[error("frame {got} arrived where frame {expected} was due")]
    OutOfSequence { expected: u64, got: u64 },
    #[error("an earlier frame on this connection failed")]
    ConnectionFailed,
}

impl FrameVerifier {
    /// A verifier for a connection from its first frame, which the client opened with `challenge`.
    pub fn new(signer: [u8; 20], deployment: Deployment, challenge: [u8; 32]) -> Self {
        Self { signer, deployment, challenge, next: Some(0) }
    }

    /// Checks the connection's next frame: its version, its place, and the attested key's
    /// signature over it.
    /// Error cases:
    /// - an earlier frame failed
    /// - the frame's version is not `FRAME_VERSION`
    /// - its `n` is not the next place: a frame was dropped, repeated, or reordered
    /// - its signature is malformed, or not the signer's over this deployment, challenge, place,
    ///   time, and message
    pub fn verify(&mut self, frame: &SignedFrame) -> Result<(), Error> {
        let expected = self.next.take().ok_or(Error::ConnectionFailed)?;
        if frame.v != FRAME_VERSION {
            return Err(Error::UnsupportedVersion(frame.v));
        }
        if frame.n != expected {
            return Err(Error::OutOfSequence { expected, got: frame.n });
        }
        let digest = frame_digest(&self.deployment, &self.challenge, frame.n, frame.time, &frame.msg);
        check(&self.signer, &digest, &frame.sig)?;
        self.next = expected.checked_add(1);
        Ok(())
    }
}

/// Parses a challenge: exactly 64 hex characters, without `0x`.
pub fn parse_challenge(text: &str) -> Option<[u8; 32]> {
    let mut challenge = [0; 32];
    hex::decode_to_slice(text, &mut challenge).ok()?;
    Some(challenge)
}

/// The digest a response signature covers.
pub fn response_digest(deployment: &Deployment, response: &Response<'_>) -> [u8; 32] {
    let target = [response.method.as_bytes(), &[0], response.path_and_query.as_bytes()].concat();
    let mut h = Keccak256::new();
    h.update(RESPONSE_TAG);
    h.update(deployment.chain_id.to_be_bytes());
    h.update(deployment.vault);
    h.update(response.challenge);
    h.update(response.time_ms.to_be_bytes());
    h.update(response.head_seq.to_be_bytes());
    h.update(response.status.to_be_bytes());
    h.update(Sha256::digest(target));
    h.update(Sha256::digest(response.body));
    h.finalize().into()
}

/// The digest a signature over frame `n` of a connection covers.
pub fn frame_digest(deployment: &Deployment, challenge: &[u8; 32], n: u64, time_ms: u64, msg: &str) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(FRAME_TAG);
    h.update(deployment.chain_id.to_be_bytes());
    h.update(deployment.vault);
    h.update(challenge);
    h.update(n.to_be_bytes());
    h.update(time_ms.to_be_bytes());
    h.update(Sha256::digest(msg.as_bytes()));
    h.finalize().into()
}

/// Checks that `signature`, from `X-Engine-Signature`, is `signer`'s over `response` for
/// `deployment`.
/// Error cases:
/// - the signature is malformed
/// - it is not the signer's over this deployment and response
pub fn verify_response(
    signer: &[u8; 20],
    deployment: &Deployment,
    response: &Response<'_>,
    signature: &str,
) -> Result<(), Error> {
    check(signer, &response_digest(deployment, response), signature)
}

/// Checks that `signature` is `signer`'s over `digest`.
/// Error cases:
/// - the signature is not `0x` and 65 bytes of hex, `v` is not 27 or 28, or `r` or `s` is out of
///   range
/// - it recovers no key, or another key than the signer's
fn check(signer: &[u8; 20], digest: &[u8; 32], signature: &str) -> Result<(), Error> {
    let mut bytes = [0u8; 65];
    let hex = signature.strip_prefix("0x").ok_or(Error::MalformedSignature)?;
    hex::decode_to_slice(hex, &mut bytes).map_err(|_| Error::MalformedSignature)?;
    let recovery_id = match bytes[64] {
        v @ (27 | 28) => RecoveryId::from_byte(v - 27).ok_or(Error::MalformedSignature)?,
        _ => return Err(Error::MalformedSignature),
    };
    let ecdsa = Signature::from_slice(&bytes[..64]).map_err(|_| Error::MalformedSignature)?;
    let key = VerifyingKey::recover_from_prehash(digest, &ecdsa, recovery_id).map_err(|_| Error::WrongSigner)?;
    key.verify_prehash(digest, &ecdsa).map_err(|_| Error::WrongSigner)?;
    if address(&key) != *signer {
        return Err(Error::WrongSigner);
    }
    Ok(())
}

/// The EVM address of `key`: the last 20 bytes of keccak-256 over the uncompressed point.
fn address(key: &VerifyingKey) -> [u8; 20] {
    let point = key.to_sec1_point(false);
    let hash = Keccak256::digest(&point.as_bytes()[1..]);
    hash[12..].try_into().expect("keccak-256 has 32 bytes")
}

#[cfg(test)]
mod tests;
