//! Moving the delivered secrets with an approved upgrade. Reading the vault can take a delivered key,
//! so the new release asks for them before anything else, and the running release, which can read
//! the vault, checks that the vault names the asker. Sequencing does not stop: the keys follow in
//! their own handover, once the new release can read the vault for itself.
//!
//! The new release cannot check who answers, since it cannot read the vault yet, so it trusts no
//! answer on the answerer's word: the bundle keeps the operator's signature, which must be the key
//! its own attested config names. A host that answers with an enclave of its own can only replay an
//! older bundle that key signed, as restoring an older sealed file can (`tee::secrets`). A bundle
//! signed before a rotation that config does not reflect is refused, and the operator delivers again.
//!
//! An election moves nothing. While the registry has rotation on, the vault's release changes only by
//! election or promotion, and the operator it names brings its own vendor accounts and provider keys
//! (#359): the running release refuses, and the elected enclave holds its own bundle.

use anyhow::{Context, bail, ensure};
use engine_core::types::AccountId;
use hpke::{Kem, Serializable};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::info;

use crate::config::{NamedSecret, NodeConfig};
use crate::handover::evidence::{self, Quoting};
use crate::handover::{Challenge, Kx, MESSAGE_TIMEOUT, REPLY_TIMEOUT, Reply, Request, padded, receive, send};
use crate::log::CheckpointDomain;
use crate::secrets::SignedBundle;
use crate::secrets::delivery;
use crate::tee::Attestor;
use crate::tee::sealing::Sealer;

/// The report data a new release's request for the secrets binds, under its own tag, so a host cannot
/// turn it into a request for the keys, which would stop sequencing, or the reverse.
pub(super) fn request_report_data(
    domain: CheckpointDomain,
    challenge: &[u8; 32],
    public_key: &[u8],
    config_sha256: &[u8; 32],
) -> [u8; 64] {
    let mut hash = Sha256::new();
    hash.update(b"engine.handover.secrets-request.v1");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(challenge);
    hash.update(public_key);
    hash.update(config_sha256);
    padded(hash.finalize().into())
}

/// The report data the running release's quote binds: the request it answers, and what it sent.
pub(super) fn reply_report_data(
    domain: CheckpointDomain,
    request: &[u8; 64],
    encapped_key: &[u8],
    ciphertext: &[u8],
) -> [u8; 64] {
    let mut hash = Sha256::new();
    hash.update(b"engine.handover.secrets.v1");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(request);
    hash.update(Sha256::digest([encapped_key, ciphertext].concat()));
    padded(hash.finalize().into())
}

/// HPKE's `info` for a moved bundle.
pub(super) fn hpke_info(domain: CheckpointDomain) -> Vec<u8> {
    [b"engine.handover.secrets.v1".as_slice(), &domain.chain_id.to_be_bytes(), &domain.vault.0].concat()
}

/// This release's secrets, from the release whose handover listener is at `address`: the bundle the
/// operator signed, whole, which may hold more than this config reads.
/// Error cases:
/// - the predecessor cannot be reached in time or refuses, or its quote does not bind what it sent
/// - what it sent does not decrypt, is not signed by `operator`, or fails `Bundle::check_values`
pub(crate) async fn fetch_secrets(
    cfg: &NodeConfig,
    address: &str,
    sealer: &dyn Sealer,
    attestor: &dyn Attestor,
    operator: AccountId,
    named: &[NamedSecret],
) -> anyhow::Result<SignedBundle> {
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let platform = sealer.platform()?;
    let (secret, public) = Kx::gen_keypair();
    let public = public.to_bytes().to_vec();
    let mut stream = timeout(MESSAGE_TIMEOUT, TcpStream::connect(address))
        .await
        .context("connecting to the predecessor timed out")?
        .with_context(|| format!("connecting to the predecessor at {address}"))?;
    let challenge: Challenge =
        timeout(MESSAGE_TIMEOUT, receive(&mut stream)).await.context("the predecessor sent no challenge in time")??;
    let challenge: [u8; 32] = hex::decode(&challenge.nonce)
        .ok()
        .and_then(|nonce| nonce.try_into().ok())
        .context("the predecessor's challenge is not 32 bytes of hex")?;
    let request = request_report_data(domain, &challenge, &public, &cfg.file_sha256);
    let evidence = evidence::produce(&Quoting::from(&cfg.tee), attestor, &platform, request).await?;
    let message = Request {
        public_key: hex::encode(&public),
        config_sha256: hex::encode(cfg.file_sha256),
        log_tip: None,
        tail: None,
        reads: String::new(),
        secrets_only: true,
        evidence,
    };
    send(&mut stream, &message).await?;
    let reply =
        timeout(REPLY_TIMEOUT, receive(&mut stream)).await.context("the predecessor did not answer in time")??;
    let (encapped_key, ciphertext, evidence) = match reply {
        Reply::Secrets { encapped_key, ciphertext, evidence } => (
            hex::decode(encapped_key).context("encapsulated key")?,
            hex::decode(ciphertext).context("ciphertext")?,
            evidence,
        ),
        Reply::Refused { reason } | Reply::Missed { reason, .. } => {
            bail!("the predecessor keeps its secrets: {reason}")
        }
        Reply::Keys { .. } | Reply::Confirmed { .. } => {
            bail!("the predecessor answered a request for secrets with keys")
        }
    };
    // At the host's time, before trusted time runs: the operator's signature bounds what this accepts.
    let now = crate::clock::now_ms() / 1000;
    let bound = reply_report_data(domain, &request, &encapped_key, &ciphertext);
    evidence::verify(cfg.tee.provider, &evidence, &bound, Some(now), None)?;
    let plaintext = delivery::open(&hpke_info(domain), &secret, &request, &encapped_key, &ciphertext)
        .context("the moved bundle does not decrypt")?;
    let signed = SignedBundle::from_bytes(&plaintext)?;
    let signer = signed.signer(domain)?;
    ensure!(signer == operator, "the moved bundle is signed by {signer}, not this config's operator {operator}");
    signed.bundle.check_values(named)?;
    info!(version = signed.bundle.version, "took the delivered secrets from the running release");
    Ok(signed)
}
