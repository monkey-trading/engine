//! The successor's side: ask the running release for the keys, check its quote over them and that
//! the vault names their signing key, take the log segments past the restored tip, require the log
//! to reach the handed-over checkpoint, seal the keys for this release, and confirm.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use hpke::{Kem, Serializable};
use sha2::Digest as _;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::checkpoints::{CheckpointChain, View};
use crate::config::NodeConfig;
use crate::handover::evidence::{self, Quoting};
use crate::handover::{
    Challenge, Confirm, Kx, MESSAGE_TIMEOUT, REPLY_TIMEOUT, Reply, Request, confirm_digest, confirmed_digest,
    ensure_named, keys_report_data, open_keys, receive, receive_bytes, request_report_data, send,
};
use crate::log::{CheckpointDomain, LogEntry, Manifest, SegmentInfo, segment_seq};
use crate::tee::Attestor;
use crate::tee::keys::Head;
use crate::tee::sealing::{KeyStore, Release, Sealer};

/// This release's keys, taken from the release whose handover listener is at `address`, reading the
/// vault through `vault`, and sealed for this release in the vault's generation of it. The keys are
/// accepted only if the vault already names their signing key and this release at finality: a
/// handover moves the vault's signer, and never brings in a key a host-run enclave chose. The log
/// segments past the restored tip ride with the keys, each bound to its listing inside the
/// ciphertext: every segment is hash-checked and chained to the tip before the confirmation goes
/// out, and the last must reach the handed-over checkpoint. Without the predecessor's signed
/// acknowledgement the keys are not sealed, and the start fails under `handover_unconfirmed`: the
/// predecessor may still run.
/// Error cases:
/// - `[tee.sealing]` is missing, or this enclave cannot quote
/// - the predecessor cannot be reached in time, refuses, or its quote does not bind the keys it sent
/// - the keys do not decrypt, carry no final checkpoint, or name a signer the vault does not, or the
///   vault's current release is another one
/// - a segment differs from its listing, breaks the chain from the tip, or the log does not reach
///   the handed-over checkpoint
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
    let dir = cfg.node.log_dir.as_path();
    let log_tip = manifest_tip(dir);

    info!(predecessor = address, ?log_tip, "asking the running release for its keys");
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
    send(&mut stream, &Request { public_key: hex::encode(&public), config_sha256, log_tip, evidence }).await?;
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
    let mut keys = open_keys(domain, &secret, &request, &encapped_key, &ciphertext)?;
    let head = keys.predecessor_head.clone().context("the keys carry no final checkpoint")?;
    let tails = std::mem::take(&mut keys.handover_tails);
    let mut tip = log_tip;
    for listed in &tails {
        let bytes =
            receive_bytes(&mut stream).await.context("the predecessor named a log segment but sent no bytes")?;
        ensure_segment(dir, listed, &bytes, tip, &head)?;
        tip = Some(listed.last_seq);
    }
    if tails.is_empty() {
        info!("the predecessor handed no log segments over: the restored log already reaches the head");
    }
    ensure!(tip == Some(head.seq), "the log reaches seq {:?}, not the predecessor's final seq {}", tip, head.seq);
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
            "the predecessor did not acknowledge the handover, so it may still run: the keys are not \
             sealed here either, and this release must take them again before the predecessor stops: \
             {e:#}"
        );
        bail!("the predecessor did not acknowledge the handover: {e:#}");
    }
    Ok(store)
}

/// The last seq the restored copy holds: the last listed segment's end, if the manifest exists.
fn manifest_tip(dir: &Path) -> Option<u64> {
    Manifest::load(dir).ok().and_then(|manifest| manifest.segments.last().map(|listed| listed.last_seq))
}

