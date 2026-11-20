//! How a replay checks its command seals: every seal in full, or every seal's form and in full only the
//! first and last seal of each run, as `engine replay` does. A run is the longest stretch of consecutive
//! commands within one checkpoint interval whose seals name one signer and domain. Its last seal signs a
//! link that chains every command, stamp and event set since the interval's start, which the previous
//! checkpoint's seq and root begin, so a valid last seal authenticates every command of the run as one its
//! signer chained. The first seal is checked too, so a run whose seals were relabelled to name the signer of
//! the run after it, merging the two, fails at its first seal, as one relabelled to name the signer of the
//! run before it fails at its last. The seals between are checked only for form: the link leaves seals out.
//! A run ends at a checkpoint, at a command whose seal names another signer or domain or has none, and where
//! the replay ends or fails, so it never spans a change of the signer its seals name.

use anyhow::Context;
use engine_core::eip712::{B256, Signature};
use engine_core::types::AccountId;

use crate::log::command_seal::signs;
use crate::log::{CheckpointDomain, CommandSeal, SealsAhead};

/// How a replay checks its command seals.
pub(super) enum Seals {
    /// Every seal in full before its command applies: in place, or ahead of the replay (`SealsAhead`).
    Every(Option<SealsAhead>),
    /// Every seal's form, and each run's first seal in full when the run opens and its last when it ends.
    RunEnds(Runs),
}

/// The run a replay checking run ends has open.
#[derive(Default)]
pub(super) struct Runs {
    open: Option<Run>,
    /// How many seals were checked in full: each run's first, and its last when it is another.
    #[cfg(test)]
    checked: u64,
}

/// A run: its first and last command's seq, how many seals it holds, and the last command's seal and link,
/// whose first seal has been checked in full.
struct Run {
    first: u64,
    last: u64,
    /// How many seals the run holds, counted apart from the seqs, which the engine has yet to check.
    seals: u64,
    domain: CheckpointDomain,
    signer: AccountId,
    signature: Signature,
    link: B256,
}

impl Default for Seals {
    /// Every seal in full, in place.
    fn default() -> Self {
        Seals::Every(None)
    }
}

impl Seals {
    /// Checks `seal`, the seal of the command at `seq` whose link is `link`: in full, or, at run ends,
    /// its form, the open run ending first when `seal` names another signer or domain, and in full when it
    /// opens a run.
    /// Error cases:
    /// - the seal is invalid (`CommandSeal::verify`) or, at run ends, malformed (`CommandSeal::well_formed`)
    /// - the run it ends does not verify (`end_run`), or it opens a run and does not verify
    pub(super) fn check(&mut self, seq: u64, seal: &CommandSeal, link: B256) -> anyhow::Result<()> {
        match self {
            Seals::Every(Some(ahead)) => ahead.verify(seq, seal, link),
            Seals::Every(None) => seal.verify(link),
            Seals::RunEnds(runs) => runs.take(seq, seal, link),
        }
    }

    /// Ends the open run, if any, checking its last seal in full.
    /// Error cases:
    /// - the run's last seal does not verify over its link
    pub(super) fn end_run(&mut self) -> anyhow::Result<()> {
        match self {
            Seals::RunEnds(runs) => runs.end(),
            Seals::Every(_) => Ok(()),
        }
    }

    /// Discards the open run unchecked (`Runs::drop_run`).
    pub(super) fn drop_run(&mut self) {
        if let Seals::RunEnds(runs) = self {
            runs.drop_run();
        }
    }

    /// Whether a run is open, its last seal unchecked.
    pub(super) fn run_open(&self) -> bool {
        matches!(self, Seals::RunEnds(Runs { open: Some(_), .. }))
    }

    /// How many seals were checked in full.
    #[cfg(test)]
    pub(super) fn seals_checked(&self) -> u64 {
        match self {
            Seals::RunEnds(runs) => runs.checked,
            Seals::Every(_) => 0,
        }
    }
}

impl Runs {
    /// Takes `seal`, the seal of the command at `seq` whose link is `link`, into the open run once its
    /// form is checked, ending the open run first when `seal` names another signer or domain; a seal that
    /// opens a run is checked in full here.
    /// Error cases:
    /// - the seal is malformed (`CommandSeal::well_formed`), or the run it ends does not verify (`end`)
    /// - it opens a run and does not verify
    fn take(&mut self, seq: u64, seal: &CommandSeal, link: B256) -> anyhow::Result<()> {
        let signature = seal.well_formed().with_context(|| format!("the seal at seq {seq}"))?;
        if self.open.as_ref().is_some_and(|run| (run.signer, run.domain) != (seal.signer, seal.domain)) {
            self.end()?;
        }
        let (first, seals) = match &self.open {
            Some(run) => (run.first, run.seals + 1),
            None => {
                self.verify(seal.domain, seal.signer, &signature, link)
                    .with_context(|| format!("the seal at seq {seq} by {}, the first of its run", seal.signer))?;
                (seq, 1)
            }
        };
        let (domain, signer) = (seal.domain, seal.signer);
        self.open = Some(Run { first, last: seq, seals, domain, signer, signature, link });
        Ok(())
    }

    /// Ends the open run, if any, checking its last seal in full over its link unless it is the first, which
    /// opening the run checked.
    /// Error cases:
    /// - the seal does not verify: a command of the run, its stamp or events differ from what the seal's
    ///   signer chained, or the seal does
    fn end(&mut self) -> anyhow::Result<()> {
        let Some(Run { first, last, seals, domain, signer, signature, link }) = self.open.take() else {
            return Ok(());
        };
        if seals == 1 {
            return Ok(());
        }
        self.verify(domain, signer, &signature, link).with_context(|| {
            format!(
                "the seal at seq {last} by {signer}, which ends the run from seq {first}, does not verify over the \
                 commands since the last checkpoint: one of them, its stamp or its events differ from what \
                 {signer} sealed, or the seal does"
            )
        })
    }

    /// Discards the open run unchecked: a fault before it is the one the replay reports.
    fn drop_run(&mut self) {
        self.open = None;
    }

    /// Checks in full that `signer` signed `link` in `domain` with `signature`.
    /// Error cases:
    /// - it did not
    fn verify(
        &mut self,
        domain: CheckpointDomain,
        signer: AccountId,
        signature: &Signature,
        link: B256,
    ) -> anyhow::Result<()> {
        #[cfg(test)]
        {
            self.checked += 1;
        }
        anyhow::ensure!(signs(domain, signer, signature, link), "invalid command origin signature");
        Ok(())
    }
}
