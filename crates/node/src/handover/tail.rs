//! The tail-only handover. A successor that has followed the operator in effect holds a copy of the
//! log and a live replica of it (`standby::Follower`), so it asks only for the entries past its tip:
//! its request names the tip — the seq of its last command and the cumulative command-prefix digest
//! after it — and, as every request for the keys does, the newest protocol version its build reads,
//! all bound in its quote's report data. Before it stops, the predecessor refuses a successor that
//! reads only older versions than its log runs, tail or not, and checks the tip against the prefixes
//! its sequencer retains: a tip on its log gets the tail, which after the stop it reads from its log
//! and sends behind the keys, its range and SHA-256 inside the encrypted keys. A tip it does not find,
//! or a tail over its bound, is refused before the stop, saying why (`TailMiss`): the predecessor
//! keeps the keys and sequences on. A member's copy is written by its own writer, so its segment
//! boundaries need not be the holder's, and no whole segments are ever sent onto it.
//!
//! The successor reads the vault's signer and the schedule before it connects (`Prepared`), so
//! only the transfer and local checks fall in the window between the incumbent's last command and
//! its own first. It checks each entry against its copy's seal chain, applies it to the replica
//! before the copy takes it, requires the copy to end at the handed-over checkpoint and the replica
//! to hold the latest posted log head, seals the keys in the turn's generation, and confirms.

use std::path::Path;

use anyhow::{Context, ensure};
use engine_core::eip712::B256;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, Millis};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::net::TcpStream;
use tracing::info;

use crate::checkpoints::{CheckpointChain, View};
use crate::config::NodeConfig;
use crate::handover::successor::{Answered, ask, confirm};
use crate::handover::{padded, receive_bytes, send_bytes};
use crate::log::{CheckpointDomain, LogEntry, Manifest, Offset, offset_of};
use crate::standby::Follower;
use crate::tee::Attestor;
use crate::tee::keys::Head;
use crate::tee::sealing::{KeyStore, Release, Sealer};

/// The most bytes a tail may carry, which the successor holds in memory before it applies them.
pub(crate) const MAX_TAIL_BYTES: u64 = 512 << 20;
/// The most bytes one raw frame of a tail carries; a line may span frames.
pub(super) const TAIL_FRAME_BYTES: usize = 16 << 20;

/// A following successor's tip: the entries past it are all a tail handover carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tip {
    /// The seq of the last command the successor's replica applied.
    pub seq: u64,
    /// The cumulative command-prefix digest after it.
    pub link: B256,
}

/// The entries past a successor's tip a tail handover carries, as the keys name them: the raw frames
/// behind the keys must be exactly these bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TailListing {
    /// The seq of the tip's command, which the entries follow.
    pub after: u64,
    /// The seq of the last command among them: the final checkpoint's.
    pub last_seq: u64,
    /// How many entries, one JSON line each, the bytes hold.
    pub entries: u64,
    pub bytes: u64,
    /// Hex SHA-256 of the bytes.
    pub sha256: String,
    /// How many raw frames carry the bytes.
    pub frames: u32,
}

/// What a tail handover reads from the chain before it connects, so none of it falls in the window.
pub(crate) struct Prepared {
    /// The generation the keys are sealed in.
    pub generation: u64,
    /// The vault's signer at finality: the only key the handover may bring.
    pub signer: AccountId,
    /// The latest posted log head, with `[log_heads]`, which the copy must hold.
    pub posted: Option<crate::log_heads::Head>,
}

impl Prepared {
    /// Reads at finality the generation the vault, or the turn the finalised schedule names at
    /// `reached_ms`, names `release` in, and the vault's signer, and keeps `posted`, the latest posted
    /// log head.
    /// Error cases:
    /// - the vault or the schedule cannot be read, or neither names `release` (see `named_generation`)
    pub(crate) async fn read(
        vault: &impl CheckpointChain,
        release: &Release,
        reached_ms: Option<Millis>,
        posted: Option<crate::log_heads::Head>,
    ) -> anyhow::Result<Self> {
        let observation = vault.observe(View::Finalized, &[]).await?;
        let (generation, _) =
            crate::handover::turns::named_generation(vault, &observation, release, reached_ms).await?;
        Ok(Self { generation, signer: observation.state.signer, posted })
    }
}

