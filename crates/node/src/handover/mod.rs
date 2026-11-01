//! Handing the enclave's keys, and the log's tail, to the next release. An upgrade changes the
//! launch measurement, and a config change the config hash, so the new release cannot open the
//! running release's sealed file; the running release hands it the keys instead, only once each
//! has attested the other and the vault's governance has made the new release its current one.
//! An election an operator registry names moves the vault's release at its effective time, and the
//! elected operator's enclave then takes the keys — and the final log segment — from the incumbent
//! whose handover endpoint the registry recorded (#44).
//!
//! The new release (the successor, `take`) connects to the running release's handover listener
//! (the predecessor, `Predecessor`), which answers with a fresh challenge, and sends a quote over
//! the challenge, a fresh HPKE (RFC 9180) public key, and its config's hash. The predecessor checks the quote against the vendor's chain at trusted time; the
//! release it proves, the measurement and that config, against the vault's current release at
//! custody finality, which only the timelock sets; and the guest policy and TCB against its own. It
//! then stops sequencing, takes its final checkpoint, seals the log's tail segment — the archive
//! serves only completed segments, and the API is already down when the successor could fetch it —
//! and sends the keys, that checkpoint, and the segment encrypted to the successor's key, with its
//! own quote over the ciphertext. The successor checks that quote, opens the keys, accepts them
//! only if the vault already names their signing key, seals them for its own release, writes the
//! segment, requires the log to end at the handed-over checkpoint, and confirms by signing with the
//! handed-over key; the predecessor then exits, so the two never sign at once. The successor serves
//! only once its log reaches the predecessor's final checkpoint (`node::keys::check_predecessor_head`)
//! and the vault names the key. It seals them in the vault's generation of its release, and once
//! the vault names the successor, no start unseals the predecessor's copy, even if governance names
//! that release again (`ensure_named`).
//!
//! The host carries the connection, so it can delay or drop it. It cannot read the keys, which only
//! the attested successor can decrypt; substitute another successor, since the vault names the
//! release and the quote proves it; replay an earlier request or confirmation, which bind another
//! connection's challenge; or forge the confirmation, which only the handed-over key signs, nor its
//! acknowledgement, which the predecessor signs with the same key. A predecessor that has stopped
//! keeps serving successors until one confirms, since the vault names the successor and its own copy
//! of the keys no longer opens; a successor that sees no acknowledgement keeps the keys sealed but
//! refuses to start, since the predecessor may still hold them. The successor checks the predecessor's
//! certificates at the host's time, before trusted time runs; an enclave the host runs can still only
//! hand over the key the vault already names. The segment it writes is hash-checked against the
//! listing the predecessor signed into its quote-bound message, and the log it forms must end at
//! the checkpoint inside the encrypted keys, so a host that swaps the segment bytes is caught before
//! the confirmation goes out.
//!
//! The mock TEE's quotes and stand-in measurements prove nothing: two mock enclaves hand over to
//! each other for development only, and a real enclave never accepts a mock one.

use std::time::Duration;

use anyhow::{Context, ensure};
use hpke::aead::ChaCha20Poly1305;
use hpke::kdf::HkdfSha256;
use hpke::kem::X25519HkdfSha256;
use hpke::{Deserializable, Kem, OpModeR, OpModeS};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sha3::{Digest as _, Keccak256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;
use tracing::{info, warn};

use crate::checkpoints::{CheckpointChain, CheckpointRpc, CurrentRelease, View};
use crate::config::NodeConfig;
use crate::log::CheckpointDomain;
use crate::tee::keys::KeySet;
use crate::tee::sealing::Release;

mod evidence;
pub(crate) use evidence::Evidence;
mod predecessor;
pub(crate) use predecessor::{DrainReply, Predecessor, Published};
mod successor;
pub(crate) use successor::take;

/// A read-only client for the vault `[checkpoints]` reads, or `None` without `[checkpoints]`.
/// Error cases:
/// - `[checkpoints]` is set without EVM custody, or the vault address or RPC settings are malformed
pub(crate) fn vault_reader(cfg: &NodeConfig) -> anyhow::Result<Option<CheckpointRpc>> {
    let Some(checkpoints) = &cfg.checkpoints else { return Ok(None) };
    let evm = cfg.custody.evm.as_ref().context("[checkpoints] needs EVM custody")?;
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    CheckpointRpc::read_only(checkpoints, domain, evm.usdc, evm.withdrawal_finality_tag, 0).map(Some)
}

/// The vault's current release, and its generation, at finality.
/// Error cases:
/// - the vault cannot be read
pub(crate) async fn current_release(vault: &impl CheckpointChain) -> anyhow::Result<CurrentRelease> {
    let finalized = vault.observe(View::Finalized, &[]).await?;
    vault.current_release(&finalized).await
}

/// The generation of `current`, once it is `release`: governance names one release at a time, so
/// an approved upgrade or config change supersedes the release before it, and naming a release
/// again starts a new generation.
/// Error cases:
/// - `current` is another release
pub(crate) fn ensure_named(current: CurrentRelease, release: &Release) -> anyhow::Result<u64> {
    ensure!(
        current.id == release.id(),
        "the vault's current release is 0x{}, not 0x{} (launch measurement 0x{}, config 0x{})",
        hex::encode(current.id),
        hex::encode(release.id()),
        hex::encode(release.launch_digest),
        hex::encode(release.config_sha256)
    );
    Ok(current.generation)
}

/// The HPKE suite: DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305.
type Kx = X25519HkdfSha256;

/// The largest message either side reads; collateral, the largest part, stays under 300 KiB.
const MAX_MESSAGE_BYTES: u32 = 1 << 20;
/// The largest log segment frame either side reads: the same bound the archive serves a segment
/// under, so every segment a predecessor can publish it can also hand over.
const MAX_SEGMENT_BYTES: u32 = 64 << 20;
/// How long a peer gets to send the challenge or a request, or the confirmation after the keys.
const MESSAGE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the successor waits for the keys: the predecessor's drain and its vendor fetch.
const REPLY_TIMEOUT: Duration = Duration::from_secs(300);

/// The predecessor's first message on each connection, which the successor's request binds, so a
/// request or confirmation recorded on one connection is refused on any other.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Challenge {
    /// 32 random bytes, hex.
    nonce: String,
}

