//! Access-source alert episodes from authenticated engine decisions.

use std::collections::BTreeMap;

use engine_core::Engine;
use engine_core::access::{AccessEvidence, AccessReject};
use engine_core::command::Command;
use engine_core::deposits::{DepositReceipt, DepositUpdate, RecordedDeposit};
use engine_core::event::{Event, Reject};
use engine_core::types::{AccountId, Millis};
use tracing::{info, warn};

#[derive(Default)]
pub(super) struct AccessAlerts {
    failed: bool,
    /// The sequenced time of the open episode's latest failure. A verified decision observed before it,
    /// such as one the API kept from before the failure, says nothing of the source since, so it
    /// resolves nothing; within the freshness bound every accepted decision is observed after it.
    since: Millis,
}

/// What one command's own screening said about the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SourceHealth {
    Failed(AccessReject),
    Verified,
}

impl AccessAlerts {
    /// Restores an unresolved outage so the first fresh decision resolves it after restart.
    pub(super) fn recovered(failed: bool) -> Self {
        Self { failed, since: Millis::MIN }
    }

    /// Whether a source failure episode is open.
    pub(super) fn failed(&self) -> bool {
        self.failed
    }

    /// Applies the episode change a command's own screening gives, without alerting. Run on the
    /// engine after `events` were applied.
    pub(super) fn observe(&mut self, engine: &Engine, cmd: &Command, events: &[Event]) -> Option<SourceHealth> {
        let health = is_access_command(cmd).then(|| source_health(engine, cmd, events)).flatten();
        match health {
            Some(SourceHealth::Failed(_)) => (self.failed, self.since) = (true, engine.now()),
            Some(SourceHealth::Verified) if self.failed && predates(engine, cmd, events, self.since) => return None,
            Some(SourceHealth::Verified) => self.failed = false,
            None => {}
        }
        health
    }

    /// Raises a source failure episode and resolves it after a verified fresh source decision.
    pub(super) fn note(&mut self, engine: &Engine, cmd: &Command, events: &[Event]) {
        let was_failed = self.failed;
        match self.observe(engine, cmd, events) {
            Some(SourceHealth::Failed(reason)) => {
                warn!(alert = "access_source_failed", "access source refused: {reason:?}; request remains unexecuted");
            }
            Some(SourceHealth::Verified) if was_failed => {
                info!(alert_resolved = "access_source_failed", "authenticated fresh access evidence is available");
            }
            _ => {}
        }
    }
}

/// The source health the access command `cmd` itself established. Only events that record this
/// command's own verified screening count: an approval of an older queued withdrawal, a retained
/// credit and an inherited hold say nothing about the source. From `V83` a reorganisation screens each
/// of its moves, and any move whose screening failed outweighs those that passed.
fn source_health(engine: &Engine, cmd: &Command, events: &[Event]) -> Option<SourceHealth> {
    let replaced = replaced_receipts(engine, cmd);
    let health = |event: &Event| match event {
        Event::EligibilityAttested { .. }
        | Event::OrderAccepted { .. }
        | Event::PositionLevelsSet { .. }
        | Event::WithdrawalQueued { .. }
        | Event::Deposited { .. }
        | Event::HouseCapitalMoved { .. } => Some(SourceHealth::Verified),
        // A receipt credited on screening; adoption moves no principal and screens nothing.
        Event::CustodyDepositChanged { receipt, uncredited: None, principal_delta, .. }
            if principal_delta.is_positive() && !retained_credit(&replaced, &receipt.reference()) =>
        {
            Some(SourceHealth::Verified)
        }
        Event::DepositHeld { chain_ref, .. } if inherited_hold(&replaced, chain_ref) => None,
        Event::DepositHeld { reason, .. } | Event::DepositPending { reason, .. } => source_result(*reason),
        Event::EligibilityRejected { reason, .. }
        | Event::OrderRejected { reason, .. }
        | Event::WithdrawalRejected { reason, .. } => match reason {
            Reject::Access { reason } => source_result(*reason),
            Reject::MissingSignature
            | Reject::InvalidSignature
            | Reject::InvalidNonce { .. }
            | Reject::Expired { .. }
            | Reject::ExpiryTooFar { .. }
            | Reject::DuplicateReference
            | Reject::InvalidSessionKey
            | Reject::SessionKeyRevoked
            | Reject::SessionKeyExpired { .. }
            | Reject::SessionKeyScope
            | Reject::SessionKeyUnsupported
            | Reject::BatchUnsupported
            | Reject::LevelsUnsupported
            | Reject::TooManySessionKeys { .. } => None,
            _ => Some(SourceHealth::Verified),
        },
        _ => None,
    };
    if matches!(cmd, Command::CustodyReorganisation { .. }) {
        let mut results = events.iter().filter_map(health);
        let first = results.next()?;
        return Some(results.find(|result| matches!(result, SourceHealth::Failed(_))).unwrap_or(first));
    }
    events.iter().find_map(health)
}