/// A tail handover that failed once it began changing the follower's copy: the copy and its replica
/// may hold part of what it took, so the member rebuilds them before it tries again.
#[derive(Debug)]
pub(crate) struct CopyChanged;

impl std::fmt::Display for CopyChanged {
    /// What happened.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the handover failed once it began changing the copy")
    }
}

impl std::error::Error for CopyChanged {}

/// Why a predecessor refused a tail request before its stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TailMiss {
    /// The tip names another history than the predecessor's log holds at its seq, or a seq past its
    /// head: the copy diverged.
    NotOnLog,
    /// The tip is older than the prefixes the predecessor retains: the copy fell behind.
    Lagged,
    /// The tail past the tip may be over its bound: the copy fell far behind.
    OverBound,
}

/// A predecessor's refusal of a tail request, before its stop, and why.
#[derive(Debug)]
pub(crate) struct TailRefused {
    pub miss: TailMiss,
    pub reason: String,
}

impl std::fmt::Display for TailRefused {
    /// The predecessor's reason.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "the predecessor keeps its keys ({:?}): {}", self.miss, self.reason)
    }
}

impl std::error::Error for TailRefused {}

/// What a tail handover moved: the keys sealed in `store`, and the tail past the command at `after`,
/// `bytes` long, that extended the follower's copy and replica, which the node starts from, by
/// `entries`.
pub(crate) struct Taken {
    pub store: KeyStore,
    pub after: u64,
    pub entries: u64,
    pub bytes: u64,
}

/// The report data a tail request's quote binds: the connection's challenge, the successor's public
/// key and config hash, its tip and the newest protocol version its build `reads`, under a tag of its
/// own, for this deployment.
pub(crate) fn tail_report_data(
    domain: CheckpointDomain,
    challenge: &[u8; 32],
    public_key: &[u8],
    config_sha256: &[u8; 32],
    reads: &str,
    tip: &Tip,
) -> [u8; 64] {
    let mut hash = Sha256::new();
    hash.update(b"engine.handover.request.v2");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(challenge);
    hash.update(public_key);
    hash.update(config_sha256);
    hash.update(tip.seq.to_be_bytes());
    hash.update(tip.link.0);
    hash.update((reads.len() as u64).to_be_bytes());
    hash.update(reads.as_bytes());
    padded(hash.finalize().into())
}

/// The name the log gives `version` (`v51`).
pub(crate) fn version_name(version: ProtocolVersion) -> String {
    serde_json::to_value(version).ok().and_then(|name| name.as_str().map(str::to_owned)).unwrap_or_default()
}

/// Refuses a successor whose build reads only versions older than `log`, the version the log runs:
/// it could not replay the log it would take over.
/// Error cases:
/// - `reads` is not a version name, or names a version older than `log`
pub(crate) fn ensure_reads(log: ProtocolVersion, reads: &str) -> anyhow::Result<()> {
    let number = |name: &str| name.strip_prefix('v').and_then(|number| number.parse::<u32>().ok());
    let ours = version_name(log);
    let theirs =
        number(reads).with_context(|| format!("the successor names no protocol version it reads: {reads:?}"))?;
    ensure!(
        number(&ours).is_some_and(|ours| theirs >= ours),
        "the log runs protocol version {ours}, newer than {reads}, the newest the successor's build reads"
    );
    Ok(())
}