/// The successor's request: a one-time public key and the config it runs with, and its quote over
/// both and the connection's challenge.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    /// The X25519 public key the keys are encrypted to, hex.
    public_key: String,
    /// The SHA-256 of the successor's config file, hex: with the quote's measurement, its release.
    config_sha256: String,
    /// The last seq of the log copy the successor restored from the published archive, if any: the
    /// handover carries every segment past it, so nothing sequenced up to the stop is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    log_tip: Option<u64>,
    evidence: Evidence,
}

/// The predecessor's answers.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Reply {
    /// The keys, encrypted to the request's public key, and the predecessor's quote over them. The
    /// keys name every log segment past the successor's tip (`handover_tails`), and each segment's
    /// bytes follow as one raw frame, so the listings are bound by the attested ciphertext.
    Keys { encapped_key: String, ciphertext: String, evidence: Evidence },
    /// Why the predecessor keeps the keys.
    Refused { reason: String },
    /// The confirmation verified, signed with the handed-over key so the host cannot forge it: the
    /// predecessor exits.
    Confirmed { signature: String },
}

/// The successor's proof that it holds the keys: the handed-over key's signature.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirm {
    signature: String,
}

/// The report data a successor's quote binds: the connection's challenge, its public key, and its
/// config's hash, for this deployment.
fn request_report_data(
    domain: CheckpointDomain,
    challenge: &[u8; 32],
    public_key: &[u8],
    config_sha256: &[u8; 32],
) -> [u8; 64] {
    let mut hash = Sha256::new();
    hash.update(b"engine.handover.request.v1");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(challenge);
    hash.update(public_key);
    hash.update(config_sha256);
    padded(hash.finalize().into())
}

/// The report data the predecessor's quote binds: the request it answers, and the keys it sent.
fn keys_report_data(domain: CheckpointDomain, request: &[u8; 64], encapped_key: &[u8], ciphertext: &[u8]) -> [u8; 64] {
    let mut hash = Sha256::new();
    hash.update(b"engine.handover.keys.v1");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(request);
    hash.update(Sha256::digest([encapped_key, ciphertext].concat()));
    padded(hash.finalize().into())
}

/// What the successor signs with the handed-over key to confirm the request it made.
fn confirm_digest(domain: CheckpointDomain, request: &[u8; 64]) -> [u8; 32] {
    let mut hash = Keccak256::new();
    hash.update(b"engine.handover.confirm.v1");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(request);
    hash.finalize().into()
}

/// What the predecessor signs to acknowledge the confirmation of the request it answered.
fn confirmed_digest(domain: CheckpointDomain, request: &[u8; 64]) -> [u8; 32] {
    let mut hash = Keccak256::new();
    hash.update(b"engine.handover.confirmed.v1");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(request);
    hash.finalize().into()
}

/// A SHA-256 in the first half of report data, the second half zero.
fn padded(digest: [u8; 32]) -> [u8; 64] {
    let mut report_data = [0u8; 64];
    report_data[..32].copy_from_slice(&digest);
    report_data
}

