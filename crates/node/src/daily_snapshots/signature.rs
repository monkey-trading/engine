//! The enclave key's signature over a daily snapshot: its deployment, its checkpoint's offset in the log and
//! root, and the SHA-256 of its engine's encoding (`state_digest`), which binds the parts of the state the root
//! leaves out. A member that starts from a snapshot takes the state only on the word of the vault-recorded
//! signer that signed the snapshot's checkpoint, so it requires this signature by that same key
//! (`open_signed`); a replay in ranges, which compares the states where its ranges meet, needs none.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, ensure};
use engine_core::Engine;
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use serde::Deserialize;
use serde_json::value::RawValue;
use sha2::Sha256;
use sha3::{Digest, Keccak256};

use crate::daily_snapshots::{DailySnapshot, MAX_SNAPSHOT_BYTES, SCHEMA, ensure_engine, ensure_named};
use crate::log::{Checkpoint, CheckpointDomain, Offset};
use crate::tee::EnclaveKey;

/// What signs the daily snapshots a node writes: its enclave key, which signs its checkpoints, in its
/// deployment.
#[derive(Clone)]
pub(crate) struct Signing {
    pub key: Arc<EnclaveKey>,
    pub domain: CheckpointDomain,
}

/// A daily snapshot whose signature, by `signer`, a vault-recorded signer, covers its state, read from `path`
/// with its indexes rebuilt (`open_signed`).
pub(crate) struct Signed {
    pub snapshot: DailySnapshot,
    pub signer: AccountId,
    pub path: PathBuf,
}

#[cfg(test)]
impl Signing {
    /// The published mock key, in a stand-in deployment: for a test whose snapshots no member starts from.
    pub(crate) fn for_tests() -> Self {
        let domain = CheckpointDomain { chain_id: 1, vault: AccountId([0x11; 20]) };
        Self { key: Arc::new(EnclaveKey::test_key()), domain }
    }
}

impl Signing {
    /// The signature over the snapshot of the checkpoint at `offset` with `root`, whose engine encodes to
    /// bytes whose SHA-256 is `state`.
    pub(crate) fn sign(&self, offset: Offset, root: B256, state: [u8; 32]) -> String {
        self.key.sign_digest(digest(self.domain, offset, root, state))
    }
}

/// The digest a daily snapshot's signature signs: a tag no other signature of the enclave's uses, the
/// deployment, the checkpoint's offset and root, and the state's SHA-256.
pub(crate) fn digest(domain: CheckpointDomain, offset: Offset, root: B256, state: [u8; 32]) -> [u8; 32] {
    let mut hash = Keccak256::new();
    hash.update(b"engine.daily-snapshot.v1");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(offset.seq.to_be_bytes());
    hash.update(offset.index.to_be_bytes());
    hash.update(root.0);
    hash.update(state);
    hash.finalize().into()
}

/// Reads the daily snapshot at `path` for a member's start, in `domain`: of the signed format, at most
/// `MAX_SNAPSHOT_BYTES` long, whose engine's bytes as written hash to the state its signature covers, and whose
/// signature is by its checkpoint's signer, one of `signers`, the vault's recorded signers; then its engine is
/// decoded, checked to stand at its offset with what a replica needs, as `load` checks it, its resting-order
/// index rebuilt from its accounts' orders and its transient indexes restored. The signature is checked before
/// the engine is decoded, so only a state such a signer vouched for reaches the engine's own code. Its root is
/// the caller's to check against the checkpoint the copy holds at its offset.
/// Error cases:
/// - the file cannot be read, is longer than the bound, or is not a snapshot of the signed format
/// - its name, offset and checkpoint name different offsets (`ensure_named`), or its engine does not stand at its
///   offset or predates `V27` or custody finality (`ensure_engine`)
/// - its engine's bytes do not hash to the state it names, or its signature is malformed, by another key than its
///   checkpoint's signer, or by a key `signers` does not hold
pub(crate) fn open_signed(
    path: &Path,
    domain: CheckpointDomain,
    signers: &BTreeSet<AccountId>,
) -> anyhow::Result<Signed> {
    /// A signed snapshot as written, its engine's bytes held as they are.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Envelope<'a> {
        schema: u32,
        offset: Offset,
        checkpoint: Checkpoint,
        #[serde(borrow)]
        engine: &'a RawValue,
        state: String,
        signature: String,
    }
    let bytes = std::fs::metadata(path).with_context(|| format!("opening daily snapshot {}", path.display()))?.len();
    ensure!(bytes <= MAX_SNAPSHOT_BYTES, "daily snapshot {} is longer than the bound", path.display());
    let text = std::fs::read_to_string(path).with_context(|| format!("reading daily snapshot {}", path.display()))?;
    let Envelope { schema, offset, checkpoint, engine, state, signature } = serde_json::from_str(&text)
        .with_context(|| format!("daily snapshot {} is not one of the signed format", path.display()))?;
    ensure!(schema == SCHEMA, "daily snapshot {} is of format {schema}, not {SCHEMA}", path.display());
    ensure_named(path, offset, &checkpoint)?;
    let state: [u8; 32] = hex::decode(state.trim_start_matches("0x"))
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .with_context(|| format!("daily snapshot {}'s state digest is not 32 bytes of hex", path.display()))?;
    ensure!(
        <[u8; 32]>::from(<Sha256 as sha2::Digest>::digest(engine.get().as_bytes())) == state,
        "daily snapshot {}'s state is not the state its signature covers",
        path.display()
    );
    let root: B256 = checkpoint.root.parse().context("the snapshot's checkpoint root")?;
    let checkpoint_signer: AccountId = checkpoint.signer.parse().context("the snapshot's checkpoint signer")?;
    let parsed: Signature = signature.parse().context("the snapshot's signature is malformed")?;
    let signer = eip712::recover(&B256::from(digest(domain, offset, root, state)), &parsed);
    ensure!(
        signer == Some(checkpoint_signer),
        "daily snapshot {} is not signed by {checkpoint_signer}, its checkpoint's signer, in this deployment",
        path.display()
    );
    ensure!(
        signers.contains(&checkpoint_signer),
        "daily snapshot {} is signed by {checkpoint_signer}, which the vault has not recorded",
        path.display()
    );
    let engine: Engine = serde_json::from_str(engine.get())
        .with_context(|| format!("decoding daily snapshot {}'s engine", path.display()))?;
    drop(text);
    let (state, signature) = (Some(hex::encode(state)), Some(signature));
    let mut snapshot = DailySnapshot { schema, offset, checkpoint, engine, state, signature };
    ensure_engine(path, &snapshot)?;
    snapshot.engine.rebuild_resting();
    snapshot.engine.restore_indexes();
    Ok(Signed { snapshot, signer: checkpoint_signer, path: path.to_owned() })
}