/// The entries the log in `dir` holds after the command at `after`, through the final checkpoint
/// `head`, as the JSON lines the log holds them in, with their listing; each listed segment they
/// come from is checked against its listing first.
/// Error cases:
/// - the manifest or a segment cannot be read, or a segment differs from its listing
/// - the log holds no command at `after`, does not end at `head`, or the tail is over its bound
pub(crate) fn read_tail(dir: &Path, after: u64, head: &Head) -> anyhow::Result<(TailListing, Vec<u8>)> {
    let manifest = Manifest::load(dir)?;
    let (mut bytes, mut entries, mut last_seq, mut found) = (Vec::new(), 0_u64, after, false);
    for listed in manifest.segments.iter().filter(|listed| listed.last_seq >= after) {
        let segment = std::fs::read(dir.join(&listed.file)).with_context(|| format!("reading {}", listed.file))?;
        ensure!(
            segment.len() as u64 == listed.bytes && hex::encode(Sha256::digest(&segment)) == listed.sha256,
            "the log segment {} differs from its listing",
            listed.file
        );
        let mut previous = None;
        for line in segment.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()) {
            let entry: LogEntry = serde_json::from_slice(line).context("a log line is not an entry")?;
            let offset = offset_of(previous, &entry)?;
            previous = Some(offset);
            found |= offset == Offset::start_of(after);
            if offset > Offset::start_of(after) {
                bytes.extend_from_slice(line);
                bytes.push(b'\n');
                entries += 1;
                last_seq = offset.seq;
                ensure!(bytes.len() as u64 <= MAX_TAIL_BYTES, "the tail past seq {after} is over its bound");
            }
        }
    }
    ensure!(found, "the log holds no command at seq {after}");
    ensure!(last_seq == head.seq, "the log ends at seq {last_seq}, not the final checkpoint's seq {}", head.seq);
    let listing = TailListing {
        after,
        last_seq,
        entries,
        bytes: bytes.len() as u64,
        sha256: hex::encode(Sha256::digest(&bytes)),
        frames: u32::try_from(bytes.chunks(TAIL_FRAME_BYTES).count()).context("too many tail frames")?,
    };
    Ok((listing, bytes))
}

/// Sends `bytes`, a tail, as the raw frames its listing counts.
/// Error cases:
/// - a write fails or takes longer than its bound
pub(crate) async fn send_tail(stream: &mut TcpStream, bytes: &[u8]) -> anyhow::Result<()> {
    for frame in bytes.chunks(TAIL_FRAME_BYTES) {
        send_bytes(stream, frame).await?;
    }
    Ok(())
}

/// Reads the tail `listing` names: its frames, which must hold exactly its bytes.
/// Error cases:
/// - the listing is over the bound, a frame is missing or over the listed bytes, or the bytes
///   differ from the listing
async fn receive_tail(stream: &mut TcpStream, listing: &TailListing) -> anyhow::Result<Vec<u8>> {
    ensure!(listing.bytes <= MAX_TAIL_BYTES, "the predecessor's tail of {} bytes is over the bound", listing.bytes);
    let mut bytes = Vec::with_capacity(usize::try_from(listing.bytes)?);
    for _ in 0..listing.frames {
        let frame = receive_bytes(stream).await.context("the predecessor named a tail but sent no bytes")?;
        bytes.extend_from_slice(&frame);
        ensure!(bytes.len() as u64 <= listing.bytes, "the predecessor's tail runs past its listing");
    }
    ensure!(
        bytes.len() as u64 == listing.bytes && hex::encode(Sha256::digest(&bytes)) == listing.sha256,
        "the predecessor's tail differs from its listing inside the keys"
    );
    Ok(bytes)
}

