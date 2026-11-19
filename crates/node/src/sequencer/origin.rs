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
    /// The key that sealed the command at `first_untrusted`, if it carries a seal.
    first_untrusted_by: Option<AccountId>,
    /// Legacy staking references were not in the SMT; checkpoint root coverage cannot attest them.
    unbound_staking: Option<u64>,
    historical_staking: Option<(u64, CheckpointDomain, AccountId)>,
    authorized_signers: BTreeSet<AccountId>,
    /// A copy of another node's log: every command a checkpoint covers must carry a seal by an accepted
    /// signer, as its writer wrote it (see `copy`).
    copy: bool,
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

    /// `with_attested` in `domain`, for a copy of another node's log, which an archive or a predecessor
    /// supplied: every command must carry a seal by one of `attested` unless it is in the copy's tail,
    /// which no checkpoint covers. The writer seals every command, and a seal is part of the command's
    /// identity in the cumulative command prefix, so a copy that dropped or replaced one a checkpoint
    /// covers is refused rather than taken with another prefix than its writer's. A tail from a command
    /// with no seal, or one by a key not accepted (`first_untrusted`), is the caller's to refuse, as a
    /// handover and a takeover do (`require_attested`), or to cut back to the last checkpoint, as a
    /// follower's replica does, for the stream to refill.
    pub(crate) fn copy(domain: CheckpointDomain, attested: BTreeSet<AccountId>) -> Self {
        Self { copy: true, ..Self::with_attested(Some(domain), attested) }
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
        if !trusted && self.first_untrusted.is_none() {
            self.first_untrusted = Some(command.sequenced.seq);
            self.first_untrusted_by = command.seal.as_ref().map(|seal| seal.signer);
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
    /// - in a copy, it covers a command sealed by no accepted signer
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
            if let Some(seq) = self.first_untrusted.filter(|_| self.copy) {
                bail!(
                    "the command at seq {seq} carries no seal by an accepted signer, and the checkpoint at seq {} \
                     covers it: a copy must hold every seal its writer wrote",
                    checkpoint.seq
                );
            }
            self.authorized_signers.insert(checkpoint.signer.parse()?);
            if let Some((seq, sealed_domain, signer)) = self.historical_staking.take()
                && (sealed_domain != domain || !self.authorized_signers.contains(&signer))
            {
                self.unbound_staking.get_or_insert(seq);
            }
            self.domain = Some(domain);
            (self.first_untrusted, self.first_untrusted_by) = (None, None);
        }
        Ok(())
    }

    /// The first command since the last authenticated checkpoint that carries no seal by an accepted
    /// signer, if any, with the key that sealed it, if one did: in a copy, where its tail that no accepted
    /// signer vouches for starts (see `copy`).
    pub(crate) fn first_untrusted(&self) -> Option<(u64, Option<AccountId>)> {
        self.first_untrusted.map(|seq| (seq, self.first_untrusted_by))
    }

    /// Adds `signer` to the attested authority: the standby key a promotion made the vault's signer,
    /// which the promotion, not a checkpoint, authenticates (#595).
    pub(crate) fn admit(&mut self, signer: AccountId) {
        if let Some(attested) = self.attested.as_mut() {
            attested.insert(signer);
        }
    }

    /// Pins `signer`, one of the attested signers a live replica accepted, as this replay's recovery
    /// authority, as if recovery had replayed under it: sound only once no command follows the last
    /// checkpoint, which the caller requires `signer` to have signed, or a promotion that `admit`ted
    /// it authenticates the commands after it.
    /// Error cases:
    /// - the replay accepted no attested set naming `signer`, ran in another deployment, or a
    ///   command lacks authenticated coverage
    pub(crate) fn pin(&mut self, domain: CheckpointDomain, signer: AccountId) -> anyhow::Result<()> {
        ensure!(
            self.attested.as_ref().is_some_and(|attested| attested.contains(&signer)) && self.domain == Some(domain),
            "command origin attested authority does not name {signer} in this deployment"
        );
        if let Some(seq) = self.first_untrusted {
            bail!("unauthenticated command tail starts at seq {seq}");
        }
        (self.signer, self.attested) = (Some(signer), None);
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

    /// A copy of another node's log refuses a command with no seal, or one sealed by a key it does not
    /// accept, once a checkpoint covers it; a tail no checkpoint covers, an accepted signer's command
    /// after it or not, is left to the caller, which `first_untrusted` names with its first sealer, and
    /// which a handover's or a takeover's replay refuses and a follower's replica cuts back.
    #[test]
    fn a_copy_refuses_a_missing_or_foreign_seal_once_a_checkpoint_covers_it() {
        let (writer, foreign) = (EnclaveKey::test_key(), EnclaveKey::generate());
        let domain = CheckpointDomain { chain_id: 31337, vault: AccountId([7; 20]) };
        let accepted: BTreeSet<_> = [AccountId(writer.address())].into();
        let sealed = |origin: &Origin, seq: u64, key: Option<&EnclaveKey>| {
            let mut entry = command(seq);
            entry.seal = key.map(|key| key.seal_command(domain, command_link(origin.link, &entry).unwrap()));
            entry
        };
        let checkpoint = Checkpoint {
            parent: None,
            seq: 1,
            root: format!("0x{}", hex::encode([1; 32])),
            signature: String::new(),
            signer: writer.address_hex(),
            domain: Some(domain),
            deferred_withdrawals: false,
        };
        for (case, key) in [("no seal", None), ("a foreign seal", Some(&foreign))] {
            let mut origin = Origin::copy(domain, accepted.clone());
            origin.command(&sealed(&origin, 0, Some(&writer)), ProtocolVersion::LATEST).unwrap();
            let second = sealed(&origin, 1, key);
            origin
                .command(&second, ProtocolVersion::LATEST)
                .unwrap_or_else(|e| panic!("{case}: refused uncovered: {e:#}"));
            let sealer = key.map(|key| AccountId(key.address()));
            assert_eq!(origin.first_untrusted(), Some((1, sealer)), "{case}: the tail's start or its sealer");
            // The uncovered tail opens a follower's replica, and a handover's or takeover's replay refuses it.
            assert!(origin.require_attested(&accepted).is_err(), "{case}: an uncovered foreign tail passed");
            let refused = origin.checkpoint(&checkpoint, domain, true).unwrap_err();
            assert!(refused.to_string().contains("seq 1 carries no seal by an accepted signer"), "{case}: {refused:#}");
        }
        // A foreign seal an accepted signer's command follows is still the copy's uncovered tail.
        let mut origin = Origin::copy(domain, accepted.clone());
        origin.command(&sealed(&origin, 0, Some(&foreign)), ProtocolVersion::LATEST).unwrap();
        origin.command(&sealed(&origin, 1, Some(&writer)), ProtocolVersion::LATEST).unwrap();
        assert_eq!(origin.first_untrusted(), Some((0, Some(AccountId(foreign.address())))), "the tail's start");
        assert!(origin.require_attested(&accepted).is_err(), "an uncovered foreign seal passed");
        // The writer's own seals pass, covered or not.
        let mut origin = Origin::copy(domain, accepted.clone());
        for seq in 0..2 {
            origin.command(&sealed(&origin, seq, Some(&writer)), ProtocolVersion::LATEST).unwrap();
        }
        origin.checkpoint(&checkpoint, domain, true).unwrap();
        origin.require_attested(&accepted).unwrap();
    }
}
