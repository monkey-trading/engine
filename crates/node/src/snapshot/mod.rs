//! Signed recovery caches. Authority comes from configured attested signer pins, never the disk.

mod context;
pub(crate) use context::{RecoveryBase, RecoveryContext};
pub(crate) mod catalog_heads;
pub(crate) mod generations;
pub(crate) mod head;
pub(crate) mod lifecycle;
pub(crate) mod paired;
pub(crate) mod payloads;
pub(crate) mod pins;
mod references;
pub(crate) mod sealing;

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use engine_core::Engine;
use engine_core::eip712::{self, B256, Signature};
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;
use serde::{Deserialize, Deserializer, Serialize};
use sha3::{Digest, Keccak256};
use tracing::warn;

use crate::config::SnapshotSection;
use crate::log::{Checkpoint, Offset};
use crate::sequencer::GenesisProgress;
use crate::tee::EnclaveKey;

/// Replay facts carried across a checkpoint; every field is authenticated with the engine bytes.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Progress {
    pub genesis: GenesisProgress,
    pub activations: Vec<(ProtocolVersion, u64)>,
    pub checkpoints: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub schema: u32,
    pub chain_id: u64,
    pub vault: AccountId,
    pub checkpoint: Checkpoint,
    pub offset: Offset,
    pub engine: String,
    pub progress: Progress,
    pub context: RecoveryContext,
    #[serde(deserialize_with = "required_pair")]
    pub pair: Option<paired::Pair>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    payload: String,
    signature: String,
}

/// Atomically publishes a synced cache after its checkpoint is synced, then prunes older caches.
/// Error cases:
/// - encoding, writing, syncing, renaming, or pruning fails, or the size limit is exceeded
#[cfg(test)]
pub(crate) fn write(cfg: &SnapshotSection, snapshot: &Snapshot, key: &EnclaveKey) -> anyhow::Result<()> {
    ensure!(snapshot.pair.is_none(), "paired snapshots require managed generation retirement");
    write_with_sync(cfg, snapshot, key, &sync_directory)
}

/// Reclaims the cache family's staging files and the caches beyond retention before another cache
/// is staged, so a failed cleanup refuses the allocation. Runs under the lifecycle lock.
/// Error cases:
/// - listing, staging cleanup, directory sync or journaled pruning fails
pub(crate) fn prepare(cfg: &SnapshotSection, prune: &mut impl Prune) -> anyhow::Result<()> {
    reconcile_managed(&cfg.dir, cfg.retain.get(), is_snapshot_name, &sync_directory, prune).map(drop)
}

/// Encodes, signs and syncs a cache's staging file. Inventory ignores staging names and only this
/// owner stages caches, so this runs outside the lifecycle lock; `prepare` runs first.
/// Error cases:
/// - encoding, the size limit, or the staging write or sync fails
pub(crate) fn stage(cfg: &SnapshotSection, snapshot: &Snapshot, key: &EnclaveKey) -> anyhow::Result<Staged> {
    let payload = serde_json::to_string(snapshot).context("encoding snapshot")?;
    let signature = key.sign_digest(digest(snapshot.chain_id, snapshot.vault, payload.as_bytes()));
    let bytes = serde_json::to_vec(&Envelope { payload, signature }).context("encoding snapshot envelope")?;
    ensure!(bytes.len() <= cfg.max_bytes.get(), "snapshot exceeds snapshots.max_bytes");
    fs::create_dir_all(&cfg.dir).context("creating snapshot directory")?;
    Staged::create(cfg.dir.join(name(snapshot.offset)), cfg.retain.get(), is_snapshot_name, |file| {
        file.write_all(&bytes).context("writing snapshot")?;
        file.sync_all().context("syncing snapshot")
    })
}

/// A synced staging file that `publish` renames into place under the lifecycle lock. Dropping it
/// unpublished removes it.
pub(crate) struct Staged {
    staged: PathBuf,
    target: PathBuf,
    retain: usize,
    belongs: fn(&str) -> bool,
    done: bool,
}

