//! The successor's side: ask the running release for the keys, check its quote over them and that
//! the vault names their signing key, take the log segment the final checkpoint is in, seal the
//! keys for this release, require the log to end at the handed-over checkpoint, and confirm.

use std::path::Path;

use anyhow::{Context, bail, ensure};
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use hpke::{Kem, Serializable};
use sha2::Digest as _;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{error, info};

use crate::checkpoints::{CheckpointChain, View};
use crate::config::NodeConfig;
use crate::handover::evidence::{self, Quoting};
use crate::handover::{
    Challenge, Confirm, Kx, MESSAGE_TIMEOUT, REPLY_TIMEOUT, Reply, Request, confirm_digest, confirmed_digest,
    ensure_named, keys_report_data, open_keys, receive, receive_bytes, request_report_data, send,
};
use crate::log::{CheckpointDomain, LogEntry, SegmentInfo, SegmentedLog};
use crate::tee::Attestor;
use crate::tee::keys::Head;
use crate::tee::sealing::{KeyStore, Release, Sealer};

/// This release's keys, taken from the release whose handover listener is at `address`, reading the
/// vault through `vault`, and sealed for this release in the vault's generation of it. The keys are
/// accepted only if the vault already names their signing key and this release at finality: a
/// handover moves the vault's signer, and never brings in a key a host-run enclave chose. The log
/// segment the final checkpoint is in rides with the keys, checked against the listing inside the
/// quote-bound message; the log is required to end at that checkpoint before the confirmation goes
/// out. Without the predecessor's signed acknowledgement the
/// keys stay sealed here, but the start fails under `handover_unconfirmed`: the predecessor may
/// still run.
/// Error cases:
/// - `[tee.sealing]` is missing, or this enclave cannot quote
/// - the predecessor cannot be reached in time, refuses, or its quote does not bind the keys it sent
/// - the keys do not decrypt, carry no final checkpoint, or name a signer the vault does not, or the
///   vault's current release is another one
/// - the log segment differs from its listing, or the log does not end at the final checkpoint
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
    let (encapped_key, ciphertext, evidence, segment) = match reply {
        Reply::Keys { encapped_key, ciphertext, evidence, segment } => (
            hex::decode(encapped_key).context("encapsulated key")?,
            hex::decode(ciphertext).context("ciphertext")?,
            evidence,
            segment,
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
    match segment {
        Some(listed) => {
            let bytes = receive_bytes(&mut stream)
                .await
                .context("the predecessor listed the final segment but sent no bytes")?;
            write_segment(&cfg.node.log_dir, &listed, &bytes)?;
        }
        None => info!("the predecessor handed no log segment over: its final checkpoint ends a listed segment"),
    }
    ensure_log_ends_at(&cfg.node.log_dir, &head).context("refusing to confirm the handover with an incomplete log")?;
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

/// Verifies `listed` against `bytes`, writes the segment into `dir`, and appends its listing to the
/// manifest unless it is already there: staged and linked, so a crash leaves the old or the new
/// whole file, never a torn one. A manifest that already lists the file is left alone, so a retried
/// handover is idempotent.
/// Error cases:
/// - the listing is not canonical for its first seq, or the bytes differ from it
/// - the directory, the file, or the manifest cannot be written
fn write_segment(dir: &Path, listed: &SegmentInfo, bytes: &[u8]) -> anyhow::Result<()> {
    ensure!(
        crate::log::segment_seq(&listed.file) == Some(listed.first_seq),
        "the predecessor's segment listing is not canonical for seq {}",
        listed.first_seq
    );
    ensure!(
        bytes.len() as u64 == listed.bytes && hex::encode(sha2::Sha256::digest(bytes)) == listed.sha256,
        "the predecessor's segment {} differs from its listing",
        listed.file
    );
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let staged = dir.join(format!("{}.staging", listed.file));
    std::fs::write(&staged, bytes).with_context(|| format!("writing {}", staged.display()))?;
    std::fs::rename(&staged, dir.join(&listed.file)).with_context(|| format!("linking {}", listed.file))?;
    let manifest = dir.join(crate::log::MANIFEST);
    let text = std::fs::read_to_string(&manifest).unwrap_or_default();
    if !text.contains(&listed.file) {
        let mut text = text;
        text.push_str(&serde_json::to_string(listed).context("encoding the segment listing")?);
        text.push('\n');
        let staged = dir.join(format!("{}.staging", crate::log::MANIFEST));
        std::fs::write(&staged, text).with_context(|| format!("writing {}", staged.display()))?;
        std::fs::rename(&staged, &manifest).with_context(|| format!("linking {}", manifest.display()))?;
    }
    info!(file = %listed.file, first_seq = listed.first_seq, last_seq = listed.last_seq, "wrote the predecessor's final segment");
    Ok(())
}

/// Requires the log in `dir` to end at the handed-over head: opened, so every listed segment is
/// hash-checked against its manifest listing, and the last entry is the final checkpoint itself. A
/// host that swapped the segment's bytes is caught here, before the confirmation goes out and while
/// the predecessor still serves.
/// Error cases:
/// - the log cannot be opened, ends at another seq, or its last checkpoint is not the head
fn ensure_log_ends_at(dir: &Path, head: &Head) -> anyhow::Result<()> {
    let mut tail: Option<(u64, String)> = None;
    let log = SegmentedLog::open(dir, |entry| {
        if let LogEntry::Checkpoint(cp) = entry {
            tail = Some((cp.seq, cp.root));
        }
        Ok(())
    })?;
    ensure!(
        log.last_seq() == Some(head.seq),
        "the log ends at seq {:?}, not the predecessor's final seq {}",
        log.last_seq(),
        head.seq
    );
    ensure!(
        tail.as_ref().is_some_and(|(seq, root)| *seq == head.seq && *root == head.root),
        "the log's last checkpoint is not the predecessor's final checkpoint (seq {}, root {})",
        head.seq,
        head.root
    );
    Ok(())
}
