//! Authenticated complete history heads. A head supplies no engine state or custody authority.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use engine_core::types::Millis;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sha3::{Digest as _, Keccak256};
use tracing::warn;

use crate::authenticated::{CODEC, Codec, Store};
use crate::config::SnapshotSection;
use crate::history::authenticated::{Head, Index};
use crate::log::CheckpointDomain;
use crate::snapshot::{Prune, reconcile_managed, sync_directory};
use crate::tee::EnclaveKey;

/// Two durable metadata pointers bound interrupted publication, independently of event retention.
const RETAINED_HEADS: usize = 2;

/// A namespace-qualified packed database generation; filenames never come from host strings.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub(crate) struct StoreId(pub [u8; 32]);

/// A complete indexed command prefix, signed only after its store commit has been synced.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeadStatement {
    pub format: u32,
    pub codec: Codec,
    pub publication: u64,
    pub chain_id: u64,
    pub vault: AccountId,
    pub store: StoreId,
    pub head: Head,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Envelope {
    payload: String,
    signature: String,
}

impl HeadStatement {
    /// Matches the head against the same strict replay that materialized engine and custody work.
    /// Error cases:
    /// - sequence, time, retained-history policy, or complete command/event prefix differs
    pub(crate) fn verify_prefix(
        &self,
        seq: u64,
        ts: Millis,
        command_prefix: [u8; 32],
        retention_ms: Millis,
    ) -> anyhow::Result<()> {
        ensure!(self.head.last_seq() == Some(seq) && self.head.last_ts() == Some(ts), "history head cursor mismatch");
        ensure!(self.head.command_prefix == command_prefix, "history head command prefix mismatch");
        ensure!(self.head.retention_ms == retention_ms, "history head retention differs from config");
        Ok(())
    }
}

impl StoreId {
    /// Names the one app-owned database file for this generation.
    pub(crate) fn path(self, directory: &Path) -> PathBuf {
        directory.join(format!("authenticated-{}.sqlite", hex::encode(self.0)))
    }

    /// Separates history and catalog keys without delegating namespace choice to disk rows.
    pub(crate) fn history_namespace(self) -> [u8; 32] {
        namespace(b"history", self)
    }

    /// The catalog has its own immutable root lineage in the same packed store.
    pub(crate) fn catalog_namespace(self) -> [u8; 32] {
        namespace(b"catalog", self)
    }
}

/// Signs a proved complete head and publishes it without replacing an earlier unsynced pointer.
/// Error cases:
/// - head verification, allocation, encoding, signing preparation, sync, rename, or retention fails
#[cfg(test)]
pub(crate) fn write(
    cfg: &SnapshotSection,
    domain: CheckpointDomain,
    store_id: StoreId,
    store: &Store,
    head: Head,
    key: &EnclaveKey,
) -> anyhow::Result<HeadStatement> {
    write_with_sync(cfg, domain, store_id, store, head, key, &sync_directory)
}

/// Reclaims head staging files and heads beyond retention before another head is staged. Runs
/// under the lifecycle lock.
/// Error cases:
/// - listing, staging cleanup, directory sync or journaled pruning fails
pub(crate) fn prepare(cfg: &SnapshotSection, prune: &mut impl Prune) -> anyhow::Result<()> {
    reclaim(cfg, prune)
}

/// A signed head and its synced staging file, renamed into place by `publish`.
pub(crate) struct StagedHead {
    pub statement: HeadStatement,
    file: super::Staged,
}

impl StagedHead {
    /// The certificate a paired cache embeds.
    /// Error cases:
    /// - encoding or signing fails
    pub(crate) fn certificate(&self, key: &EnclaveKey) -> anyhow::Result<Envelope> {
        certificate(&self.statement, key)
    }

    /// Renames the head into place, syncs the directory and prunes heads beyond retention.
    /// Error cases:
    /// - as `Staged::publish`
    pub(crate) fn publish(self, prune: &mut impl Prune) -> anyhow::Result<HeadStatement> {
        self.file.publish(prune)?;
        Ok(self.statement)
    }
}

/// Proves, signs and syncs a head's staging file. Only the history owner stages heads and
/// inventory ignores staging names, so this runs outside the lifecycle lock; `prepare` runs first.
/// Error cases:
/// - proof, listing, encoding, signing, size limit, or the staging write or sync fails
pub(crate) fn stage(
    cfg: &SnapshotSection,
    domain: CheckpointDomain,
    store_id: StoreId,
    store: &Store,
    head: Head,
    key: &EnclaveKey,
) -> anyhow::Result<StagedHead> {
    let paths = candidates_readonly(cfg)?;
    stage_with(Writer { cfg, domain, store_id, store, key }, head, &paths)
}

struct Writer<'a> {
    cfg: &'a SnapshotSection,
    domain: CheckpointDomain,
    store_id: StoreId,
    store: &'a Store,
    key: &'a EnclaveKey,
}