/// Whether the screening that made `cmd`'s result verified was observed before `since`, the latest
/// failure, so it says nothing of the source since. A reorganisation's result is verified by the moves
/// whose own screening passed, found by their replacement's reference, so it predates `since` unless
/// one of them carries a decision observed at or after it.
fn predates(engine: &Engine, cmd: &Command, events: &[Event], since: Millis) -> bool {
    let Command::CustodyReorganisation { moves } = cmd else {
        return observed_at(cmd).is_some_and(|at| at < since);
    };
    let observed_since = |reference: &str| {
        moves.iter().any(|move_| {
            move_.replacement.reference() == reference
                && matches!(&move_.evidence, Some(AccessEvidence::Screened(screening)) if screening.observed_at >= since)
        })
    };
    !events.iter().any(|event| {
        let reference = match event {
            Event::CustodyDepositChanged { receipt, .. } => receipt.reference(),
            Event::DepositHeld { chain_ref, .. } | Event::DepositPending { chain_ref, .. } => chain_ref.clone(),
            _ => return false,
        };
        source_health(engine, cmd, std::slice::from_ref(event)) == Some(SourceHealth::Verified)
            && observed_since(&reference)
    })
}

/// When the source observed the decision `cmd` carries, if it carries one.
fn observed_at(cmd: &Command) -> Option<Millis> {
    let evidence = match cmd {
        Command::AttestEligibility { evidence, .. }
        | Command::AccessOrder { evidence, .. }
        | Command::AccessCloseAll { evidence, .. }
        | Command::AccessSetPositionLevels { evidence, .. }
        | Command::AccessWithdraw { evidence, .. }
        | Command::AccessDeposit { evidence, .. }
        | Command::AccessCustodyDeposit { evidence, .. }
        | Command::AccessCustodyReplacement { evidence, .. }
        | Command::RetryDepositScreening { evidence, .. } => evidence,
        _ => return None,
    };
    match evidence {
        AccessEvidence::Screened(screening) => Some(screening.observed_at),
        AccessEvidence::Failed { .. } => None,
    }
}

/// Each replaced receipt of a screened replacement, as recorded after it applied, by its replacement's
/// reference, and whether its payer is the replacement's: one for `AccessCustodyReplacement`, and from
/// `V83` one for each move of a reorganisation.
fn replaced_receipts<'a>(engine: &'a Engine, cmd: &Command) -> BTreeMap<String, (&'a RecordedDeposit, bool)> {
    let moves: Vec<(&DepositReceipt, &DepositReceipt, Option<AccountId>)> = match cmd {
        Command::AccessCustodyReplacement { update, payer, .. } => match &**update {
            DepositUpdate::Replace { receipt, replacement, .. } => vec![(receipt, replacement, *payer)],
            _ => Vec::new(),
        },
        Command::CustodyReorganisation { moves } => {
            moves.iter().map(|move_| (&move_.receipt, &move_.replacement, move_.payer)).collect()
        }
        _ => Vec::new(),
    };
    moves
        .into_iter()
        .filter_map(|(receipt, replacement, payer)| {
            let orphan = engine.recorded_deposit(&receipt.reference())?;
            Some((replacement.reference(), (orphan, payer.is_some() && orphan.payer == payer)))
        })
        .collect()
}

/// Whether a replacement kept a credited receipt's principal for its unchanged authenticated
/// payer, which the engine does whatever the replacement's screening said.
fn retained_credit(replaced: &BTreeMap<String, (&RecordedDeposit, bool)>, replacement: &str) -> bool {
    replaced.get(replacement).is_some_and(|(orphan, same_payer)| *same_payer && orphan.uncredited.is_none())
}

/// Whether a replacement's hold was inherited from the receipt it replaced, not screened afresh.
fn inherited_hold(replaced: &BTreeMap<String, (&RecordedDeposit, bool)>, replacement: &str) -> bool {
    replaced
        .get(replacement)
        .is_some_and(|(orphan, _)| orphan.uncredited.is_some_and(|held| held.reason.holds_deposit()))
}

/// Whether `cmd` is one of the access-screened commands, whose refusals feed the source episode.
pub(super) fn is_access_command(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::AttestEligibility { .. }
            | Command::AccessOrder { .. }
            | Command::AccessCloseAll { .. }
            | Command::AccessSetPositionLevels { .. }
            | Command::AccessWithdraw { .. }
            | Command::AccessDeposit { .. }
            | Command::AccessCustodyDeposit { .. }
            | Command::AccessCustodyReplacement { .. }
            | Command::CustodyReorganisation { .. }
            | Command::RetryDepositScreening { .. }
    )
}

/// Whether a custody gate that admits no withdrawals, as while collateral is unreconciled or the
/// vault halts claims, admits `cmd`, forcing a wrapped access withdrawal to record its refusal
/// durably without debiting collateral while any other withdrawal waits. A node with no gate
/// admits every command.
pub(super) fn gate_withdrawal(cmd: &mut Command, gate_admits: Option<bool>) -> bool {
    if gate_admits != Some(false) || !matches!(cmd, Command::Withdraw { .. } | Command::AccessWithdraw { .. }) {
        return true;
    }
    warn!(alert = "withdrawals_paused", "withdrawal refused until custody reconciles and the vault admits claims");
    match cmd {
        Command::AccessWithdraw { payout_available, .. } => {
            *payout_available = false;
            true
        }
        _ => false,
    }
}

/// Distinguishes provider failures from an available source's excluding decision.
fn source_result(reason: AccessReject) -> Option<SourceHealth> {
    match reason {
        reason if reason.is_source_failure() => Some(SourceHealth::Failed(reason)),
        // A payer-less deposit is held before any evidence is read.
        AccessReject::DepositHeld
        | AccessReject::DepositPending
        | AccessReject::PayerMissing
        | AccessReject::ProtocolInactive
        | AccessReject::AlreadyAttested => None,
        _ => Some(SourceHealth::Verified),
    }
}

#[cfg(test)]
mod tests;