impl Staged {
    /// Creates and syncs `<target>.tmp`, removing it again if the write or sync fails.
    /// Error cases:
    /// - creation, the supplied write/sync operation, or cleanup fails
    fn create(
        target: PathBuf,
        retain: usize,
        belongs: fn(&str) -> bool,
        write_synced: impl FnOnce(&mut File) -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let mut staged = target.clone().into_os_string();
        staged.push(".tmp");
        let staged = PathBuf::from(staged);
        write_staging(&staged, write_synced)?;
        Ok(Self { staged, target, retain, belongs, done: false })
    }

    /// Renames the file into place, syncs the directory, and prunes its family beyond retention.
    /// Error cases:
    /// - the rename, its cleanup, the directory sync or journaled pruning fails
    pub(crate) fn publish(self, prune: &mut impl Prune) -> anyhow::Result<()> {
        self.publish_with(&sync_directory, prune, |_| {})
    }

    /// As `publish`, handing the renamed file to `renamed` before pruning, which reads the family.
    /// Error cases:
    /// - as `publish`
    pub(crate) fn publish_then(self, prune: &mut impl Prune, renamed: impl FnOnce(&Path)) -> anyhow::Result<()> {
        self.publish_with(&sync_directory, prune, renamed)
    }

    /// As `publish_then`, with the supplied directory sync.
    fn publish_with(
        mut self,
        sync_dir: &impl Fn(&Path) -> anyhow::Result<()>,
        prune: &mut impl Prune,
        renamed: impl FnOnce(&Path),
    ) -> anyhow::Result<()> {
        self.done = true;
        rename_staging(&self.staged, &self.target)?;
        renamed(&self.target);
        let dir = self.target.parent().context("staged file has no directory")?;
        sync_dir(dir)?;
        reconcile_managed(dir, self.retain, self.belongs, sync_dir, prune).map(drop)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.done {
            let _ = fs::remove_file(&self.staged);
        }
    }
}

/// Runs publication with the same directory durability check used by interrupted-write cleanup.
/// Error cases:
/// - as `write`
#[cfg(test)]
fn write_with_sync(
    cfg: &SnapshotSection,
    snapshot: &Snapshot,
    key: &EnclaveKey,
    sync_dir: &impl Fn(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    write_with_prune(cfg, snapshot, key, sync_dir, &mut PlainPrune)
}

/// Keeps the original publication/fsync order while allowing generation-aware final-file pruning.
/// Error cases:
/// - as `write`, including the supplied durability or pruning failure
#[cfg(test)]
fn write_with_prune(
    cfg: &SnapshotSection,
    snapshot: &Snapshot,
    key: &EnclaveKey,
    sync_dir: &impl Fn(&Path) -> anyhow::Result<()>,
    prune: &mut impl Prune,
) -> anyhow::Result<()> {
    reconcile_managed(&cfg.dir, cfg.retain.get(), is_snapshot_name, sync_dir, prune)?;
    stage(cfg, snapshot, key)?.publish_with(sync_dir, prune, |_| {})?;
    tracing::info!(seq = snapshot.offset.seq, "checkpoint snapshot stored");
    Ok(())
}

/// Publishes one staging file, releasing its allocation on every failed write, sync, or rename.
/// Error cases:
/// - staging creation, the supplied write/sync operation, publication, or cleanup fails
fn publish(
    staged: &Path,
    target: &Path,
    write_synced: impl FnOnce(&mut File) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    write_staging(staged, write_synced)?;
    rename_staging(staged, target)
}

/// Creates and syncs a staging file, removing it if the write or sync fails.
/// Error cases:
/// - creation, the supplied write/sync operation, or cleanup fails
fn write_staging(staged: &Path, write_synced: impl FnOnce(&mut File) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let written = (|| {
        let mut file =
            OpenOptions::new().write(true).create_new(true).open(staged).context("creating staged snapshot")?;
        write_synced(&mut file)
    })();
    if written.is_err() {
        remove_staging(staged)?;
    }
    written
}

/// Renames a synced staging file into place, removing it if the rename fails.
/// Error cases:
/// - the rename or its cleanup fails
fn rename_staging(staged: &Path, target: &Path) -> anyhow::Result<()> {
    let renamed = fs::rename(staged, target).context("publishing snapshot");
    if renamed.is_err() {
        remove_staging(staged)?;
    }
    renamed
}

/// Removes a failed attempt's staging file, if it exists.
/// Error cases:
/// - the file exists but cannot be removed
fn remove_staging(staged: &Path) -> anyhow::Result<()> {
    match fs::remove_file(staged) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("cleaning failed snapshot staging file"),
    }
}

