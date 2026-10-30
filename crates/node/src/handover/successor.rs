//! The successor's side: ask the running release for the keys, check its quote over them and that
//! the vault names their signing key, seal them for this release, and confirm.

use anyhow::{Context, bail, ensure};
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use hpke::{Kem, Serializable};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{error, info};

use crate::checkpoints::{CheckpointChain, View};
use crate::config::NodeConfig;
use crate::handover::evidence::{self, Quoting};
use crate::handover::{
    Challenge, Confirm, Kx, MESSAGE_TIMEOUT, REPLY_TIMEOUT, Reply, Request, confirm_digest, confirmed_digest,
    ensure_named, keys_report_data, open_keys, receive, request_report_data, send,
};
use crate::log::CheckpointDomain;
use crate::tee::Attestor;
use crate::tee::sealing::{KeyStore, Release, Sealer};

/// This release's keys, taken from the release whose handover listener is at `address`, reading the
/// vault through `vault`, and sealed for this release in the vault's generation of it. The keys are
/// accepted only if the vault already names their signing key and this release at finality: a
/// handover moves the vault's signer, and never brings in a key a host-run enclave chose. Without
/// the predecessor's signed acknowledgement the
/// keys stay sealed here, but the start fails under `handover_unconfirmed`: the predecessor may
/// still run.
/// Error cases:
/// - `[tee.sealing]` is missing, or this enclave cannot quote
/// - the predecessor cannot be reached in time, refuses, or its quote does not bind the keys it sent
/// - the keys do not decrypt, carry no final checkpoint, or name a signer the vault does not, or the
///   vault's current release is another one
/// - the keys cannot be sealed, or the predecessor does not acknowledge the confirmation
pub(crate) async fn take(
    cfg: &NodeConfig,
    address: &str,
    sealer: &dyn Sealer,
    attestor: &dyn Attestor,
    vault: &impl CheckpointChain,
) -> anyhow::Result<KeyStore> {
    let sealing = cfg.tee.sealing.as_ref().context("--predecessor needs [tee.sealing]")?;
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let platform = sealer.platform()?;
    let (secret, public) = Kx::gen_keypair();
    let public = public.to_bytes().to_vec();

    info!(predecessor = address, "asking the running release for its keys");
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
    let config_sha256 = hex::encode(cfg.file_sha256);
    send(&mut stream, &Request { public_key: hex::encode(&public), config_sha256, evidence }).await?;
    let reply =
        timeout(REPLY_TIMEOUT, receive(&mut stream)).await.context("the predecessor did not answer in time")??;
    let (encapped_key, ciphertext, evidence) = match reply {
        Reply::Keys { encapped_key, ciphertext, evidence } => (
            hex::decode(encapped_key).context("encapsulated key")?,
            hex::decode(ciphertext).context("ciphertext")?,
            evidence,
        ),
        Reply::Refused { reason } => bail!("the predecessor keeps its keys: {reason}"),
        Reply::Confirmed { .. } => bail!("the predecessor confirmed a handover it never made"),
    };
    // Before trusted time runs: the vault naming the key bounds what this accepts.
    let now = crate::clock::now_ms() / 1000;
    let peer = evidence::verify(
        cfg.tee.provider,
        &evidence,
        &keys_report_data(domain, &request, &encapped_key, &ciphertext),
        Some(now),
        None,
    )?;
    let keys = open_keys(domain, &secret, &request, &encapped_key, &ciphertext)?;
    let head = keys.predecessor_head.clone().context("the keys carry no final checkpoint")?;
    let enclave = keys.enclave_key()?;
    keys.tls_key()?;
    let finalized = vault.observe(View::Finalized, &[]).await?;
    let named = finalized.state.signer;
    ensure!(
        named == AccountId(enclave.address()),
        "the handed-over key {} is not the vault's signer {named}",
        enclave.address_hex()
    );
    let release = Release { launch_digest: platform.launch_digest, config_sha256: cfg.file_sha256 };
    let generation = ensure_named(vault.current_release(&finalized).await?, &release)?;
    let store = KeyStore::create(&sealing.dir, sealer, cfg.file_sha256, generation, keys)?;
    info!(
        enclave = %enclave.address_hex(),
        predecessor_launch_digest = %hex::encode(peer.launch_digest),
        generation = store.generation(),
        head_seq = head.seq,
        "sealed the keys the predecessor handed over"
    );

    let confirm = Confirm { signature: enclave.sign_digest(confirm_digest(domain, &request)) };
    let confirmed = async {
        send(&mut stream, &confirm).await?;
        match timeout(MESSAGE_TIMEOUT, receive(&mut stream)).await?? {
            Reply::Confirmed { signature } => {
                let signature: Signature = signature.parse().context("the acknowledgement is not a signature")?;
                let digest = B256::from(confirmed_digest(domain, &request));
                ensure!(
                    eip712::recover(&digest, &signature) == Some(AccountId(enclave.address())),
                    "the acknowledgement is not the signing key's"
                );
                Ok(())
            }
            Reply::Keys { .. } => bail!("the predecessor answered the confirmation with keys"),
            Reply::Refused { reason } => bail!("the predecessor refused the confirmation: {reason}"),
        }
    };
    if let Err(e) = confirmed.await {
        error!(
            alert = "handover_unconfirmed",
            predecessor = address,
            "the keys are sealed here but the predecessor did not acknowledge the handover, so it may still run: \
             start this release again only once the predecessor has stopped: {e:#}"
        );
        bail!("the predecessor did not acknowledge the handover: {e:#}");
    }
    Ok(store)
}
