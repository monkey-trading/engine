//! Commitments bind a derived history head to the exact authenticated command/event prefix.

use anyhow::Context;
use sha2::{Digest, Sha256};
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

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::command::{Command, Sequenced};
    use engine_core::event::Event;
    use engine_core::types::AccountId;

    #[test]
    fn streaming_identity_preserves_canonical_bytes_and_covers_all_logged_events() {
        let mut command =
            CommandEntry { sequenced: Sequenced { seq: 0, ts: 0, cmd: Command::Tick }, events: Some(Vec::new()) };
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