/// Authenticates an embedded history certificate with independent configuration pins.
/// Error cases:
/// - signature, codec, deployment, namespace, or complete progress validation fails
pub(crate) fn verify_certificate(
    cfg: &SnapshotSection,
    envelope: &Envelope,
    domain: CheckpointDomain,
) -> anyhow::Result<HeadStatement> {
    verify_envelope(envelope, domain, &cfg.trusted_signers)
}

/// Uses the same bounded publication/durability seam as engine snapshots.
/// Error cases:
/// - as `write`, including the supplied directory-sync failure
#[cfg(test)]
fn write_with_sync(
    cfg: &SnapshotSection,
    domain: CheckpointDomain,
    store_id: StoreId,
    store: &Store,
    head: Head,
    key: &EnclaveKey,
    sync_dir: &impl Fn(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<HeadStatement> {
    publish_head(Writer { cfg, domain, store_id, store, key }, head, sync_dir, &mut super::PlainPrune)
}

/// Serializes one proven head under the same fault-injected durability sequence as snapshots.
/// Error cases:
/// - proof, allocation, signing, file/directory sync or per-artifact retirement fails
#[cfg(test)]
fn publish_head(
    writer: Writer<'_>,
    head: Head,
    sync_dir: &impl Fn(&Path) -> anyhow::Result<()>,
    prune: &mut impl Prune,
) -> anyhow::Result<HeadStatement> {
    let paths = reconcile_managed(&writer.cfg.dir, RETAINED_HEADS, is_head_name, sync_dir, prune)?;
    let staged = stage_with(writer, head, &paths)?;
    staged.file.publish_with(sync_dir, prune, |_| {})?;
    Ok(staged.statement)
}

/// Proves the head, numbers it after the newest of `paths` that verifies, and stages it signed.
/// Error cases:
/// - as `stage`
fn stage_with(writer: Writer<'_>, head: Head, paths: &[PathBuf]) -> anyhow::Result<StagedHead> {
    let Writer { cfg, domain, store_id, store, key } = writer;
    ensure!(head.last_seq().is_some(), "cannot certify incomplete empty history progress");
    ensure!(head.root.namespace == store_id.history_namespace(), "history head namespace mismatch");
    let _pin = store.lease(&head.root)?;
    Index::resume(store.clone(), head.clone()).context("proving complete history head")?;
    let mut signers = cfg.trusted_signers.clone();
    signers.push(AccountId(key.address()));
    let mut publication = 0;
    for path in paths {
        if let Ok(previous) = read_with_signers(cfg, path, domain, &signers) {
            publication = publication.max(previous.publication);
        }
    }
    let publication = publication.checked_add(1).context("history head publication counter overflow")?;
    let statement = HeadStatement {
        format: 2,
        codec: CODEC,
        publication,
        chain_id: domain.chain_id,
        vault: domain.vault,
        store: store_id,
        head,
    };
    let bytes = serde_json::to_vec(&certificate(&statement, key)?).context("encoding history head envelope")?;
    ensure!(bytes.len() <= cfg.max_bytes.get(), "history head exceeds snapshots.max_bytes");
    fs::create_dir_all(&cfg.dir).context("creating history head directory")?;
    let file = super::Staged::create(cfg.dir.join(name(publication)), RETAINED_HEADS, is_head_name, |file| {
        file.write_all(&bytes).context("writing history head")?;
        file.sync_all().context("syncing history head")
    })?;
    Ok(StagedHead { statement, file })
}

/// Reclaims interrupted head staging files and heads beyond retention.
/// Error cases:
/// - listing, staging cleanup, directory sync, or journaled pruning fails
pub(super) fn reclaim(cfg: &SnapshotSection, prune: &mut impl Prune) -> anyhow::Result<()> {
    reconcile_managed(&cfg.dir, RETAINED_HEADS, is_head_name, &sync_directory, prune).map(|_| ())
}

/// Lists head candidates during preflight without changing a file or recovering SQLite journals.
/// Error cases:
/// - an existing directory cannot be read
pub(crate) fn candidates_readonly(cfg: &SnapshotSection) -> anyhow::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(&cfg.dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("listing history head certificates"),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.context("reading history head candidate")?;
        if entry.file_name().to_str().is_some_and(is_head_name) {
            paths.push(entry.path());
        }
    }
    paths.sort_unstable_by(|a, b| b.cmp(a));
    Ok(paths)
}

/// Verifies provenance; the caller must still match its command prefix and prove stored metadata.
/// Error cases:
/// - unreadable, oversized, malformed, untrusted, or deployment/namespace-inconsistent data
pub(crate) fn read(cfg: &SnapshotSection, path: &Path, domain: CheckpointDomain) -> anyhow::Result<HeadStatement> {
    read_with_signers(cfg, path, domain, &cfg.trusted_signers)
}

/// Bounds decoding and checks externally provisioned authority before decoding the head payload.
/// Error cases:
/// - as `read`
fn read_with_signers(
    cfg: &SnapshotSection,
    path: &Path,
    domain: CheckpointDomain,
    signers: &[AccountId],
) -> anyhow::Result<HeadStatement> {
    let mut bytes = Vec::new();
    File::open(path)?.take((cfg.max_bytes.get() as u64).saturating_add(1)).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= cfg.max_bytes.get(), "history head exceeds snapshots.max_bytes");
    let envelope: Envelope = serde_json::from_slice(&bytes).context("decoding history head envelope")?;
    let statement = verify_envelope(&envelope, domain, signers)?;
    ensure!(
        path.file_name().and_then(|value| value.to_str()) == Some(name(statement.publication).as_str()),
        "history head filename mismatch"
    );
    Ok(statement)
}

