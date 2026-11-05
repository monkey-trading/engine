//! The digests the enclave key signs: withdrawal authorizations, checkpoint roots with and
//! without their exact parent, and the on-chain log heads with the parent head they extend.
//! Each mirrors its contract's packed encoding byte-for-byte.

use engine_core::Fixed;
use engine_core::types::AccountId;
use sha3::{Digest, Keccak256};

/// Historical withdrawal authorization bytes, unchanged across checkpoint versions.
pub(crate) fn withdrawal_digest(
    chain_id: u64,
    vault: &[u8; 20],
    account: AccountId,
    amount: Fixed,
    nonce: u64,
) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(b"engine.withdrawal.v1");
    h.update(chain_id.to_be_bytes());
    h.update(vault);
    h.update(account.0);
    h.update(amount.to_be_bytes());
    h.update(nonce.to_be_bytes());
    h.finalize().into()
}

/// V2 binds the exact parent so two authorized publishers cannot extend divergent forks.
pub(crate) fn checkpoint_digest_v2(
    chain_id: u64,
    vault: &[u8; 20],
    seq: u64,
    root: &[u8; 32],
    parent: crate::log::CheckpointParent,
) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(b"engine.checkpoint.v2");
    h.update(chain_id.to_be_bytes());
    h.update(vault);
    h.update(seq.to_be_bytes());
    h.update(root);
    h.update([u8::from(parent.has_checkpoint)]);
    h.update(parent.seq.to_be_bytes());
    h.update(parent.root.0);
    h.finalize().into()
}

/// The digest a log-head signature covers: domain tag, chain id, vault, signer epoch, seq, link,
/// and the parent head it extends. Mirrors `LogHeads.sol`'s `keccak256(abi.encodePacked(...))`
/// byte-for-byte (#541).
pub(crate) fn log_head_digest(
    chain_id: u64,
    vault: &[u8; 20],
    epoch: u64,
    seq: u64,
    link: &[u8; 32],
    parent_seq: u64,
    parent_link: &[u8; 32],
) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(b"engine.log-head.v1");
    h.update(chain_id.to_be_bytes());
    h.update(vault);
    h.update(epoch.to_be_bytes());
    h.update(seq.to_be_bytes());
    h.update(link);
    h.update(parent_seq.to_be_bytes());
    h.update(parent_link);
    h.finalize().into()
}

/// The digest a checkpoint signature covers: domain tag, chain id, vault, seq, and root.
pub(crate) fn checkpoint_digest(chain_id: u64, vault: &[u8; 20], seq: u64, root: &[u8; 32]) -> [u8; 32] {
    let mut h = Keccak256::new();
    h.update(b"engine.checkpoint.v1");
    h.update(chain_id.to_be_bytes());
    h.update(vault);
    h.update(seq.to_be_bytes());
    h.update(root);
    h.finalize().into()
}
