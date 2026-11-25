//! The enclave key's signature over a daily snapshot: its deployment, its checkpoint's offset in the log and
//! root, the SHA-256 of its engine's encoding (`state_digest`), which binds the parts of the state the root
//! leaves out, and the SHA-256s of the replay state it carries and of its history anchors, each as written. A
//! start from a snapshot takes the state and what it carries only on the word of the vault-recorded signer
//! that signed the snapshot's checkpoint, so it requires this signature by that same key (`open_signed`); a
//! replay in ranges, which compares the states where its ranges meet, needs none.

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
use crate::sequencer::replay::Carried;
use crate::tee::EnclaveKey;

/// What signs the daily snapshots a node writes: its enclave key, which signs its checkpoints, in its
/// deployment.
#[derive(Clone)]
pub(crate) struct Signing {
    pub key: Arc<EnclaveKey>,
    pub domain: CheckpointDomain,
}

/// A daily snapshot whose signature, by `signer`, a vault-recorded signer, covers its state and what it carries,
/// read from `path` with its indexes rebuilt (`open_signed`).
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

/// The SHA-256s a snapshot's signature covers besides its checkpoint's: its engine's encoding, the replay state
/// it carries and its history anchors, each as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Covered {
    pub state: [u8; 32],
    pub carried: [u8; 32],
    pub history: [u8; 32],
}

impl Signing {
    /// The signature over the snapshot of the checkpoint at `offset` with `root`, whose parts hash to `covered`.
    pub(crate) fn sign(&self, offset: Offset, root: B256, covered: Covered) -> String {
        self.key.sign_digest(digest(self.domain, offset, root, covered))
    }
}

/// The digest a daily snapshot's signature signs: a tag no other signature of the enclave's uses, the
/// deployment, the checkpoint's offset and root, and the SHA-256s of what the snapshot holds besides.
pub(crate) fn digest(domain: CheckpointDomain, offset: Offset, root: B256, covered: Covered) -> [u8; 32] {
    let mut hash = Keccak256::new();
    hash.update(b"engine.daily-snapshot.v2");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(offset.seq.to_be_bytes());
    hash.update(offset.index.to_be_bytes());
    hash.update(root.0);
    hash.update(covered.state);
    hash.update(covered.carried);
    hash.update(covered.history);
    hash.finalize().into()
}

/// The SHA-256 of `bytes`.
pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    <Sha256 as sha2::Digest>::digest(bytes).into()
}

/// Reads the daily snapshot at `path` for a start, in `domain`: of the format that carries the replay state,
/// at most `MAX_SNAPSHOT_BYTES` long, whose engine's bytes as written hash to the state its signature covers,
/// with the replay state and history anchors it carries as written, and whose signature is by its checkpoint's
/// signer, one of `signers`, the vault's recorded signers; then its engine and replay state are decoded, the
/// engine checked to stand at its offset with what a start needs, as `load` checks it, its resting-order index
/// rebuilt from its accounts' orders and its transient indexes restored. The signature is checked before
/// anything is decoded, so only what such a signer vouched for reaches the engine's own code. Its root and the
/// replay state's agreement with the engine are the caller's to check (`Replay::resume`).
/// Error cases:
/// - the file cannot be read, is longer than the bound, or is not a snapshot of the format this build writes
/// - its name, offset and checkpoint name different offsets (`ensure_named`), or its engine does not stand at its
///   offset or predates `V27` or custody finality (`ensure_engine`)
/// - its engine's bytes do not hash to the state it names, or its signature is malformed, by another key than its
///   checkpoint's signer, or by a key `signers` does not hold
/// - its replay state does not decode
pub(crate) fn open_signed(
    path: &Path,
    domain: CheckpointDomain,
    signers: &BTreeSet<AccountId>,
) -> anyhow::Result<Signed> {
    /// A snapshot of the format this build writes, its parts' bytes held as they are.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Envelope<'a> {
        schema: u32,
        offset: Offset,
        checkpoint: Checkpoint,
        #[serde(borrow)]
        engine: &'a RawValue,
        state: String,
        #[serde(borrow)]
        carried: &'a RawValue,
        #[serde(borrow)]
        history: &'a RawValue,
        signature: String,
    }
    let bytes = std::fs::metadata(path).with_context(|| format!("opening daily snapshot {}", path.display()))?.len();
    ensure!(bytes <= MAX_SNAPSHOT_BYTES, "daily snapshot {} is longer than the bound", path.display());
    let text = std::fs::read_to_string(path).with_context(|| format!("reading daily snapshot {}", path.display()))?;
    // Its format from its first bytes, where every writer puts it, with no second parse of the whole file.
    ensure!(
        !super::earlier_format(text.as_bytes()),
        "daily snapshot {} is of an earlier format, which carries no replay state for a start, not {SCHEMA}",
        path.display()
    );
    let Envelope { schema, offset, checkpoint, engine, state, carried, history, signature } =
        serde_json::from_str(&text)
            .with_context(|| format!("daily snapshot {} is not one of format {SCHEMA}", path.display()))?;
    ensure!(schema == SCHEMA, "daily snapshot {} is of format {schema}, not {SCHEMA}", path.display());
    ensure_named(path, offset, &checkpoint)?;
    let state: [u8; 32] = hex::decode(state.trim_start_matches("0x"))
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .with_context(|| format!("daily snapshot {}'s state digest is not 32 bytes of hex", path.display()))?;
    ensure!(
        sha256(engine.get().as_bytes()) == state,
        "daily snapshot {}'s state is not the state its signature covers",
        path.display()
    );
    let covered =
        Covered { state, carried: sha256(carried.get().as_bytes()), history: sha256(history.get().as_bytes()) };
    let root: B256 = checkpoint.root.parse().context("the snapshot's checkpoint root")?;
    let checkpoint_signer: AccountId = checkpoint.signer.parse().context("the snapshot's checkpoint signer")?;
    let parsed: Signature = signature.parse().context("the snapshot's signature is malformed")?;
    let signer = eip712::recover(&B256::from(digest(domain, offset, root, covered)), &parsed);
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
    let decoded: Carried = serde_json::from_str(carried.get())
        .with_context(|| format!("decoding daily snapshot {}'s replay state", path.display()))?;
    let history = Some(RawValue::from_string(history.get().to_owned()).context("the snapshot's history anchors")?);
    let engine: Engine = serde_json::from_str(engine.get())
        .with_context(|| format!("decoding daily snapshot {}'s engine", path.display()))?;
    drop(text);
    let (state, signature, carried) = (Some(hex::encode(state)), Some(signature), Some(decoded));
    let mut snapshot = DailySnapshot { schema, offset, checkpoint, engine, state, carried, history, signature };
    ensure_engine(path, &snapshot)?;
    snapshot.engine.rebuild_resting();
    snapshot.engine.restore_indexes();
    Ok(Signed { snapshot, signer: checkpoint_signer, path: path.to_owned() })
}
