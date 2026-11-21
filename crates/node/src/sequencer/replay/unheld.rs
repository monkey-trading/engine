//! The vault's checkpoint a log holds no entry for (#595): a promotion fixes the vault's latest
//! accepted checkpoint, which the outgoing key may have posted while no member received its entry. A
//! copy holding every command through it, whose state there has the vault's root, anchors there as if
//! it held the entry; and a checkpoint the outgoing key signed past it, which the vault never took,
//! anchors nothing.

use alloy::primitives::B256;
use engine_core::types::AccountId;

use crate::log::{Checkpoint, CheckpointDomain, LogEntry};
use crate::sequencer::replay::Replay;

/// What a replay notes for a promotion's copy.
#[derive(Debug, Default)]
pub(super) struct Noted {
    /// The signer of the last command's seal, if it carried one.
    last_sealer: Option<AccountId>,
    /// A superseded key's checkpoint the vault never accepted, which no command a later key sealed has
    /// followed yet: its seq and signer.
    superseded: Option<(u64, AccountId)>,
}

impl Replay {
    /// Checks a checkpoint `signer` signed, not the trusted recovery signer, against the vault's
    /// history: one the vault accepted must match it; a superseded key's checkpoint the vault never
    /// accepted, as a promotion leaves one past the checkpoint it fixed, can never be posted, so it
    /// anchors, extends and authenticates nothing, and only the seal chain runs through it. Returns
    /// whether it is that.
    /// Error cases:
    /// - it differs from the checkpoint the vault accepted at its seq, or no history covers its signer
    pub(super) fn superseded_checkpoint(
        &mut self,
        cp: &Checkpoint,
        signer: AccountId,
        domain: CheckpointDomain,
    ) -> anyhow::Result<bool> {
        match self.authorities.get(&cp.seq) {
            Some(authority) => authority.verify(cp).map(|()| false),
            None if self.superseded(signer) => {
                self.origin.checkpoint(cp, domain, false)?;
                self.checkpoints += 1;
                self.unheld.superseded.get_or_insert((cp.seq, signer));
                Ok(true)
            }
            None => anyhow::bail!("checkpoint signer differs from trusted recovery authority"),
        }
    }

    /// Whether the vault's history names `signer` in an epoch before the trusted signer's: a key the
    /// vault moved past, whose checkpoints it no longer takes.
    fn superseded(&self, signer: AccountId) -> bool {
        let last_epoch =
            |key: AccountId| self.signer_epochs.iter().filter(|(_, named)| **named == key).map(|(e, _)| *e).max();
        match (last_epoch(signer), self.trusted_signer.and_then(last_epoch)) {
            (Some(signer), Some(trusted)) => signer < trusted,
            _ => false,
        }
    }

    /// What the replay notes before it applies `entry`: at a command, the anchor at the vault's
    /// checkpoint the log holds no entry for, at the last command's state, and the command's sealer.
    /// Error cases:
    /// - the anchor's root cannot be computed (`anchor_unheld`)
    pub(super) fn before(&mut self, entry: &LogEntry) -> anyhow::Result<()> {
        if let LogEntry::Command(command) = entry {
            self.anchor_unheld()?;
            let sealer = command.seal.as_ref().map(|seal| seal.signer);
            if sealer.is_some_and(|sealer| self.unheld.superseded.is_some_and(|(_, signer)| signer != sealer)) {
                self.unheld.superseded = None;
            }
            self.unheld.last_sealer = sealer.or(self.unheld.last_sealer);
        }
        Ok(())
    }

    /// Anchors a recovery at the vault's finalized checkpoint when the log holds no entry for it: once
    /// the replay stands at its seq with nothing anchored and its state root is the vault's, the anchor
    /// is this state, under the signer the vault's history names for it or, without that history, the
    /// signer of the last command's seal. A tree on the replay's root thread comes back to the engine
    /// first.
    /// Error cases:
    /// - a root still due on the root thread differs from the log's, or the thread has stopped
    pub(super) fn anchor_unheld(&mut self) -> anyhow::Result<()> {
        let Some(origin) = self.recovery_origin.filter(|origin| origin.has_checkpoint) else { return Ok(()) };
        if self.recovery_anchor.is_some() || self.engine.last_seq() != Some(origin.seq) {
            return Ok(());
        }
        let Some(signer) =
            self.authorities.get(&origin.seq).map(|accepted| accepted.signer).or(self.unheld.last_sealer)
        else {
            return Ok(());
        };
        self.roots.take_back(&mut self.engine)?;
        if B256::from(self.engine.state_root()) != origin.root {
            return Ok(());
        }
        let checkpoint = unheld(origin.seq, origin.root, signer, self.domain);
        self.recovery_anchor_ts = Some(self.engine.now());
        self.recovery_anchor_work =
            Some(crate::checkpoints::Work::capture(checkpoint.clone(), self.accounting, &self.engine));
        self.recovery_anchor = Some(checkpoint);
        Ok(())
    }

    /// The signer of the last command's seal.
    pub(super) fn last_sealer(&self) -> Option<AccountId> {
        self.unheld.last_sealer
    }

    /// Takes `sealer` as the signer of the last command's seal, for a replay that goes on from a state it did
    /// not apply the log to, past commands whose seals another pass checked.
    pub(super) fn seed_last_sealer(&mut self, sealer: Option<AccountId>) {
        self.unheld.last_sealer = sealer;
    }

    /// Refuses a recovery whose log ends in a superseded key's checkpoint the vault never accepted
    /// with no command a later key sealed after it: a promotion's copy continues past such a checkpoint
    /// under the promoted key, while a log a rotation left behind ends there.
    /// Error cases:
    /// - such a checkpoint, as above
    pub(in crate::sequencer) fn ensure_superseded_continued(&self) -> anyhow::Result<()> {
        match self.unheld.superseded {
            Some((seq, signer)) => anyhow::bail!(
                "checkpoint signer differs from trusted recovery authority: {signer} signed the checkpoint at seq \
                 {seq}, which the vault never accepted, and no later key's command follows it"
            ),
            None => Ok(()),
        }
    }
}

/// The checkpoint the vault accepted at `seq` with `root` from `signer`, as an anchor holds it when the
/// log holds no entry for it: unsigned here, since only the vault's record of it is known.
pub(super) fn unheld(seq: u64, root: B256, signer: AccountId, domain: Option<CheckpointDomain>) -> Checkpoint {
    Checkpoint {
        parent: None,
        seq,
        root: format!("{root:#x}"),
        signature: String::new(),
        signer: format!("0x{}", hex::encode(signer.0)),
        domain,
        deferred_withdrawals: true,
    }
}
