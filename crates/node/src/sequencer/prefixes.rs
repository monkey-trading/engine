//! The cumulative command prefix after each of the latest commands this sequencer applied: a
//! handover checks a successor's tip against it before it stops sequencing, so a tip that is not
//! on this log, or older than these, is refused while sequencing goes on.

use engine_core::eip712::B256;

use crate::log::{CommandEntry, PrefixRing};
use crate::sequencer::Sequencer;

/// How many of the latest commands' prefixes the sequencer retains: a follower's tip is a few
/// seconds behind the head when its successor asks for the tail, which this covers at a thousand
/// commands a second for over a minute. A restart's replay and a replica's start seed all of them.
pub(crate) const RETAINED_PREFIXES: usize = PrefixRing::CAPACITY;

impl Sequencer {
    /// Seals `entry`, the command just applied, at `link`, the command seal chain after it, then
    /// extends the cumulative command prefix with it and retains that.
    /// Error cases:
    /// - the command cannot be encoded
    pub(super) fn seal_and_extend(&mut self, entry: &mut CommandEntry, link: B256) -> anyhow::Result<()> {
        let domain = crate::log::CheckpointDomain { chain_id: self.chain_id, vault: self.vault };
        entry.seal = Some(self.key.seal_command(domain, link));
        self.command_prefix =
            crate::log::extend_command_prefix(self.command_prefix, crate::log::command_digest(entry)?);
        self.prefixes.record(entry.sequenced.seq, B256::from(self.command_prefix));
        Ok(())
    }
}

/// A ring of `RETAINED_PREFIXES` pairs, seeded with what recovery's replay retained.
pub(super) fn retained(replayed: &PrefixRing) -> PrefixRing {
    let mut ring = PrefixRing::with_capacity(RETAINED_PREFIXES);
    ring.extend(replayed);
    ring
}
