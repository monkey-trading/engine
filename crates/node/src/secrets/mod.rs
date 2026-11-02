//! Delivering production secrets into the attested enclave: custody and checkpoint RPC provider
//! keys, market-data vendor keys, alert webhook URLs, and a screening source's key. The config names
//! each one (`config::secrets`); the host, which holds the config, the disk, and the network, never
//! sees a value in plaintext.
//!
//! The operator runs `engine admin deliver-secrets` against this enclave's delivery listener
//! (`[tee.sealing.secrets]`). It sends a fresh nonce, and the enclave answers with a one-time HPKE
//! (RFC 9180) public key, as a key handover's successor does, and a quote over the nonce, that key,
//! its config's hash, and what it holds. The operator's tool checks the quote against the release
//! manifest, the measurement, policy, and TCB, and the config hash against the manifest and its own
//! copy, and only then encrypts the bundle to that key, with the release's id in the HPKE context. The
//! bundle carries the operator's signature over its version and contents. The enclave decrypts it,
//! checks the signer, seals the bundle for this release (`tee::secrets`), and answers with a quote
//! over a receipt, so the host cannot forge one.
//!
//! Who may deliver is the operator key in effect. Before the log is recovered that is the key the
//! attested config names: reading the vault can itself take a delivered key, so no log can be
//! authenticated yet, and a host could forge one naming any operator. Once the node serves it is the
//! key the log records, or a rotation's that has come due, as `POST /v1/admin` checks it; on a node
//! a pending rotation names the successor, its own configured key alone. A node whose config still
//! names a key the log has since rotated away from takes that key until the log opens; what a bundle
//! can do bounds this (below).
//!
//! What the host can do at each step:
//! - read the bundle: no, only the one-time key's holder can, and the quote proves who that is.
//!   Answering with another enclave, or with a recorded offer, fails the tool's checks, since the
//!   quote binds this connection's nonce and proves the release;
//! - replay an older delivery: no, each connection has a new key, so an old ciphertext does not
//!   decrypt, and a rotated secret cannot be rolled back through the listener. The version refuses an
//!   older bundle even under a fresh encryption;
//! - forge a bundle: no, anyone can encrypt to the offered key, but only the operator key signs;
//! - forge a receipt: no, it is quoted, so the tool reports only what the enclave did. A refusal is
//!   not quoted and can be forged, which is only the denial the host has anyway;
//! - delay or drop a delivery: yes, for as long as a connection's timeouts allow, and the tool then
//!   reports no receipt. A retry with the same version and secrets is accepted as already sealed;
//! - point the node at its own servers: no, a delivered value fills a URL's key, and the attested
//!   config fixes the URL's origin. A bundle, even one a stolen operator key signs, can name only
//!   another key at the same provider, a vendor key, or a webhook, so it can deny service or redirect
//!   pages, both of which a host that drops traffic can do already, but cannot change what the node
//!   believes about the chain;
//! - restore an older sealed file: see `tee::secrets`.
//!
//! A bundle reaching a running node is sealed at once, but nothing running picks it up: the custody
//! quorum, feeds, and webhooks hold connections and clients built from what they started with. The
//! receipt says a restart is needed, and `secrets_restart_required` stays open until one, which keeps
//! the keys and costs nothing but the restart. A node still holding for its required secrets takes
//! them and goes on starting.
//!
//! An approved upgrade moves the bundle to the next release, signature and all (`handover::secrets`);
//! an election moves nothing, since the elected operator brings its own.

mod bundle;
pub(crate) use bundle::{Bundle, SignedBundle};
pub(crate) mod delivery;
mod listener;
pub(crate) use listener::{Authority, Listener};

#[cfg(test)]
pub(crate) mod tests;
