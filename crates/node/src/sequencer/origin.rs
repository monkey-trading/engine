//! Read-only origin validation shared by full replay and snapshot suffix replay.

use std::collections::BTreeSet;

use anyhow::{Context, bail, ensure};
use engine_core::command::Command;
use engine_core::eip712::B256;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::log::command_seal::{checkpoint_link, command_link};
use crate::log::{Checkpoint, CheckpointDomain, CommandEntry};

/// Tracks tentative replay spans until independent authority covers them.
#[derive(Default)]
pub(crate) struct Origin {
    pub(crate) link: B256,
    domain: Option<CheckpointDomain>,
    signer: Option<AccountId>,
    /// Independently attested signers a pure audit accepts in place of one pinned authority.
    attested: Option<BTreeSet<AccountId>>,
    first_untrusted: Option<u64>,
    /// Legacy staking references were not in the SMT; checkpoint root coverage cannot attest them.
    unbound_staking: Option<u64>,
    historical_staking: Option<(u64, CheckpointDomain, AccountId)>,
    authorized_signers: BTreeSet<AccountId>,
}

impl Origin {
    /// Pins the independent deployment and current signer used by checkpoint recovery.
    pub(crate) fn new(domain: Option<CheckpointDomain>, signer: Option<AccountId>) -> Self {
        Self { domain, signer, ..Self::default() }
    }

    /// Accepts the independently attested signers, one per signer epoch, instead of one pinned
    /// authority: a command is trusted when its seal is any of them.
    pub(crate) fn with_attested(domain: Option<CheckpointDomain>, attested: BTreeSet<AccountId>) -> Self {
        Self { domain, attested: Some(attested), ..Self::default() }
    }

    /// Checks a present seal before applying a command and retains every uncovered span.
    /// Error cases:
    /// - encoding, domain, events, or the present cryptographic seal is invalid
    pub(crate) fn command(&mut self, command: &CommandEntry, version: ProtocolVersion) -> anyhow::Result<()> {
        let link = command_link(self.link, command)?;
        let mut trusted = false;
        if let Some(seal) = &command.seal {
            ensure!(command.events.is_some(), "sealed command lacks logged events");
            ensure!(self.domain.is_none_or(|domain| domain == seal.domain), "command seal deployment mismatch");
            seal.verify(link)?;
            trusted = match &self.attested {
                Some(attested) => attested.contains(&seal.signer) && self.domain == Some(seal.domain),
                None => self.signer == Some(seal.signer) && self.domain == Some(seal.domain),
            };
        }
        if !trusted {
            self.first_untrusted.get_or_insert(command.sequenced.seq);
        }
        let staking_trusted = trusted
            || command
                .seal
                .as_ref()
                .is_some_and(|seal| self.domain == Some(seal.domain) && self.authorized_signers.contains(&seal.signer));
        if version < ProtocolVersion::V27
            && matches!(command.sequenced.cmd, Command::Stake { .. } | Command::Unstake { .. })
            && !staking_trusted
        {
            match &command.seal {
                None => {
                    self.unbound_staking.get_or_insert(command.sequenced.seq);
                }
                Some(seal) => {
                    let pending = (command.sequenced.seq, seal.domain, seal.signer);
                    if let Some((first, domain, signer)) = self.historical_staking {
                        if (domain, signer) != (seal.domain, seal.signer) {
                            self.unbound_staking.get_or_insert(first);
                        }
                    } else {
                        self.historical_staking = Some(pending);
                    }
                }
            }
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
            self.authorized_signers.insert(checkpoint.signer.parse()?);
            if let Some((seq, sealed_domain, signer)) = self.historical_staking.take()
                && (sealed_domain != domain || !self.authorized_signers.contains(&signer))
            {
                self.unbound_staking.get_or_insert(seq);
            }
            self.domain = Some(domain);
            self.first_untrusted = None;
        }
        Ok(())
    }

    /// Refuses writable recovery of legacy staking references without genuine command provenance.
    /// Error cases:
    /// - ordinary origin coverage is missing, or a legacy staking seal lacks independent authority
    pub(crate) fn require_recovery(&self, domain: CheckpointDomain, signer: AccountId) -> anyhow::Result<()> {
        self.require_coverage(domain, signer)?;
        self.require_staking_provenance()
    }

