//! Passed normal changes (`engine_core::governance`): the node sequences `ApplyProposal` for each whose
//! seven days' notice has run, ahead of the command it stamps then, so a change applies at the first
//! stamp past its time but a benchmark snapshot's, which installs its rates before any catch-up. The
//! engine refuses one before its time, so the node only chooses how soon.

use anyhow::bail;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::types::Millis;
use tracing::{info, warn};

use crate::sequencer::Sequencer;

impl Sequencer {
    /// Sequences `ApplyProposal` at `ts` for each passed change due by then, in the order mirrored,
    /// ahead of `next`, unless `next` is a benchmark snapshot or one is awaited: a change runs the
    /// catch-up a snapshot must precede, so it waits for the next command after the snapshot.
    /// Error cases:
    /// - as `stamp`
    /// - the engine keeps a change due after applying it, which would sequence it forever
    pub(super) fn apply_due_changes(&mut self, ts: Millis, next: &Command) -> anyhow::Result<()> {
        if self.benchmark_sync || next.is_benchmark_batch() {
            return Ok(());
        }
        while let Some(due) = self.engine.due_change(ts) {
            let voted = VotedChange { proposal: due.proposal, command: due.command.clone() };
            let proposal = voted.proposal;
            let Some(events) = self.stamp(ts, Command::ApplyProposal(Box::new(voted)))? else { return Ok(()) };
            match admin_outcome(&events) {
                Some(Event::AdminApplied) => info!(proposal, "a passed change applied"),
                Some(Event::AdminRejected { reason }) => {
                    warn!(alert = "proposal_rejected", proposal, "a passed change did not apply: {reason}");
                }
                _ => warn!(proposal, "a passed change had no admin outcome: {events:?}"),
            }
            if self.engine.due_change(ts).is_some_and(|due| due.proposal == proposal) {
                bail!("the engine kept proposal {proposal} due after it was sequenced");
            }
        }
        Ok(())
    }
}

/// An admin command's outcome among its `events`: `AdminApplied` or `AdminRejected`, which the
/// withdrawals the queue pays after the command follow.
pub(super) fn admin_outcome(events: &[Event]) -> Option<&Event> {
    events.iter().find(|event| matches!(event, Event::AdminApplied | Event::AdminRejected { .. }))
}