/// Verifies one handed-over segment against its listing, the chain from `tip`, and the head, then
/// writes it durably: the staged file and manifest are synced before their names publish, and the
/// directory with them, so the confirmed handover survives a VM stop. An existing file is left
/// alone only when it is byte-identical; anything else refuses before the confirmation goes out.
/// Error cases:
/// - the listing is not canonical, the bytes differ from it, the chain breaks, or the file exists
///   with other bytes
/// - the directory, file, or manifest cannot be written or synced
fn ensure_segment(dir: &Path, listed: &SegmentInfo, bytes: &[u8], tip: Option<u64>, head: &Head) -> anyhow::Result<()> {
    ensure!(
        segment_seq(&listed.file) == Some(listed.first_seq),
        "the predecessor's segment listing is not canonical for seq {}",
        listed.first_seq
    );
    ensure!(
        bytes.len() as u64 == listed.bytes && hex::encode(sha2::Sha256::digest(bytes)) == listed.sha256,
        "the predecessor's segment {} differs from its listing",
        listed.file
    );
    ensure!(
        match tip {
            Some(tip) => listed.first_seq == tip + 1,
            // A successor with no local copy receives the whole log, which starts at seq 0.
            None => listed.first_seq == 0,
        },
        "the predecessor's segment {} does not continue the log at seq {}",
        listed.file,
        tip.map_or(0, |tip| tip + 1)
    );
    // The last handed-over segment must carry the final checkpoint itself.
    if listed.last_seq == head.seq {
        let mut tail: Option<(u64, String)> = None;
        for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
            if let Ok(LogEntry::Checkpoint(cp)) = serde_json::from_slice(line) {
                tail = Some((cp.seq, cp.root));
            }
        }
        ensure!(
            tail.as_ref().is_some_and(|(seq, root)| *seq == head.seq && *root == head.root),
            "the handed-over log's last checkpoint is not the predecessor's final checkpoint (seq {}, \
             root {})",
            head.seq,
            head.root
        );
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let target = dir.join(&listed.file);
    match std::fs::read(&target) {
        Ok(existing) if existing == bytes => {
            info!(file = %listed.file, "the predecessor's segment is already here");
        }
        Ok(_) => bail!("{} exists with other bytes: refusing to overwrite it", target.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let staged = dir.join(format!("{}.staging", listed.file));
            write_synced(&staged, bytes)?;
            std::fs::rename(&staged, &target).with_context(|| format!("linking {}", target.display()))?;
            append_manifest(dir, listed)?;
            sync_dir(dir)?;
            info!(file = %listed.file, first_seq = listed.first_seq, last_seq = listed.last_seq, "wrote the predecessor's segment");
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", target.display())),
    }
    Ok(())
}

/// Appends `listed` to the manifest unless it is already listed, staging and syncing first so a
/// crash leaves the old or the new whole file, never a torn one.
fn append_manifest(dir: &Path, listed: &SegmentInfo) -> anyhow::Result<()> {
    let manifest = dir.join(crate::log::MANIFEST);
    let text = match std::fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", manifest.display())),
    };
    if text.contains(&listed.file) {
        return Ok(());
    }
    let mut text = text;
    text.push_str(&serde_json::to_string(listed).context("encoding the segment listing")?);
    text.push('\n');
    let staged = dir.join(format!("{}.staging", crate::log::MANIFEST));
    write_synced(&staged, text.as_bytes())?;
    std::fs::rename(&staged, &manifest).with_context(|| format!("linking {}", manifest.display()))?;
    Ok(())
}

/// Writes `bytes` to `path` and syncs them to storage.
fn write_synced(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut file = OpenOptions::new().create(true).write(true).truncate(true).open(path)?;
    file.write_all(bytes).with_context(|| format!("writing {}", path.display()))?;
    file.sync_all().with_context(|| format!("syncing {}", path.display()))?;
    Ok(())
}

/// Syncs the directory entry naming everything written under it.
fn sync_dir(dir: &Path) -> anyhow::Result<()> {
    OpenOptions::new().read(true).open(dir)?.sync_all().with_context(|| format!("syncing {}", dir.display()))
}