/// HPKE's `info`: the deployment the keys belong to.
fn hpke_info(domain: CheckpointDomain) -> Vec<u8> {
    [b"engine.handover.v1".as_slice(), &domain.chain_id.to_be_bytes(), &domain.vault.0].concat()
}

/// Encrypts `keys` to `public_key` for this deployment and request: the encapsulated key and the
/// ciphertext.
/// Error cases:
/// - the public key is not an X25519 key, or encryption fails
fn seal_keys(
    domain: CheckpointDomain,
    public_key: &[u8],
    request: &[u8; 64],
    keys: &KeySet,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let recipient = <Kx as Kem>::PublicKey::from_bytes(public_key)
        .map_err(|e| anyhow::anyhow!("the successor's key is not an X25519 key: {e}"))?;
    let plaintext = serde_json::to_vec(keys).context("encoding the keys")?;
    let (encapped, ciphertext) = hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, Kx>(
        &OpModeS::Base,
        &recipient,
        &hpke_info(domain),
        &plaintext,
        request,
    )
    .map_err(|e| anyhow::anyhow!("encrypting the keys: {e}"))?;
    Ok((hpke::Serializable::to_bytes(&encapped).to_vec(), ciphertext))
}

/// Decrypts the keys `seal_keys` sent for this deployment and request.
/// Error cases:
/// - the encapsulated key is malformed, or the ciphertext does not decrypt to a key set
fn open_keys(
    domain: CheckpointDomain,
    secret: &<Kx as Kem>::PrivateKey,
    request: &[u8; 64],
    encapped_key: &[u8],
    ciphertext: &[u8],
) -> anyhow::Result<KeySet> {
    let encapped = <Kx as Kem>::EncappedKey::from_bytes(encapped_key)
        .map_err(|e| anyhow::anyhow!("the encapsulated key is malformed: {e}"))?;
    let plaintext = hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, Kx>(
        &OpModeR::Base,
        secret,
        &encapped,
        &hpke_info(domain),
        ciphertext,
        request,
    )
    .map_err(|e| anyhow::anyhow!("the keys do not decrypt: {e}"))?;
    serde_json::from_slice(&plaintext).context("the decrypted keys do not parse")
}

/// Writes `message` as a 4-byte big-endian length and its JSON, within `MESSAGE_TIMEOUT`, so a
/// peer that stops reading cannot hold the other side.
/// Error cases:
/// - the write fails or takes longer
async fn send(stream: &mut (impl AsyncWrite + Unpin), message: &impl Serialize) -> anyhow::Result<()> {
    let body = serde_json::to_vec(message).context("encoding a handover message")?;
    let len = u32::try_from(body.len()).ok().filter(|len| *len <= MAX_MESSAGE_BYTES).context("message too large")?;
    let write = async {
        stream.write_all(&len.to_be_bytes()).await?;
        stream.write_all(&body).await?;
        stream.flush().await
    };
    tokio::time::timeout(MESSAGE_TIMEOUT, write)
        .await
        .context("sending a handover message timed out")?
        .context("sending a handover message")
}

/// Reads one message `send` wrote.
/// Error cases:
/// - the read fails or ends early, the length is over `MAX_MESSAGE_BYTES`, or the JSON is not a `T`
async fn receive<T: DeserializeOwned>(stream: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<T> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).await.context("reading a handover message")?;
    let len = u32::from_be_bytes(len);
    ensure!(len <= MAX_MESSAGE_BYTES, "a handover message of {len} bytes is over the limit");
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body).await.context("reading a handover message")?;
    serde_json::from_slice(&body).context("a handover message does not parse")
}