/// Checks signature authority before decoding any root or namespace supplied by disk.
/// Error cases:
/// - an invalid signature or incompatible complete-head statement
fn verify_envelope(
    envelope: &Envelope,
    domain: CheckpointDomain,
    signers: &[AccountId],
) -> anyhow::Result<HeadStatement> {
    verify_signature(envelope, domain, signers)?;
    let statement: HeadStatement = serde_json::from_str(&envelope.payload).context("decoding history head")?;
    ensure!(statement.format == 2 && statement.codec == CODEC, "unsupported history head format or proof codec");
    ensure!(
        statement.chain_id == domain.chain_id && statement.vault == domain.vault,
        "history head deployment mismatch"
    );
    ensure!(statement.head.root.namespace == statement.store.history_namespace(), "history head namespace mismatch");
    ensure!(statement.head.last_seq().is_some(), "history head has no complete command");
    Ok(statement)
}

/// Distinguishes unsupported authenticated metadata from unauthenticated optional cache debris.
/// Error cases:
/// - bounded IO, envelope, signature or independent provenance validation fails
pub(super) fn verified_payload(cfg: &SnapshotSection, path: &Path, domain: CheckpointDomain) -> anyhow::Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?.take((cfg.max_bytes.get() as u64).saturating_add(1)).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= cfg.max_bytes.get(), "history head exceeds snapshots.max_bytes");
    let envelope: Envelope = serde_json::from_slice(&bytes).context("decoding history head envelope")?;
    verify_signature(&envelope, domain, &cfg.trusted_signers)?;
    Ok(envelope.payload)
}

/// Recovers authority before any root or progress supplied by disk is decoded.
/// Error cases:
/// - malformed or independently untrusted signature
fn verify_signature(envelope: &Envelope, domain: CheckpointDomain, signers: &[AccountId]) -> anyhow::Result<()> {
    let signature: Signature = envelope.signature.parse().context("history head signature")?;
    let signer = eip712::recover(&B256::from(digest(domain, envelope.payload.as_bytes())), &signature)
        .context("invalid history head signature")?;
    ensure!(signers.contains(&signer), "history head signer is not independently trusted");
    Ok(())
}

/// Uses a separate signature domain while preserving the complete head's exact payload bytes.
/// Error cases:
/// - statement encoding fails
fn certificate(statement: &HeadStatement, key: &EnclaveKey) -> anyhow::Result<Envelope> {
    let payload = serde_json::to_string(statement).context("encoding history head")?;
    let domain = CheckpointDomain { chain_id: statement.chain_id, vault: statement.vault };
    let signature = key.sign_digest(digest(domain, payload.as_bytes()));
    Ok(Envelope { payload, signature })
}

/// Identifies one rejected optional pointer without granting it engine or freshness authority.
pub(crate) fn rejected(path: &Path, error: &anyhow::Error) {
    warn!(alert = "history_head_rejected", path = %path.display(), "ignoring history head: {error:#}");
}

/// Commits the role and generation using the workspace SHA-256 implementation.
fn namespace(role: &[u8], store: StoreId) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"engine.authenticated-store.namespace.v1");
    hash.update((role.len() as u64).to_be_bytes());
    hash.update(role);
    hash.update(store.0);
    hash.finalize().into()
}

/// Separates head signatures from checkpoints, snapshots, receipts, and other deployments.
fn digest(domain: CheckpointDomain, payload: &[u8]) -> [u8; 32] {
    let mut hash = Keccak256::new();
    hash.update(b"engine.history.head.v1");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(payload);
    hash.finalize().into()
}

/// Lexical ordering follows the signed publication counter, including same-sequence maintenance.
fn name(publication: u64) -> String {
    format!("history-head-{publication:020}.json")
}

/// Recognizes only metadata pointers owned by this module.
fn is_head_name(file: &str) -> bool {
    file.strip_prefix("history-head-")
        .and_then(|value| value.strip_suffix(".json"))
        .is_some_and(|digits| digits.len() == 20 && digits.bytes().all(|byte| byte.is_ascii_digit()))
}

#[cfg(test)]
mod tests;
