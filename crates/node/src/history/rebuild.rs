//! Binds a rebuild to the typed entries authenticated by its read-only pass. Catalog descriptors
//! separately bind the actual raw segment bytes, including the active-file prefix.

use anyhow::{Context, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::log::{LogEntry, Offset, offset_of};

/// The complete read-only replay prefix; this is a comparison witness, never root authority.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ReplayFingerprint {
    pub entries: u64,
    pub last: Option<Offset>,
    pub digest: [u8; 32],
}

/// Updated in the same visit that strictly replays each entry, using canonical typed input bytes.
pub(crate) struct Fingerprinter {
    hash: Sha256,
    entries: u64,
    last: Option<Offset>,
}

impl Fingerprinter {
    /// Starts the domain-separated sequence comparison.
    pub(crate) fn new() -> Self {
        let mut hash = Sha256::new();
        hash.update(b"engine.history.rebuild-prefix.v1");
        Self { hash, entries: 0, last: None }
    }

    /// Includes every command, withdrawal, and checkpoint in physical entry order.
    /// Error cases:
    /// - encoding, physical offset, or entry-count arithmetic fails
    pub(crate) fn observe(&mut self, entry: &LogEntry) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(entry).context("encoding rebuild prefix entry")?;
        let offset = offset_of(self.last, entry)?;
        self.entries = self.entries.checked_add(1).context("rebuild prefix entry count overflow")?;
        self.hash.update((bytes.len() as u64).to_be_bytes());
        self.hash.update(&bytes);
        self.last = Some(offset);
        Ok(())
    }

    /// Captures the authenticated input identity, without retaining its event or log history.
    pub(crate) fn finish(self) -> ReplayFingerprint {
        ReplayFingerprint { entries: self.entries, last: self.last, digest: self.hash.finalize().into() }
    }
}

impl ReplayFingerprint {
    /// Requires the writing pass to consume the same full prefix before exposing its generation.
    /// Error cases:
    /// - a command, event, checkpoint, completion, order, or prefix length changed between passes
    pub(crate) fn verify(&self, rebuilt: &Self) -> anyhow::Result<()> {
        ensure!(self == rebuilt, "authenticated log prefix changed during history rebuild");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use engine_core::Engine;
    use engine_core::command::{Command, Sequenced};
    use engine_core::types::AccountId;

    use crate::log::{Checkpoint, CheckpointDomain, CommandEntry};
    use crate::tee::EnclaveKey;

    #[test]
    fn writing_rebuild_must_match_commands_checkpoints_order_and_complete_prefix() {
        let key = EnclaveKey::test_key();
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([0; 20]) };
        let mut engine = Engine::new();
        let first = Sequenced { seq: 0, ts: 0, cmd: Command::Tick };
        let events = engine.apply(&first).unwrap();
        let root = engine.state_root();
        let entries = vec![
            LogEntry::Command(CommandEntry { sequenced: first, events: Some(events) }),
            LogEntry::Checkpoint(Checkpoint {
                seq: 0,
                root: format!("0x{}", hex::encode(root)),
                signer: key.address_hex(),
                signature: key.sign_checkpoint(domain.chain_id, &domain.vault.0, 0, &root),
                domain: Some(domain),
                parent: None,
                deferred_withdrawals: false,
            }),
            LogEntry::Command(CommandEntry {
                sequenced: Sequenced { seq: 1, ts: 1, cmd: Command::Tick },
                events: Some(Vec::new()),
            }),
        ];
        let expected = fingerprint(&entries);
        expected.verify(&fingerprint(&entries)).unwrap();
        let mut changed = entries.clone();
        let LogEntry::Checkpoint(checkpoint) = &mut changed[1] else { unreachable!() };
        checkpoint.signature.push('0');
        assert!(
            expected.verify(&fingerprint(&changed)).is_err(),
            "unchanged final command cannot authorize changed checkpoint evidence"
        );
        let mut changed = entries.clone();
        let LogEntry::Command(command) = &mut changed[2] else { unreachable!() };
        command.sequenced.ts += 1;
        assert!(expected.verify(&fingerprint(&changed)).is_err());
        assert!(expected.verify(&fingerprint(&entries[..2])).is_err());
        let mut changed = entries;
        changed.swap(1, 2);
        assert!(expected.verify(&fingerprint(&changed)).is_err());
    }

    fn fingerprint(entries: &[LogEntry]) -> ReplayFingerprint {
        let mut fingerprint = Fingerprinter::new();
        for entry in entries {
            fingerprint.observe(entry).unwrap();
        }
        fingerprint.finish()
    }
}
