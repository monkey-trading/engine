//! The carried staking frontier, from `V92`: the source log mirrored every event of the contracts it followed at or
//! below it, under references this log does not hold, so mirroring one again would apply it twice. Every mirror
//! (stakes, unstakes, and the registry, schedule, vote and fault proof contracts' events) is read in one scan, so a
//! mirror from a block at or below it is refused, as is a completed scan there.

use crate::engine::Engine;
use crate::protocol::ProtocolVersion;

impl Engine {
    /// From `V92`, the carried staking frontier's block, when `block` is at or below it.
    pub(crate) fn carried_frontier_covering(&self, block: u64) -> Option<u64> {
        let through = self.staking.as_ref()?.carried_through?;
        (self.protocol_version >= ProtocolVersion::V92 && block <= through).then_some(through)
    }

    /// For an activation crossing `V92` in a log whose genesis carried state, records its completed staking
    /// frontier as the carried one. That frontier is at or above the one the genesis carried, and every event at or
    /// below it is in the source log or credited in this one, so refusing a mirror there refuses only a repeat.
    pub(crate) fn start_carried_frontier(&mut self, version: ProtocolVersion) {
        if self.protocol_version >= ProtocolVersion::V92
            || version < ProtocolVersion::V92
            || self.carried_from.is_none()
        {
            return;
        }
        let staking = self.staking.as_mut().expect("a carried genesis initialised staking");
        let completed = staking.completed_through.expect("a carried genesis completes the staking frontier");
        staking.carried_through = Some(completed.block);
    }
}