    /// Requires complete legacy staking provenance before selecting a writable snapshot suffix.
    /// Error cases:
    /// - a staking seal is missing, or its signer lacks independent authority in this prefix
    pub(crate) fn require_staking_provenance(&self) -> anyhow::Result<()> {
        if let Some(seq) = self.unbound_staking.or(self.historical_staking.map(|pending| pending.0)) {
            bail!("legacy staking command at seq {seq} lacks authenticated reference provenance");
        }
        Ok(())
    }

    /// Refuses unbound audit results and uncovered tails before writes, signing, or activation.
    /// Error cases:
    /// - recovery uses another authority, or any command lacks authenticated coverage
    fn require_coverage(&self, domain: CheckpointDomain, signer: AccountId) -> anyhow::Result<()> {
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
        self.require_coverage(self.domain.context("command origin audit requires a deployment")?, signer)
    }

    /// Authenticates a tail against the independently attested signer set: every command is
    /// sealed by one of them, so a host-appended unsealed tail cannot pass as verified.
    /// Error cases:
    /// - no deployment is recorded, or an unauthenticated command tail starts at `seq`
    pub(crate) fn require_attested(&self, attested: &BTreeSet<AccountId>) -> anyhow::Result<()> {
        ensure!(self.attested.as_ref() == Some(attested), "command origin attested authority mismatch");
        self.require_audit_untrusted()
    }

    /// Refuses an uncovered tail.
    /// Error cases:
    /// - a command after the last authenticated checkpoint was not sealed by an accepted signer
    fn require_audit_untrusted(&self) -> anyhow::Result<()> {
        if let Some(seq) = self.first_untrusted {
            bail!("unauthenticated command tail starts at seq {seq}");
        }
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
    fn an_unsealed_span_cannot_be_laundered_by_a_later_sealed_command() {
        let key = EnclaveKey::test_key();
        let signer = AccountId(key.address());
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
        let mut origin = Origin::new(Some(domain), Some(signer));
        origin.command(&command(0), ProtocolVersion::V2).unwrap();
        let mut later = command(1);
        later.seal = Some(key.seal_command(domain, command_link(origin.link, &later).unwrap()));
        origin.command(&later, ProtocolVersion::V2).unwrap();
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
        origin.command(&entry, ProtocolVersion::V2).unwrap();
        origin.require_recovery(domain, signer).unwrap();
        assert_eq!(origin.link, expected);
        assert!(origin.require_recovery(domain, AccountId([1; 20])).is_err());
        let mut audit = Origin::default();
        audit.command(&entry, ProtocolVersion::V2).unwrap();
        assert!(audit.require_recovery(domain, signer).is_err());
    }

    #[test]
    fn a_different_key_seal_cannot_authorize_an_unanchored_tail() {
        let key = EnclaveKey::test_key();
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
        let mut origin = Origin::new(Some(domain), Some(AccountId([1; 20])));
        let mut entry = command(0);
        entry.seal = Some(key.seal_command(domain, command_link(origin.link, &entry).unwrap()));
        origin.command(&entry, ProtocolVersion::V2).unwrap();
        assert_eq!(
            origin.require_recovery(domain, AccountId([1; 20])).unwrap_err().to_string(),
            "unauthenticated command tail starts at seq 0"
        );
    }
    #[test]
    fn a_historical_staking_seal_needs_its_own_authorized_checkpoint_signer() {
        let historical = EnclaveKey::test_key();
        let current = EnclaveKey::generate();
        let current_signer = AccountId(current.address());
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
        let mut origin = Origin::new(Some(domain), Some(current_signer));
        let mut entry = command(0);
        entry.sequenced.cmd = Command::Stake {
            account: AccountId([8; 20]),
            amount: engine_core::emissions::TokenUnits(1),
            block: 1,
            chain_ref: "old-stake".into(),
        };
        entry.seal = Some(historical.seal_command(domain, command_link(origin.link, &entry).unwrap()));
        origin.command(&entry, ProtocolVersion::V25).unwrap();
        let root = [1; 32];
        let checkpoint = Checkpoint {
            parent: None,
            seq: 0,
            root: format!("0x{}", hex::encode(root)),
            signature: historical.sign_checkpoint(domain.chain_id, &domain.vault.0, 0, &root),
            signer: historical.address_hex(),
            domain: Some(domain),
            deferred_withdrawals: false,
        };
        origin.checkpoint(&checkpoint, domain, true).unwrap();
        origin.require_recovery(domain, current_signer).unwrap();
    }
}
