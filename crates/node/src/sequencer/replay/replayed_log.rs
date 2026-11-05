//! A log read from disk and replayed, and what the log-head poster reads from it.

use super::Replayed;
use crate::log::SegmentedLog;

/// A log read from disk and replayed, before anything is written to it.
pub struct ReplayedLog {
    pub(crate) log: SegmentedLog,
    pub(crate) replayed: Replayed,
    pub(crate) snapshot_seq: Option<u64>,
    pub(crate) legacy_authorizations: crate::sequencer::legacy::Authorizations,
}

impl ReplayedLog {
    /// The cumulative command-prefix digest after the log's last command: the head commitment this
    /// node publishes on chain. A trailing checkpoint does not change it.
    pub fn head_link(&self) -> engine_core::eip712::B256 {
        engine_core::eip712::B256::from(self.replayed.command_prefix)
    }

    /// The replay-authenticated prefix ring: the last `crate::log::PrefixRing::CAPACITY`
    /// (seq, cumulative-prefix) pairs, the poster's adoption evidence for on-chain heads.
    pub(crate) fn prefix_ring(&self) -> crate::log::PrefixRing {
        self.replayed.prefix_ring.clone()
    }
}
