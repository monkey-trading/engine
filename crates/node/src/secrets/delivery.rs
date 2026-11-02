//! The delivery's messages, in the handover's framing, what each quote binds, and the encryption. The
//! tool sends a `Hello`, the enclave an `Offer`; a status read ends there, and a delivery goes on
//! with a `Delivery` and a `Reply`.

use anyhow::Context;
use hpke::aead::ChaCha20Poly1305;
use hpke::kdf::HkdfSha256;
use hpke::{Deserializable, Kem, OpModeR, OpModeS, Serializable};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::config::SecretName;
use crate::handover::{Evidence, Kx, padded};
use crate::log::CheckpointDomain;
use crate::secrets::SignedBundle;
use crate::tee::sealing::Release;

/// The tool's first message: the nonce the enclave's quote binds, so a recorded offer is refused.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hello {
    /// 32 random bytes, hex.
    pub nonce: String,
}

/// What the enclave reports it holds, which its quote binds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Status {
    /// The newest bundle sealed here. A lower number than the operator delivered shows a host
    /// restored an older sealed file.
    pub sealed_version: Option<u64>,
    /// The bundle this start runs with; `None` while it holds for its secrets, or runs without any.
    pub running_version: Option<u64>,
    /// The secrets the config names and the sealed bundle lacks.
    pub missing: Vec<SecretName>,
}

/// The enclave's answer: a one-time key for this connection, and its quote over it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Offer {
    /// The X25519 public key the bundle is encrypted to, hex.
    pub public_key: String,
    /// The SHA-256 of the enclave's config file, hex: with the quote's measurement, its release.
    pub config_sha256: String,
    pub status: Status,
    pub evidence: Evidence,
}

/// The bundle, encrypted to the offered key.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Delivery {
    pub encapped_key: String,
    pub ciphertext: String,
}

/// What the enclave did with a delivery, which its quote binds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Receipt {
    pub version: u64,
    /// The same bundle was sealed already: a retry after a lost receipt.
    pub already_sealed: bool,
    /// The secrets the config names and this bundle lacks.
    pub missing: Vec<SecretName>,
    /// What runs still uses another bundle, and picks this one up only at a restart.
    pub restart_required: bool,
}

/// The enclave's answer to a delivery.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Reply {
    Sealed {
        receipt: Receipt,
        evidence: Evidence,
    },
    /// Why it was refused; names no value. Not quoted: a host can forge a refusal, as it can drop
    /// the delivery.
    Refused {
        reason: String,
    },
}

/// The report data an offer's quote binds.
pub(crate) fn offer_report_data(
    domain: CheckpointDomain,
    nonce: &[u8; 32],
    public_key: &[u8],
    config_sha256: &[u8; 32],
    status: &Status,
) -> [u8; 64] {
    let mut hash = Sha256::new();
    hash.update(b"engine.secrets.offer.v1");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(nonce);
    hash.update(public_key);
    hash.update(config_sha256);
    hash.update(serde_json::to_vec(status).expect("a status encodes"));
    padded(hash.finalize().into())
}

/// The report data a receipt's quote binds: the delivery it answers, on this connection.
pub(crate) fn receipt_report_data(
    domain: CheckpointDomain,
    nonce: &[u8; 32],
    delivery: &Delivery,
    receipt: &Receipt,
) -> [u8; 64] {
    let mut hash = Sha256::new();
    hash.update(b"engine.secrets.receipt.v1");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(nonce);
    hash.update(Sha256::digest([delivery.encapped_key.as_bytes(), delivery.ciphertext.as_bytes()].concat()));
    hash.update(serde_json::to_vec(receipt).expect("a receipt encodes"));
    padded(hash.finalize().into())
}

/// HPKE's `info`: the deployment and the release the bundle is for, so a bundle for another release
/// does not open here even under this key.
fn hpke_info(domain: CheckpointDomain, release: &Release) -> Vec<u8> {
    [b"engine.secrets.v1".as_slice(), &domain.chain_id.to_be_bytes(), &domain.vault.0, &release.id()].concat()
}

/// Encrypts `signed` to `public_key` for `release` on this connection: the delivery message.
/// Error cases:
/// - the public key is not an X25519 key, or encryption fails
pub(crate) fn encrypt(
    domain: CheckpointDomain,
    release: &Release,
    public_key: &[u8],
    nonce: &[u8; 32],
    signed: &SignedBundle,
) -> anyhow::Result<Delivery> {
    let (encapped, ciphertext) = seal(&hpke_info(domain, release), public_key, nonce, &signed.to_bytes()?)?;
    Ok(Delivery { encapped_key: hex::encode(encapped), ciphertext: hex::encode(ciphertext) })
}

/// Decrypts what `encrypt` sent. Its errors say only that it failed: nothing about the plaintext.
/// Error cases:
/// - the message is malformed, or was encrypted to another key, for another release or deployment,
///   or on another connection
/// - what it decrypts to is not a signed bundle
pub(crate) fn decrypt(
    domain: CheckpointDomain,
    release: &Release,
    secret: &<Kx as Kem>::PrivateKey,
    nonce: &[u8; 32],
    delivery: &Delivery,
) -> anyhow::Result<SignedBundle> {
    let encapped = hex::decode(&delivery.encapped_key).context("the encapsulated key is not hex")?;
    let ciphertext = hex::decode(&delivery.ciphertext).context("the ciphertext is not hex")?;
    let plaintext = open(&hpke_info(domain, release), secret, nonce, &encapped, &ciphertext)
        .context("the bundle does not decrypt for this release under this connection's key")?;
    SignedBundle::from_bytes(&plaintext)
}

/// HPKE base-mode encryption in the handover's suite: the encapsulated key and the ciphertext.
/// Error cases:
/// - the public key is not an X25519 key, or encryption fails
pub(crate) fn seal(info: &[u8], public_key: &[u8], aad: &[u8], plaintext: &[u8]) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let recipient = <Kx as Kem>::PublicKey::from_bytes(public_key)
        .map_err(|_| anyhow::anyhow!("the offered key is not an X25519 key"))?;
    let (encapped, ciphertext) =
        hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, Kx>(&OpModeS::Base, &recipient, info, plaintext, aad)
            .map_err(|_| anyhow::anyhow!("encrypting the bundle failed"))?;
    Ok((encapped.to_bytes().to_vec(), ciphertext))
}

/// Opens what `seal` encrypted.
/// Error cases:
/// - the encapsulated key is malformed, or the ciphertext does not open
pub(crate) fn open(
    info: &[u8],
    secret: &<Kx as Kem>::PrivateKey,
    aad: &[u8],
    encapped: &[u8],
    ciphertext: &[u8],
) -> anyhow::Result<Vec<u8>> {
    let encapped = <Kx as Kem>::EncappedKey::from_bytes(encapped)
        .map_err(|_| anyhow::anyhow!("the encapsulated key is malformed"))?;
    hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, Kx>(&OpModeR::Base, secret, &encapped, info, ciphertext, aad)
        .map_err(|_| anyhow::anyhow!("the ciphertext does not open"))
}