/// Takes the keys from the predecessor whose handover listener is at `address`, asking for the
/// entries past `follower`'s tip alone, with what `prepared` read before connecting. The tail must
/// extend the copy's seal chain entry by entry and replay onto its replica, end at the handed-over
/// checkpoint, and leave the replica holding the latest posted log head. The keys are then sealed in
/// the prepared generation and the handover confirmed.
/// Error cases:
/// - `[tee.sealing]` is missing, the copy holds no command, or this enclave cannot quote
/// - the predecessor cannot be reached, refuses (a tail it would not serve, `TailRefused`), or its
///   quote does not bind the keys it sent, or sends no tail
/// - the keys carry no final checkpoint, or name a signer the vault does not
/// - the tail differs from its listing, does not extend the copy, does not replay, does not end at
///   the final checkpoint, or the replica does not hold the latest posted head (`CopyChanged`)
/// - the keys cannot be sealed, or the predecessor does not acknowledge the confirmation
pub(crate) async fn take_tail(
    cfg: &NodeConfig,
    address: &str,
    sealer: &dyn Sealer,
    attestor: &dyn Attestor,
    prepared: &Prepared,
    follower: &mut Follower,
) -> anyhow::Result<Taken> {
    let sealing = cfg.tee.sealing.as_ref().context("a tail handover needs [tee.sealing]")?;
    let domain = CheckpointDomain { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse()? };
    let platform = sealer.platform()?;
    let (seq, link) = follower.tip().context("the follower's copy holds no command to name as its tip")?;
    let tip = Tip { seq, link };
    follower.sync()?;
    // The tip alone names what the copy holds: its segment files are its own writer's.
    let Answered { mut stream, request, mut keys, peer } =
        ask(cfg, address, attestor, &platform, None, Some(&tip)).await?;
    let head = keys.predecessor_head.clone().context("the keys carry no final checkpoint")?;
    let tails = std::mem::take(&mut keys.handover_tails);
    let listing = keys.handover_tail.take();
    let enclave = keys.enclave_key()?;
    keys.tls_key()?;
    ensure!(
        prepared.signer == AccountId(enclave.address()),
        "the handed-over key {} is not the vault's signer {}",
        enclave.address_hex(),
        prepared.signer
    );
    let listing = listing.context("the predecessor answered a tail request with no tail")?;
    ensure!(tails.is_empty(), "the predecessor sent whole segments for a tail request");
    ensure!(listing.after == tip.seq, "the predecessor's tail follows seq {}, not the tip {}", listing.after, tip.seq);
    let bytes = receive_tail(&mut stream, &listing).await?;
    let entries = follower.take_tail(tip.seq, &bytes).map_err(|e| e.context(CopyChanged))?;
    ensure_tail_head(follower, &head, prepared.posted).map_err(|e| e.context(CopyChanged))?;
    info!(entries, bytes = listing.bytes, after = tip.seq, head_seq = head.seq, "took the tail past the copy's tip");
    let store = KeyStore::create(&sealing.dir, sealer, cfg.file_sha256, prepared.generation, keys)?;
    info!(
        enclave = %enclave.address_hex(),
        predecessor_launch_digest = %hex::encode(peer.launch_digest),
        generation = store.generation(),
        head_seq = head.seq,
        "sealed the keys the predecessor handed over"
    );
    confirm(&mut stream, domain, &request, &enclave, address).await?;
    Ok(Taken { store, after: tip.seq, entries, bytes: listing.bytes })
}

/// Requires the follower's replica to end at the handed-over checkpoint `head` and, with `posted`,
/// to hold the latest posted log head: the same seq and link.
/// Error cases:
/// - the replica ends elsewhere, or holds another link at the posted seq, or none
fn ensure_tail_head(follower: &Follower, head: &Head, posted: Option<crate::log_heads::Head>) -> anyhow::Result<()> {
    let replica = follower.replica();
    ensure!(
        replica.last_seq() == Some(head.seq)
            && replica.last_checkpoint().is_some_and(|cp| cp.seq == head.seq && cp.root == head.root),
        "the copy does not end at the predecessor's final checkpoint (seq {}, root {})",
        head.seq,
        head.root
    );
    if let Some(posted) = posted {
        let held = replica.link_at(posted.seq).map(|held| held.link);
        ensure!(
            held == Some(posted.link),
            "the copy does not hold the latest posted log head (seq {}, link {}): it holds {held:?} there",
            posted.seq,
            posted.link
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
