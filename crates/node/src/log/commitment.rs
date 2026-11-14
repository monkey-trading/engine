//! Commitments bind a derived history head to the exact authenticated command/event prefix.

use anyhow::Context;
use engine_core::eip712::B256;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{self, Write};

use crate::log::CommandEntry;

/// Commits to the canonical typed command and its complete logged event vector.
/// Error cases:
/// - canonical command encoding fails
pub(crate) fn command_digest(command: &CommandEntry) -> anyhow::Result<[u8; 32]> {
    let mut hash = Sha256::new();
    hash.update(b"engine.log.command.v1");
    serde_json::to_writer(HashWriter(&mut hash), command).context("encoding authenticated command identity")?;
    Ok(hash.finalize().into())
}

/// Feeds serde's canonical bytes directly into the existing SHA-256 implementation.
struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Extends a command-prefix commitment with one fixed-length command identity.
pub(crate) fn extend_command_prefix(previous: [u8; 32], command: [u8; 32]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"engine.log.command-prefix.v1");
    hash.update(previous);
    hash.update(command);
    hash.finalize().into()
}

/// The highest-seq `PrefixRing::CAPACITY` (seq, cumulative-prefix) pairs recorded from one log:
/// evidence that the log holds an on-chain head. A head at a retained seq naming another prefix is
/// foreign to the log; one at a seq the ring does not hold carries no evidence either way.
#[derive(Debug, Clone)]
pub(crate) struct PrefixRing {
    pairs: BTreeMap<u64, B256>,
    capacity: usize,
}

impl Default for PrefixRing {
    /// An empty ring of `CAPACITY` pairs.
    fn default() -> Self {
        Self::with_capacity(Self::CAPACITY)
    }
}

impl PrefixRing {
    /// How many pairs the ring retains: the latest commands' as many as a follower's replica keeps
    /// links for (`RETAINED_LINKS`), so a restart that replays the log and a start from a replica
    /// seed the sequencer's tip ring and the log-head poster alike (2,048 in tests, so a test can
    /// lag past it). A head at a seq the ring no longer holds carries no evidence: the log-head
    /// poster does not adopt it, and a restart checks whatever the chain holds against the whole log.
    pub(crate) const CAPACITY: usize = if cfg!(test) { 2_048 } else { 1 << 16 };

    /// An empty ring retaining `capacity` pairs.
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self { pairs: BTreeMap::new(), capacity }
    }

    /// Records the cumulative prefix after `seq`, evicting the lowest seq past the capacity.
    pub(crate) fn record(&mut self, seq: u64, prefix: B256) {
        self.pairs.insert(seq, prefix);
        if self.pairs.len() > self.capacity {
            self.pairs.pop_first();
        }
    }

    /// Records every pair `other` retains.
    pub(crate) fn extend(&mut self, other: &PrefixRing) {
        other.pairs.iter().for_each(|(seq, prefix)| self.record(*seq, *prefix));
    }

    /// The retained cumulative prefix after `seq`, if the ring holds it.
    pub(crate) fn prefix_at(&self, seq: u64) -> Option<B256> {
        self.pairs.get(&seq).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::command::{Command, Sequenced};
    use engine_core::event::Event;
    use engine_core::types::AccountId;

    #[test]
    fn the_ring_retains_the_prefix_at_a_recorded_seq() {
        let mut ring = PrefixRing::default();
        assert_eq!(ring.prefix_at(7), None);
        ring.record(7, B256::from([7; 32]));
        assert_eq!(ring.prefix_at(7), Some(B256::from([7; 32])));
        assert_eq!(ring.prefix_at(8), None, "only recorded seqs carry evidence");
        ring.record(7, B256::from([8; 32]));
        assert_eq!(ring.prefix_at(7), Some(B256::from([8; 32])), "re-recording replaces the pair");
    }

    #[test]
    fn the_ring_evicts_the_lowest_seqs_past_the_capacity() {
        let mut ring = PrefixRing::default();
        for seq in 0..PrefixRing::CAPACITY as u64 + 4 {
            ring.record(seq, B256::from([1; 32]));
        }
        assert_eq!(ring.prefix_at(0), None, "the lowest seqs evict");
        assert_eq!(ring.prefix_at(3), None);
        assert_eq!(ring.prefix_at(4), Some(B256::from([1; 32])));
        assert!(ring.prefix_at(PrefixRing::CAPACITY as u64 + 3).is_some(), "the highest seqs are retained");
    }

    #[test]
    fn streaming_identity_preserves_canonical_bytes_and_covers_all_logged_events() {
        let mut command = CommandEntry {
            seal: None,
            sequenced: Sequenced { seq: 0, ts: 0, cmd: Command::Tick },
            events: Some(Vec::new()),
        };
        let mut expected = Sha256::new();
        expected.update(b"engine.log.command.v1");
        expected.update(serde_json::to_vec(&command).unwrap());
        assert_eq!(command_digest(&command).unwrap(), <[u8; 32]>::from(expected.finalize()));
        let previous = command_digest(&command).unwrap();
        command
            .events
            .as_mut()
            .unwrap()
            .push(Event::Deposited { account: AccountId([1; 20]), amount: engine_core::Fixed::ONE });
        let changed = command_digest(&command).unwrap();
        assert_ne!(previous, changed);
        assert_ne!(extend_command_prefix([0; 32], previous), extend_command_prefix([0; 32], changed));
        assert_ne!(extend_command_prefix([0; 32], changed), extend_command_prefix(previous, changed));
    }
}
