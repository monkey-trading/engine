//! Read-only origin validation shared by full replay and snapshot suffix replay.

mod seals;

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, bail, ensure};
use engine_core::command::Command;
use engine_core::eip712::B256;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::AccountId;

use crate::log::command_seal::{checkpoint_link, command_link};
use crate::log::{Checkpoint, CheckpointDomain, CommandEntry, ReplayWorkers, SealsAhead};
use crate::sequencer::origin::seals::{Runs, Seals};

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
    /// How seals are checked: every one in full, in place or ahead of the replay (`check_ahead`), or each
    /// run's last in full (`check_run_ends`).
    seals: Seals,
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

    /// Has the seals of the commands this replay applies from `from` on, in the log at `path`, checked
    /// ahead of them by `workers`, until `stop_checking_ahead` (`SealsAhead`): each is still checked
    /// before its command applies, with the same verdict.
    pub(crate) fn check_ahead(&mut self, path: &Path, from: u64, workers: ReplayWorkers) {
        self.seals = Seals::Every(SealsAhead::start(path, from, self.link, workers));
    }

    /// Checks every later seal's form, and in full only the first and last seal of each run of one signer's
    /// seals, as `engine replay` does (see `seals`), in place of every seal in full.
    pub(crate) fn check_run_ends(&mut self) {
        self.seals = Seals::RunEnds(Runs::default());
    }

    /// Checks every later seal in place, stopping the checks ahead (`check_ahead`).
    pub(crate) fn stop_checking_ahead(&mut self) {
        if let Seals::Every(ahead) = &mut self.seals {
            *ahead = None;
        }
    }

    /// Ends the open run, if any, checking its last seal in full (`check_run_ends`). A replay checking run
    /// ends calls it before it reports any later fault, after the root still due, so a run's failed seal
    /// comes in log order, and before it finishes.
    /// Error cases:
    /// - the run's last seal does not verify over its link
    pub(crate) fn end_run(&mut self) -> anyhow::Result<()> {
        self.seals.end_run()
    }

    /// Discards the open run unchecked (`check_run_ends`): a fault the replay found before it, which it
    /// reports, makes the run moot.
    pub(crate) fn drop_run(&mut self) {
        self.seals.drop_run();
    }

    /// Whether a run is open, its last seal unchecked (`check_run_ends`).
    pub(crate) fn run_open(&self) -> bool {
        self.seals.run_open()
    }

    /// How many seals' verdicts this replay took from the checks ahead (`check_ahead`).
    #[cfg(test)]
    pub(crate) fn seals_taken_ahead(&self) -> u64 {
        match &self.seals {
            Seals::Every(Some(ahead)) => ahead.taken_ahead(),
            _ => 0,
        }
    }

    /// How many seals this replay checked in full at run ends (`check_run_ends`).
    #[cfg(test)]
    pub(crate) fn seals_checked(&self) -> u64 {
        self.seals.seals_checked()
    }

    /// Checks a present seal before applying a command (`seals`) and retains every uncovered span.
    /// Error cases:
    /// - encoding, domain, events, or the present cryptographic seal is invalid, or at run ends malformed
    /// - at run ends, the run this command's seal, or its lack of one, ends does not verify
    pub(crate) fn command(&mut self, command: &CommandEntry, version: ProtocolVersion) -> anyhow::Result<()> {
        let link = command_link(self.link, command)?;
        let mut trusted = false;
        if let Some(seal) = &command.seal {
            ensure!(command.events.is_some(), "sealed command lacks logged events");
            ensure!(self.domain.is_none_or(|domain| domain == seal.domain), "command seal deployment mismatch");
            self.seals.check(command.sequenced.seq, seal, link)?;
            trusted = match &self.attested {
                Some(attested) => attested.contains(&seal.signer) && self.domain == Some(seal.domain),
                None => self.signer == Some(seal.signer) && self.domain == Some(seal.domain),
            };
        } else {
            // A command with no seal ends the run before it, whose last seal is then checked.
            self.seals.end_run()?;
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

    /// Advances only after every existing checkpoint authority and parent check passed, ending the run
    /// before it (`end_run`), since the chain restarts here.
    /// Error cases:
    /// - the run before it does not verify (`end_run`)
    /// - the checkpoint root is malformed, or its authenticated deployment conflicts
    /// - in a copy, it covers a command sealed by no accepted signer
    pub(crate) fn checkpoint(
        &mut self,
        checkpoint: &Checkpoint,
        domain: CheckpointDomain,
        authenticated: bool,
    ) -> anyhow::Result<()> {
        self.seals.end_run()?;
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
mod tests;
