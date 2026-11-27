//! From `V93`, a carried genesis's stakes, mirrored again (`carry`). The genesis carries no stake: its `End` records
//! the source's completed frontier and total staked, and the node mirrors the kept staking contract's `Staked` and
//! `UnstakeCompleted` events from its deployment on, as it mirrors any. Until a completed frontier reaches the
//! source's block, with its hash and the stakes summing to the source's total, no frontier passes it, none names a
//! turn, no registry, schedule, vote or fault event mirrors, and no epoch closes; that frontier ends the re-mirror.
//! A frontier at that block with another total is refused, so a re-mirror that missed an event holds every close.
//! Reaching it records it as the carried frontier, at or below which `V92` refuses every later mirror.

use alloy_primitives::B256;

use crate::engine::Engine;
use crate::types::Millis;

impl Engine {
    /// Whether a carried genesis's stakes are still being mirrored again.
    pub fn remirroring(&self) -> bool {
        self.staking.as_ref().is_some_and(|staking| staking.remirror.is_some())
    }

    /// Why a completed staking frontier at `block` cannot apply while the carried stakes are mirrored again, if it
    /// cannot; none once they are.
    /// Error cases:
    /// - a frontier that names a turn, or passes the source's block
    /// - a frontier at the source's block with another hash, or with the stakes summing to another total
    pub(super) fn remirror_refusal(
        &self,
        block: u64,
        block_hash: B256,
        named_turn: Option<Option<Millis>>,
    ) -> Option<String> {
        let staking = self.staking.as_ref()?;
        let pending = staking.remirror?;
        let through = pending.through;
        if named_turn.is_some() {
            return Some("no frontier names a turn while the carried stakes are mirrored again".into());
        }
        if block < through.block {
            return None;
        }
        if block > through.block {
            return Some(format!("the carried stakes are mirrored again through block {} first", through.block));
        }
        if block_hash != through.block_hash {
            return Some(format!("block {block} is {block_hash}, not the carried frontier's {}", through.block_hash));
        }
        let staked = staking.staked_total.unwrap_or_default();
        (staked != pending.staked_total).then(|| {
            format!(
                "the stakes mirrored again sum to {staked} at block {block}, not the source's total of {}",
                pending.staked_total
            )
        })
    }
}
