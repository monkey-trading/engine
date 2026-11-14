//! The handover's wire messages: the predecessor's challenge, the successor's request, the
//! predecessor's answers, and the successor's confirmation.

use serde::{Deserialize, Serialize};

use super::Evidence;
use super::tail;

/// The predecessor's first message on each connection, which the successor's request binds, so a
/// request or confirmation recorded on one connection is refused on any other.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Challenge {
    /// 32 random bytes, hex.
    pub(super) nonce: String,
}

/// The successor's request: a one-time public key, the config it runs with and, for the keys, the
/// newest protocol version its build reads, and its quote over them and the connection's challenge.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    /// The X25519 public key the keys are encrypted to, hex.
    pub(super) public_key: String,
    /// The SHA-256 of the successor's config file, hex: with the quote's measurement, its release.
    pub(super) config_sha256: String,
    /// The last seq of the log copy the successor restored from the published archive, if any: the
    /// handover carries every segment past it, so nothing sequenced up to the stop is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) log_tip: Option<u64>,
    /// A following successor's tip, which asks for the entries past it alone (`tail_report_data`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) tail: Option<tail::Tip>,
    /// The newest protocol version the successor's build reads (`v51`), which every request for the
    /// keys names and its quote binds; a request for the secrets alone stops nothing and names none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub(super) reads: String,
    /// Asks for the delivered secrets alone, with no stop: its quote binds that under its own tag.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) secrets_only: bool,
    pub(super) evidence: Evidence,
}

/// The predecessor's answers.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Reply {
    /// The keys, encrypted to the request's public key, and the predecessor's quote over them. The
    /// keys name every log segment past the successor's tip (`handover_tails`), and each segment's
    /// bytes follow as one raw frame, so the listings are bound by the attested ciphertext.
    Keys { encapped_key: String, ciphertext: String, evidence: Evidence },
    /// Why the predecessor keeps the keys.
    Refused { reason: String },
    /// A tail request the predecessor would not serve, refused before its stop: it keeps the keys
    /// and sequences on.
    Missed { reason: String, miss: tail::TailMiss },
    /// The confirmation verified, signed with the handed-over key so the host cannot forge it: the
    /// predecessor exits.
    Confirmed { signature: String },
    /// The delivered secrets, encrypted to the request's public key, and the predecessor's quote
    /// over them. Sequencing goes on.
    Secrets { encapped_key: String, ciphertext: String, evidence: Evidence },
}

/// The successor's proof that it holds the keys: the handed-over key's signature.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Confirm {
    pub(super) signature: String,
}
