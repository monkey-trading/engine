//! Read-only origin validation shared by full replay and snapshot suffix replay.

use anyhow::{Context, bail, ensure};
use engine_core::eip712::B256;
use engine_core::types::AccountId;

use crate::log::command_seal::{checkpoint_link, command_link};
use crate::log::{Checkpoint, CheckpointDomain, CommandEntry};

/// Tracks tentative replay spans until independent authority covers them.
#[derive(Default)]
pub(crate) struct Origin {
    pub(crate) link: B256,
    domain: Option<CheckpointDomain>,
    signer: Option<AccountId>,
    first_untrusted: Option<u64>,
}

impl Origin {
    /// Pins the independent deployment and current signer used by checkpoint recovery.
    pub(crate) fn new(domain: Option<CheckpointDomain>, signer: Option<AccountId>) -> Self {
        Self { domain, signer, ..Self::default() }
    }

    /// Checks a present seal before applying a command and retains every uncovered span.
    /// Error cases:
    /// - encoding, domain, events, or the present cryptographic seal is invalid
    pub(crate) fn command(&mut self, command: &CommandEntry) -> anyhow::Result<()> {
        let link = command_link(self.link, command)?;
        let mut trusted = false;
        if let Some(seal) = &command.seal {
            ensure!(command.events.is_some(), "sealed command lacks logged events");
            ensure!(self.domain.is_none_or(|domain| domain == seal.domain), "command seal deployment mismatch");
            seal.verify(link)?;
            trusted = self.signer == Some(seal.signer) && self.domain == Some(seal.domain);
        }
        if !trusted {
            self.first_untrusted.get_or_insert(command.sequenced.seq);
        }
        self.link = link;
        Ok(())
    }

    /// Advances only after every existing checkpoint authority and parent check passed.
    /// Error cases:
    /// - the checkpoint root is malformed, or its authenticated deployment conflicts
    pub(crate) fn checkpoint(
        &mut self,
        checkpoint: &Checkpoint,
        domain: CheckpointDomain,
        authenticated: bool,
    ) -> anyhow::Result<()> {
        self.link = checkpoint_link(checkpoint)?;
        if authenticated {
            ensure!(
                self.domain.is_none_or(|previous| previous == domain),
                "command origin checkpoint deployment mismatch"
            );
            self.domain = Some(domain);
            self.first_untrusted = None;
        }
        Ok(())
    }

    /// Refuses unbound audit results and uncovered tails before writes, signing, or activation.
    /// Error cases:
    /// - recovery uses another authority, or any command lacks authenticated coverage
    pub(crate) fn require_recovery(&self, domain: CheckpointDomain, signer: AccountId) -> anyhow::Result<()> {
        ensure!(
            self.domain == Some(domain) && self.signer == Some(signer),
            "command origin recovery authority mismatch"
        );
        if let Some(seq) = self.first_untrusted {
            bail!("unauthenticated command tail starts at seq {seq}");
        }
        Ok(())
    }

    /// Authenticates a tail in an explicit or already verified signed checkpoint deployment.
    /// Error cases:
    /// - authority, deployment, or command coverage is missing or inconsistent
    pub(crate) fn require_audit(&self, signer: AccountId) -> anyhow::Result<()> {
        self.require_recovery(self.domain.context("command origin audit requires a deployment")?, signer)
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
    fn an_unsealed_span_cannot_be_laundered_by_a_later_sealed_command() {
        let key = EnclaveKey::test_key();
        let signer = AccountId(key.address());
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
        let mut origin = Origin::new(Some(domain), Some(signer));
        origin.command(&command(0)).unwrap();
        let mut later = command(1);
        later.seal = Some(key.seal_command(domain, command_link(origin.link, &later).unwrap()));
        origin.command(&later).unwrap();
        assert_eq!(
            origin.require_recovery(domain, signer).unwrap_err().to_string(),
            "unauthenticated command tail starts at seq 0"
        );
    }

    #[test]
    fn current_key_sealed_tail_is_recoverable_without_its_next_checkpoint() {
        let key = EnclaveKey::test_key();
        let signer = AccountId(key.address());
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
        let mut origin = Origin::new(Some(domain), Some(signer));
        let mut entry = command(0);
        let expected = command_link(origin.link, &entry).unwrap();
        entry.seal = Some(key.seal_command(domain, expected));
        origin.command(&entry).unwrap();
        origin.require_recovery(domain, signer).unwrap();
        assert_eq!(origin.link, expected);
        assert!(origin.require_recovery(domain, AccountId([1; 20])).is_err());
        let mut audit = Origin::default();
        audit.command(&entry).unwrap();
        assert!(audit.require_recovery(domain, signer).is_err());
    }

    #[test]
    fn a_different_key_seal_cannot_authorize_an_unanchored_tail() {
        let key = EnclaveKey::test_key();
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
        let mut origin = Origin::new(Some(domain), Some(AccountId([1; 20])));
        let mut entry = command(0);
        entry.seal = Some(key.seal_command(domain, command_link(origin.link, &entry).unwrap()));
        origin.command(&entry).unwrap();
        assert_eq!(
            origin.require_recovery(domain, AccountId([1; 20])).unwrap_err().to_string(),
            "unauthenticated command tail starts at seq 0"
        );
    }
}