/// Reclaims interrupted writes and excess caches, then lists newest cache files first.
/// Error cases:
/// - an existing directory cannot be listed, synced, or pruned
#[cfg(test)]
pub(crate) fn candidates(cfg: &SnapshotSection) -> anyhow::Result<Vec<PathBuf>> {
    candidates_with_sync(cfg, &sync_directory)
}

/// Makes excess publications durable before pruning; a failed cleanup blocks further allocation.
/// Error cases:
/// - as `candidates`
#[cfg(test)]
fn candidates_with_sync(
    cfg: &SnapshotSection,
    sync_dir: &impl Fn(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<PathBuf>> {
    reconcile_publications(&cfg.dir, cfg.retain.get(), is_snapshot_name, sync_dir)
}

/// Reconciles a bounded publication group before any later allocation can replace durable caches.
/// Error cases:
/// - listing, interrupted staging cleanup, directory sync, or retention fails
#[cfg(test)]
pub(super) fn reconcile_publications(
    dir: &Path,
    retain: usize,
    belongs: impl Fn(&str) -> bool,
    sync_dir: &impl Fn(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<PathBuf>> {
    reconcile_managed(dir, retain, belongs, sync_dir, &mut PlainPrune)
}

/// Reclaims interrupted staging files and caches and heads beyond retention left by an earlier run,
/// once startup has authorized writes. Each later write reconciles its own family again.
/// Error cases:
/// - listing, staging cleanup, directory sync, or journaled pruning fails
pub(crate) fn reclaim(cfg: &SnapshotSection, prune: &mut impl Prune) -> anyhow::Result<()> {
    reconcile_managed(&cfg.dir, cfg.retain.get(), is_snapshot_name, &sync_directory, prune)?;
    head::reclaim(cfg, prune)?;
    catalog_heads::reclaim_staging(cfg)
}

/// Removes one final cache only through the caller's journaled reference transaction.
pub(crate) trait Prune {
    fn remove(&mut self, path: &Path) -> anyhow::Result<()>;
}

/// Removes a cache file with no generation journal, for publication tests outside paired caches.
#[cfg(test)]
struct PlainPrune;

#[cfg(test)]
impl Prune for PlainPrune {
    fn remove(&mut self, path: &Path) -> anyhow::Result<()> {
        fs::remove_file(path).context("pruning snapshot")
    }
}

/// Syncs replacements before invoking each retirement hook, and syncs the final retained set.
/// Error cases:
/// - listing, staging cleanup, directory sync or managed final-file removal fails
pub(super) fn reconcile_managed(
    dir: &Path,
    retain: usize,
    belongs: impl Fn(&str) -> bool,
    sync_dir: &impl Fn(&Path) -> anyhow::Result<()>,
    prune: &mut impl Prune,
) -> anyhow::Result<Vec<PathBuf>> {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("listing snapshots"),
    };
    let mut paths = Vec::new();
    for entry in listing {
        let entry = entry.context("listing snapshot")?;
        let file = entry.file_name();
        let Some(file) = file.to_str() else {
            continue;
        };
        if file.strip_suffix(".tmp").is_some_and(&belongs) {
            fs::remove_file(entry.path()).context("cleaning interrupted snapshot staging file")?;
        } else if belongs(file) {
            paths.push(entry.path());
        }
    }
    paths.sort_unstable_by(|a, b| b.cmp(a));
    if paths.len() > retain {
        // A failed post-rename sync may leave one extra cache. Preserve earlier durable caches
        // until every replacement is durable, and refuse new writes if reconciliation fails.
        sync_dir(dir)?;
        for path in &paths[retain..] {
            prune.remove(path).context("pruning snapshot")?;
        }
        sync_dir(dir)?;
        paths.truncate(retain);
    }
    Ok(paths)
}

/// Lists optional caches during preflight without repairing, pruning, or creating host files.
/// Error cases:
/// - an existing snapshot directory cannot be listed
pub(crate) fn candidates_readonly(cfg: &SnapshotSection) -> anyhow::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(&cfg.dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("listing snapshots during preflight"),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.context("reading snapshot candidate")?;
        if entry.file_name().to_str().is_some_and(is_snapshot_name) {
            paths.push(entry.path());
        }
    }
    paths.sort_unstable_by(|a, b| b.cmp(a));
    Ok(paths)
}

/// Persists publication or retention changes to the snapshot directory.
/// Error cases:
/// - the directory cannot be opened or synced
fn sync_directory(path: &Path) -> anyhow::Result<()> {
    File::open(path).context("opening snapshot directory")?.sync_all().context("syncing snapshot directory")
}

/// Recognizes only the sequence/index filenames this module owns.
fn is_snapshot_name(file: &str) -> bool {
    file.strip_suffix(".json").and_then(|s| s.split_once('-')).is_some_and(|(seq, index)| {
        seq.len() == 20 && index.len() == 10 && seq.bytes().chain(index.bytes()).all(|b| b.is_ascii_digit())
    })
}

/// Whether the serialized engine `state` predates the V43 gross-loss accumulator: an open
/// period result without a gross-loss field, which only a pre-V43 build writes. `read` refuses
/// such a cache — recovery would drop a crossing period's pre-upgrade weight — and replays the log.
fn predates_gross_losses(state: &serde_json::Value) -> bool {
    state["period_results"]
        .as_object()
        .is_some_and(|results| results.values().any(|result| result.get("losses").is_none()))
}

/// Authenticates the whole payload before decoding engine state, then recomputes its root.
/// Error cases:
/// - the file is unreadable, malformed, oversized, from another deployment, untrusted, or inconsistent
/// - the engine state predates the V43 gross-loss accumulator
pub(crate) fn read(
    cfg: &SnapshotSection,
    path: &Path,
    chain_id: u64,
    vault: AccountId,
) -> anyhow::Result<(Snapshot, Engine)> {
    let snapshot = read_payload(cfg, path, chain_id, vault)?;
    let state: serde_json::Value = serde_json::from_str(&snapshot.engine).context("decoding snapshot engine")?;
    ensure!(
        !predates_gross_losses(&state),
        "snapshot predates the gross-loss accumulator; recovery replays the full log"
    );
    let mut engine: Engine = serde_json::from_value(state).context("decoding snapshot engine")?;
    ensure!(engine.last_seq() == Some(snapshot.checkpoint.seq), "snapshot sequence mismatch");
    let state_root = engine.state_root();
    let root = format!("0x{}", hex::encode(state_root));
    ensure!(root == snapshot.checkpoint.root, "snapshot state root mismatch");
    let domain = crate::log::CheckpointDomain { chain_id, vault };
    crate::checkpoints::signature::verify_with_domain(&snapshot.checkpoint, &engine, Some(domain))?;
    snapshot.context.validate(&snapshot.checkpoint, domain)?;
    engine.restore_indexes();
    snapshot.context.validate_engine(&snapshot.checkpoint, &engine)?;
    Ok((snapshot, engine))
}

/// Verifies a signed metadata reference without deserializing or trusting engine state.
/// Error cases:
/// - encoding, provenance, deployment, schema or physical filename validation fails
pub(super) fn read_payload(
    cfg: &SnapshotSection,
    path: &Path,
    chain_id: u64,
    vault: AccountId,
) -> anyhow::Result<Snapshot> {
    let (payload, signer) = verified_payload(cfg, path, chain_id, vault)?;
    decode_payload(path, chain_id, vault, &payload, signer)
}

/// Authenticates raw metadata before decoding, preserving unsupported signed formats as evidence.
/// Error cases:
/// - IO, envelope bounds, signature parsing or independent signer authorization fails
pub(super) fn verified_payload(
    cfg: &SnapshotSection,
    path: &Path,
    chain_id: u64,
    vault: AccountId,
) -> anyhow::Result<(String, AccountId)> {
    let mut bytes = Vec::new();
    File::open(path)?.take((cfg.max_bytes.get() as u64).saturating_add(1)).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= cfg.max_bytes.get(), "snapshot exceeds snapshots.max_bytes");
    let envelope: Envelope = serde_json::from_slice(&bytes).context("decoding snapshot envelope")?;
    let signature: Signature = envelope.signature.parse().context("snapshot signature")?;
    let signer = eip712::recover(&B256::from(digest(chain_id, vault, envelope.payload.as_bytes())), &signature)
        .context("invalid snapshot signature")?;
    ensure!(cfg.trusted_signers.contains(&signer), "snapshot signer is not independently trusted");
    Ok((envelope.payload, signer))
}

/// Checks authenticated metadata without interpreting engine bytes as recovered state.
/// Error cases:
/// - schema, deployment, physical filename, signer or required progress validation fails
pub(super) fn decode_payload(
    path: &Path,
    chain_id: u64,
    vault: AccountId,
    payload: &str,
    signer: AccountId,
) -> anyhow::Result<Snapshot> {
    let snapshot: Snapshot = serde_json::from_str(payload).context("decoding snapshot")?;
    ensure!(
        (snapshot.schema == 2 && snapshot.pair.is_none()) || (snapshot.schema == 3 && snapshot.pair.is_some()),
        "unsupported or incomplete snapshot schema"
    );
    ensure!(snapshot.chain_id == chain_id && snapshot.vault == vault, "snapshot deployment mismatch");
    ensure!(snapshot.checkpoint.signer.parse::<AccountId>()? == signer, "snapshot checkpoint signer mismatch");
    ensure!(
        path.file_name().and_then(|s| s.to_str()) == Some(name(snapshot.offset).as_str()),
        "snapshot filename mismatch"
    );
    ensure!(snapshot.offset.seq == snapshot.checkpoint.seq, "snapshot offset mismatch");
    snapshot.progress.genesis.ensure_closed()?;
    Ok(snapshot)
}

/// Warning episodes identify the skipped cache; another cache or full replay remains available.
pub(crate) fn rejected(path: &Path, reason: &anyhow::Error) {
    warn!(alert = "snapshot_rejected", path = %path.display(), "ignoring snapshot: {reason:#}");
}

/// Domain-separated authentication covers every byte, including replay facts and checkpoint offset.
fn digest(chain_id: u64, vault: AccountId, payload: &[u8]) -> [u8; 32] {
    let mut hash = Keccak256::new();
    hash.update(b"engine.snapshot.v3");
    hash.update(chain_id.to_be_bytes());
    hash.update(vault.0);
    hash.update(payload);
    hash.finalize().into()
}

/// Stable lexical ordering matches checkpoint order, including repeated checkpoints at one seq.
pub(crate) fn name(offset: Offset) -> String {
    format!("{:020}-{:010}.json", offset.seq, offset.index)
}

/// Requires the nullable compatibility field; production paired snapshots always carry a pair.
/// Error cases:
/// - a missing or malformed field
fn required_pair<'de, D: Deserializer<'de>>(decoder: D) -> Result<Option<paired::Pair>, D::Error> {
    Option::<paired::Pair>::deserialize(decoder)
}

#[cfg(test)]
mod tests;
