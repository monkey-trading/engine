//! How a replay checks its command seals: every seal in full, or every seal's form and in full only the
//! last seal of each run, as `engine replay` does. A run is the longest stretch of consecutive commands
//! within one checkpoint interval whose seals name one signer and domain. Its last seal signs a link that
//! chains every command, stamp and event set since the interval's start, which the previous checkpoint's
//! seq and root begin, so a valid last seal authenticates every command of the run as its signer's. The
//! signature bytes of the run's other seals are the one thing it leaves unchecked: the link leaves seals
//! out. A run ends at a checkpoint, at a command whose seal names another signer or domain or has none,
//! and where the replay ends or fails, so it never spans a change of signer.

use anyhow::{Context, anyhow};
use engine_core::eip712::{B256, Signature};
use engine_core::types::AccountId;

use crate::log::command_seal::signs;
use crate::log::{CheckpointDomain, CommandSeal, SealsAhead};

/// How a replay checks its command seals.
pub(super) enum Seals {
    /// Every seal in full before its command applies: in place, or ahead of the replay (`SealsAhead`).
    Every(Option<SealsAhead>),
    /// Every seal's form, and each run's last seal in full when the run ends.
    RunEnds(Runs),
}

/// The run a replay checking run ends has open.
#[derive(Default)]
pub(super) struct Runs {
    open: Option<Run>,
    /// How many runs' last seals were checked in full.
    #[cfg(test)]
    checked: u64,
}

/// A run: its first and last command's seq, and the last command's seal and link.
struct Run {
    first: u64,
    last: u64,
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
    /// its form, the open run ending first when `seal` names another signer or domain.
    /// Error cases:
    /// - the seal is invalid (`CommandSeal::verify`) or, at run ends, malformed (`CommandSeal::well_formed`)
    /// - the run it ends does not verify (`end_run`)
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

    /// Whether a run is open, its last seal unchecked.
    pub(super) fn run_open(&self) -> bool {
        matches!(self, Seals::RunEnds(Runs { open: Some(_), .. }))
    }

    /// How many runs' last seals were checked in full.
    #[cfg(test)]
    pub(super) fn runs_checked(&self) -> u64 {
        match self {
            Seals::RunEnds(runs) => runs.checked,
            Seals::Every(_) => 0,
        }
    }
}

impl Runs {
    /// Takes `seal`, the seal of the command at `seq` whose link is `link`, into the open run once its
    /// form is checked, ending the open run first when `seal` names another signer or domain.
    /// Error cases:
    /// - the seal is malformed (`CommandSeal::well_formed`), or the run it ends does not verify (`end`)
    fn take(&mut self, seq: u64, seal: &CommandSeal, link: B256) -> anyhow::Result<()> {
        let signature = seal.well_formed().with_context(|| format!("the seal at seq {seq}"))?;
        if self.open.as_ref().is_some_and(|run| (run.signer, run.domain) != (seal.signer, seal.domain)) {
            self.end()?;
        }
        let first = self.open.as_ref().map_or(seq, |run| run.first);
        self.open = Some(Run { first, last: seq, domain: seal.domain, signer: seal.signer, signature, link });
        Ok(())
    }

    /// Ends the open run, if any, checking its last seal in full over its link.
    /// Error cases:
    /// - the seal does not verify: a command of the run, its stamp or events differ from what the seal's
    ///   signer chained, or the seal does
    fn end(&mut self) -> anyhow::Result<()> {
        let Some(Run { first, last, domain, signer, signature, link }) = self.open.take() else { return Ok(()) };
        #[cfg(test)]
        {
            self.checked += 1;
        }
        if signs(domain, signer, &signature, link) {
            return Ok(());
        }
        Err(anyhow!("invalid command origin signature")).context(format!(
            "the seal at seq {last} by {signer} does not verify over the commands from seq {first}: one of \
             them, its stamp or its events differ from what {signer} sealed, or the seal does"
        ))
    }
}
