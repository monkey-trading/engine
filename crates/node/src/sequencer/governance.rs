//! Passed normal changes (`engine_core::governance`): the node sequences `ApplyProposal` for each whose
//! seven days' notice has run, ahead of the command it stamps then, so a change applies at the first
//! stamp past its time but a benchmark snapshot's, which installs its rates before any catch-up. The
//! engine refuses one before its time, so the node only chooses how soon. A passed activation of `V70`
//! is followed by the config's fault proof contract, as a start's activation is, unless the schedule's
//! governance record names another.

use alloy::primitives::Address;
use anyhow::bail;
use engine_core::command::Command;
use engine_core::event::Event;
use engine_core::governance::VotedChange;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;
use tracing::{error, info, warn};

use crate::sequencer::Sequencer;
use crate::sequencer::genesis::rejection;

impl Sequencer {
    /// Sequences `ApplyProposal` at `ts` for each passed change due by then, in the order mirrored,
    /// ahead of `next`, unless `next` is a benchmark snapshot or one is awaited: a change runs the
    /// catch-up a snapshot must precede, so it waits for the next command after the snapshot.
    /// Then, once `V70` is in force, records the config's fault proof contract if the log has none.
    /// Error cases:
    /// - as `stamp`
    /// - the engine keeps a change due after applying it, which would sequence it forever
    /// - the schedule's governance record names another fault proof contract (see `check_governance_record`)
    /// - the engine refuses the fault proof contract
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
        if let Some(record) =
            self.fault_proof_record().filter(|_| self.engine.protocol_version() >= ProtocolVersion::V70)
        {
            self.check_governance_record()?;
            if let Some(events) = self.stamp(ts, record)?
                && let Some(reason) = rejection(&events)
            {
                bail!("recording the fault proof contract: rejected: {reason}");
            }
        }
        Ok(())
    }

    /// Refuses to record the config's vote or fault proof contract while the schedule's governance record, as
    /// last read through the custody quorum at the start or by the custody watcher, names another: that log
    /// would refuse every start that names the recorded one.
    /// Error cases:
    /// - the record names another vote or fault proof contract than the config (alert
    ///   `governance_record_mismatch`)
    pub(super) fn check_governance_record(&self) -> anyhow::Result<()> {
        let Some(record) = &self.governance_record else { return Ok(()) };
        let vote = self.governance.map(|config| Address::from(config.vote.0));
        let fault_proof = self.fault_proof.map(|config| Address::from(config.contract.0));
        if let Some(reason) = record.borrow().refusal(vote, fault_proof) {
            error!(alert = "governance_record_mismatch", "refusing to record the configured contracts: {reason}");
            bail!("refusing to record the configured contracts: {reason}");
        }
        Ok(())
    }
}

/// An admin command's outcome among its `events`: `AdminApplied` or `AdminRejected`, which the
/// withdrawals the queue pays after the command follow.
pub(super) fn admin_outcome(events: &[Event]) -> Option<&Event> {
    events.iter().find(|event| matches!(event, Event::AdminApplied | Event::AdminRejected { .. }))
}