/// Writes one raw frame `receive_bytes` reads: the protocol's length prefix, then the bytes
/// themselves, within `MESSAGE_TIMEOUT` so a peer that stops reading cannot hold the other side.
/// Error cases:
/// - `bytes` exceeds `MAX_SEGMENT_BYTES`, the write fails, or takes longer
async fn send_bytes(stream: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> anyhow::Result<()> {
    let len = u32::try_from(bytes.len()).ok().filter(|len| *len <= MAX_SEGMENT_BYTES).context("segment too large")?;
    let write = async {
        stream.write_all(&len.to_be_bytes()).await?;
        stream.write_all(bytes).await?;
        stream.flush().await
    };
    timeout(MESSAGE_TIMEOUT, write)
        .await
        .context("sending the log segment timed out")?
        .context("sending the log segment")
}

/// Reads one raw frame `send_bytes` wrote.
/// Error cases:
/// - the read fails or ends early, the length is over `MAX_SEGMENT_BYTES`, or the read stalls
async fn receive_bytes(stream: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<Vec<u8>> {
    timeout(REPLY_TIMEOUT, async {
        let mut len = [0u8; 4];
        stream.read_exact(&mut len).await.context("reading the log segment's length")?;
        let len = u32::from_be_bytes(len);
        ensure!(len <= MAX_SEGMENT_BYTES, "a handover log segment of {len} bytes is over the limit");
        let mut body = vec![0u8; len as usize];
        stream.read_exact(&mut body).await.context("reading the log segment")?;
        anyhow::Ok(body)
    })
    .await
    .context("reading the log segment timed out")?
}

/// Where the elected successor takes the keys: the operator the election in effect replaced, as
/// the vault's registry recorded when the election was made, with the handover endpoint and API
/// hostname its candidacy registered.
#[derive(Debug)]
pub(crate) struct Source {
    pub endpoint: String,
    pub api_hostname: String,
    pub release: [u8; 32],
    pub candidate: engine_core::types::AccountId,
}

/// Resolves the incumbent the election in effect replaced, reading the vault's registry: `None`
/// when no election has been made, so a release the vault names without one starts with new keys
/// instead. `Some` requires the recorded handover endpoint: an election whose incumbent never
/// registered one cannot hand over, which the runbook must prevent.
/// Error cases:
/// - the chain cannot be read, or an election's incumbent registered no handover endpoint
pub(crate) async fn resolve_source(vault: &impl CheckpointChain) -> anyhow::Result<Option<Source>> {
    let source = vault.handover_source().await?;
    if source.candidate == engine_core::types::AccountId([0; 20]) {
        return Ok(None);
    }
    ensure!(
        !source.handover_endpoint.is_empty(),
        "the election in effect replaced an operator whose candidacy registered no handover endpoint: \
         the runbook registers the operator in effect as a candidate with one before an election can \
         hand over"
    );
    Ok(Some(Source {
        endpoint: source.handover_endpoint,
        api_hostname: source.api_hostname,
        release: source.release,
        candidate: source.candidate,
    }))
}

/// While rotation is on, the certificate the enclave orders names the API hostname the registry
/// records for the operator in effect: a mismatch would certify a hostname the candidate did not
/// register, and clients of the registered one would find no attested endpoint. Without
/// `[api.tls.acme]` there is no certificate to check.
/// Error cases:
/// - the chain cannot be read, or the registered hostname differs from the config's
pub(crate) async fn check_operator_hostname(cfg: &NodeConfig, vault: &impl CheckpointChain) -> anyhow::Result<()> {
    let Some(domain) = cfg.api.tls.as_ref().map(|tls| tls.acme.domain.clone()) else { return Ok(()) };
    let registered = vault.operator_hostname().await?;
    ensure!(
        registered == domain,
        "the registry names {registered} as the operator in effect's API hostname, but api.tls.acme.domain is          {domain}: point the config at the registered hostname, or register the one the config serves"
    );
    Ok(())
}

/// Fetches the predecessor's published log into this node's log directory before the handover
/// connects: the completed segments the archive lists, checked against their listings. The final
/// segments arrive with the keys instead — the archive serves only completed segments, and this
/// fetches while the predecessor's API is still up. A fetch failure is not fatal: a local copy
/// from an earlier start may exist, and the strict tail checks at the handover decide; a fetched
/// copy that does not replay is fatal, surfacing a poisoned mirror before the predecessor stops.
/// Error cases:
/// - a fetched file differs from its listing, or the fetched copy does not replay
pub(crate) async fn sync_predecessor_log(cfg: &NodeConfig, source: &Source) -> anyhow::Result<()> {
    let section = cfg.tee.sealing.as_ref().and_then(|sealing| sealing.handover.as_ref());
    let mut origins = section.map(|handover| handover.mirrors.clone()).unwrap_or_default();
    origins.push(format!("https://{}", source.api_hostname));
    let client =
        crate::standby::archive::archive_client(section.is_some_and(|handover| handover.danger_accept_invalid_certs))?;
    let dir = cfg.node.log_dir.as_path();
    let synced = match if !dir.join(crate::log::MANIFEST).exists() {
        crate::standby::archive::bootstrap_archive(&client, &origins, dir).await
    } else {
        crate::standby::archive::refresh_archive(&client, &origins, dir).await
    } {
        Ok(()) => true,
        Err(e) => {
            warn!("fetching the predecessor's published log failed: {e:#}");
            false
        }
    };
    if synced && dir.join(crate::log::MANIFEST).exists() {
        // A poisoned cold start must surface now, not after the predecessor has stopped: archive
        // bytes were only hash-checked against the server's own manifest, and this replay, of the
        // whole fetched copy, is the authenticity check, as the standby's cold start does.
        crate::sequencer::replay_path(dir).context("the fetched log copy does not replay")?;
        info!("the fetched log copy replays to its tip");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
