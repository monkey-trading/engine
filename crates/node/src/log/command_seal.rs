//! Domain-separated command-origin evidence, independent of custody authorizations.

use anyhow::{Context, ensure};
use engine_core::eip712::{self, B256, Signature};
use engine_core::types::AccountId;
use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

use crate::log::{Checkpoint, CheckpointDomain, CommandEntry};

/// An enclave signature on one canonical command and its expected journal predecessor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandSeal {
    pub domain: CheckpointDomain,
    pub signer: AccountId,
    pub signature: String,
}

/// Links the typed command and complete event vector, excluding the seal.
/// Error cases:
/// - canonical command encoding fails
pub(crate) fn command_link(previous: B256, command: &CommandEntry) -> anyhow::Result<B256> {
    let payload = serde_json::to_vec(&(&command.sequenced, &command.events)).context("encoding command origin")?;
    let mut hash = Keccak256::new();
    hash.update(b"engine.command-origin.link.v1");
    hash.update(previous.0);
    hash.update(Keccak256::digest(payload));
    Ok(B256::from(<[u8; 32]>::from(hash.finalize())))
}

/// Restarts the command chain at an already verified checkpoint, including snapshot C.
/// Error cases:
/// - the checkpoint root is malformed
pub(crate) fn checkpoint_link(checkpoint: &Checkpoint) -> anyhow::Result<B256> {
    let root: B256 = checkpoint.root.parse().context("command origin checkpoint root")?;
    let mut hash = Keccak256::new();
    hash.update(b"engine.command-origin.checkpoint.v1");
    hash.update(checkpoint.seq.to_be_bytes());
    hash.update(root.0);
    Ok(B256::from(<[u8; 32]>::from(hash.finalize())))
}

/// Builds a journal digest that neither Vault checkpoint nor withdrawal entry points accept.
pub(crate) fn seal_digest(domain: CheckpointDomain, link: B256) -> [u8; 32] {
    let mut hash = Keccak256::new();
    hash.update(b"engine.command-origin.seal.v1");
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(link.0);
    hash.finalize().into()
}

impl CommandSeal {
    /// Verifies cryptographic origin; recovery separately pins the authorized signer.
    /// Error cases:
    /// - the signer or signature is invalid for this link and deployment
    pub(crate) fn verify(&self, link: B256) -> anyhow::Result<()> {
        let signature: Signature = self.signature.parse().context("command origin signature")?;
        ensure!(
            self.signer.0 != [0; 20]
                && eip712::recover(&B256::from(seal_digest(self.domain, link)), &signature) == Some(self.signer),
            "invalid command origin signature"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tee::EnclaveKey;
    use engine_core::command::{Command, Sequenced};

    fn command(seq: u64) -> CommandEntry {
        CommandEntry {
            sequenced: Sequenced { seq, ts: i64::try_from(seq).unwrap(), cmd: Command::Tick },
            events: Some(Vec::new()),
            seal: None,
        }
    }

    #[test]
    fn origin_binds_command_events_predecessor_and_deployment() {
        let key = EnclaveKey::test_key();
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
        let previous = B256::from([0; 32]);
        let mut entry = command(0);
        let link = command_link(previous, &entry).unwrap();
        let seal = key.seal_command(domain, link);
        seal.verify(link).unwrap();
        entry.seal = Some(seal.clone());
        assert_eq!(command_link(previous, &entry).unwrap(), link);
        entry.sequenced.ts += 1;
        assert!(seal.verify(command_link(previous, &entry).unwrap()).is_err());
        entry = command(0);
        entry.events = None;
        assert!(seal.verify(command_link(previous, &entry).unwrap()).is_err());
        assert!(seal.verify(command_link(B256::from([1; 32]), &command(0)).unwrap()).is_err());
        let mut changed = seal.clone();
        changed.domain.chain_id += 1;
        assert!(changed.verify(link).is_err());
        changed = seal.clone();
        changed.domain.vault.0[0] ^= 1;
        assert!(changed.verify(link).is_err());
        changed = seal;
        changed.signer = AccountId([1; 20]);
        assert!(changed.verify(link).is_err());
    }

    #[test]
    fn command_origin_binds_sequence_variant_and_ordered_event_content() {
        let key = EnclaveKey::test_key();
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
        let previous = B256::from([0; 32]);
        let entry = command(0);
        let seal = key.seal_command(domain, command_link(previous, &entry).unwrap());
        let mut changed = entry.clone();
        changed.sequenced.seq += 1;
        assert!(seal.verify(command_link(previous, &changed).unwrap()).is_err());
        changed = entry.clone();
        changed.sequenced.cmd = Command::Admin(engine_core::command::AdminCommand::ActivateProtocol {
            version: engine_core::protocol::ProtocolVersion::V2,
        });
        assert!(seal.verify(command_link(previous, &changed).unwrap()).is_err());
        changed = entry;
        changed.events = Some(vec![engine_core::event::Event::AdminApplied]);
        assert!(seal.verify(command_link(previous, &changed).unwrap()).is_err());
        let mut ordered = command(0);
        ordered.events = Some(vec![
            engine_core::event::Event::AdminApplied,
            engine_core::event::Event::AdminRejected { reason: "fixture".into() },
        ]);
        let seal = key.seal_command(domain, command_link(previous, &ordered).unwrap());
        ordered.events.as_mut().unwrap().reverse();
        assert!(seal.verify(command_link(previous, &ordered).unwrap()).is_err());
    }

    #[test]
    fn journal_signature_is_separate_from_checkpoint_and_withdrawal_purposes() {
        let key = EnclaveKey::test_key();
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
        let link = command_link(B256::from([0; 32]), &command(0)).unwrap();
        let signature: Signature = key.seal_command(domain, link).signature.parse().unwrap();
        for digest in [
            crate::tee::checkpoint_digest(domain.chain_id, &domain.vault.0, 0, &link.0),
            crate::tee::checkpoint_digest_v2(domain.chain_id, &domain.vault.0, 0, &link.0, Default::default()),
            crate::tee::withdrawal_digest(
                domain.chain_id,
                &domain.vault.0,
                AccountId(key.address()),
                engine_core::Fixed::ONE,
                0,
            ),
        ] {
            assert_ne!(eip712::recover(&B256::from(digest), &signature), Some(AccountId(key.address())));
        }
        let (withdrawal, _) = crate::withdrawals::tests::vector();
        let typed = crate::withdrawals::digest(&withdrawal).unwrap();
        assert_ne!(eip712::recover(&typed, &signature), Some(AccountId(key.address())));
    }
}
