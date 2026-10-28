//! Access-source alert episodes from authenticated engine decisions.

use engine_core::Engine;
use engine_core::access::AccessReject;
use engine_core::command::Command;
use engine_core::deposits::DepositUpdate;
use engine_core::event::{Event, Reject};
use tracing::{info, warn};

#[derive(Default)]
pub(super) struct AccessAlerts {
    failed: bool,
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
        Self { failed }
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
            Some(SourceHealth::Failed(_)) => self.failed = true,
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
/// credit and an inherited hold say nothing about the source.
fn source_health(engine: &Engine, cmd: &Command, events: &[Event]) -> Option<SourceHealth> {
    events.iter().find_map(|event| match event {
        Event::EligibilityAttested { .. }
        | Event::OrderAccepted { .. }
        | Event::WithdrawalQueued { .. }
        | Event::Deposited { .. }
        | Event::HouseCapitalMoved { .. } => Some(SourceHealth::Verified),
        // A receipt credited on screening; adoption moves no principal and screens nothing.
        Event::CustodyDepositChanged { uncredited: None, principal_delta, .. }
            if principal_delta.is_positive() && !retained_credit(engine, cmd) =>
        {
            Some(SourceHealth::Verified)
        }
        Event::DepositHeld { .. } if inherited_hold(engine, cmd) => None,
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
            | Reject::DuplicateReference => None,
            _ => Some(SourceHealth::Verified),
        },
        _ => None,
    })
}

/// The replaced receipt of a screened replacement, as recorded after it applied.
fn replaced_receipt<'a>(
    engine: &'a Engine,
    cmd: &Command,
) -> Option<(&'a engine_core::deposits::RecordedDeposit, bool)> {
    match cmd {
        Command::AccessCustodyReplacement { update, payer, .. } => {
            let DepositUpdate::Replace { receipt, .. } = &**update else { return None };
            let orphan = engine.recorded_deposit(&receipt.reference())?;
            Some((orphan, payer.is_some() && orphan.payer == *payer))
        }
        _ => None,
    }
}

/// Whether a replacement kept a credited receipt's principal for its unchanged authenticated
/// payer, which the engine does whatever the replacement's screening said.
fn retained_credit(engine: &Engine, cmd: &Command) -> bool {
    replaced_receipt(engine, cmd).is_some_and(|(orphan, same_payer)| same_payer && orphan.uncredited.is_none())
}

/// Whether a replacement's hold was inherited from the receipt it replaced, not screened afresh.
fn inherited_hold(engine: &Engine, cmd: &Command) -> bool {
    replaced_receipt(engine, cmd)
        .is_some_and(|(orphan, _)| orphan.uncredited.is_some_and(|held| held.reason.holds_deposit()))
}

/// Whether `cmd` is one of the access-screened commands, whose refusals feed the source episode.
pub(super) fn is_access_command(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::AttestEligibility { .. }
            | Command::AccessOrder { .. }
            | Command::AccessWithdraw { .. }
            | Command::AccessDeposit { .. }
            | Command::AccessCustodyDeposit { .. }
            | Command::AccessCustodyReplacement { .. }
            | Command::RetryDepositScreening { .. }
    )
}

/// Whether an unhealthy custody gate admits `cmd`, forcing a wrapped access withdrawal to record
/// its refusal durably without debiting collateral while any other withdrawal waits. A node with
/// no gate admits every command.
pub(super) fn gate_withdrawal(cmd: &mut Command, gate_healthy: Option<bool>) -> bool {
    if gate_healthy != Some(false) || !matches!(cmd, Command::Withdraw { .. } | Command::AccessWithdraw { .. }) {
        return true;
    }
    warn!(alert = "withdrawals_paused", "withdrawal refused until custody reconciles");
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
mod tests {
    use super::*;
    use engine_core::Fixed;
    use engine_core::access::{AccessEvidence, EligibilityClass, SourceFailure};
    use engine_core::types::AccountId;

    const ACCOUNT: AccountId = AccountId([1; 20]);

    /// A carrier access command: health reads the events, not the command's kind.
    fn command() -> Command {
        Command::RetryDepositScreening {
            chain_ref: "ref".into(),
            evidence: AccessEvidence::Failed { reason: SourceFailure::Unavailable },
        }
    }

    fn note(state: &mut AccessAlerts, events: &[Event]) {
        state.note(&Engine::default(), &command(), events);
    }

    fn refused(reason: AccessReject) -> Event {
        Event::WithdrawalRejected { account: ACCOUNT, amount: Fixed::ONE, reason: Reject::Access { reason } }
    }

    fn approved() -> [Event; 2] {
        [
            Event::WithdrawalApproved { account: ACCOUNT, amount: Fixed::ONE, nonce: 1 },
            Event::WithdrawalQueueUpdated { account: ACCOUNT, requested_seq: 1, remaining: Fixed::ZERO },
        ]
    }

    #[test]
    fn source_failure_episodes_survive_unauthenticated_requests_and_resolve_on_verified_decisions() {
        let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
        // Every client action screened at the API: an order, a withdrawal and a declaration.
        let refusals: [fn(Reject) -> Event; 3] = [
            |reason| Event::OrderRejected { account: AccountId([1; 20]), client_id: 1, reason },
            |reason| Event::WithdrawalRejected { account: AccountId([1; 20]), amount: Fixed::ONE, reason },
            |reason| Event::EligibilityRejected { account: AccountId([1; 20]), reason },
        ];
        for (episode, rejected) in (1..).zip(refusals) {
            let mut state = AccessAlerts::default();
            note(&mut state, &[rejected(Reject::Access { reason: AccessReject::SourceUnavailable })]);
            assert_eq!(logs.count("alert=\"access_source_failed\""), episode);
            assert!(state.failed);
            note(&mut state, &[rejected(Reject::InvalidSignature)]);
            assert!(state.failed, "a forged claimant says nothing about source recovery");
            note(&mut state, &[rejected(Reject::Access { reason: AccessReject::AlreadyAttested })]);
            assert!(state.failed, "a repeat declaration says nothing about source recovery");
            assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), episode - 1);
            note(&mut state, &[rejected(Reject::Access { reason: AccessReject::UnitedStates })]);
            assert!(!state.failed, "a verified excluding decision still proves the source recovered");
            note(&mut state, &[rejected(Reject::Access { reason: AccessReject::UnitedStates })]);
            assert!(!state.failed);
            assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), episode);
        }
        let mut state = AccessAlerts { failed: true };
        note(&mut state, &[Event::EligibilityAttested { account: ACCOUNT, class: EligibilityClass::Professional }]);
        assert!(!state.failed, "a recorded declaration is a verified fresh decision");
    }

    #[test]
    fn an_approval_of_an_older_queued_withdrawal_neither_resolves_nor_reopens_an_episode() {
        let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
        // The refused command also approves the FIFO head: its own result is the refusal.
        let mut state = AccessAlerts::default();
        let events = [vec![refused(AccessReject::SourceUnavailable)], approved().to_vec()].concat();
        note(&mut state, &events);
        assert!(state.failed, "the approval must not clear the outage the same command raised");
        assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), 0);
        // A command with no screening result of its own changes nothing, in either state.
        note(&mut state, &approved());
        assert!(state.failed);
        let mut healthy = AccessAlerts::default();
        note(&mut healthy, &approved());
        assert!(!healthy.failed);
        // Its own screened request is the verified result, queued or approved at once.
        for queued in [
            vec![Event::WithdrawalQueued { account: ACCOUNT, amount: Fixed::ONE, requested_seq: 2 }],
            [
                vec![Event::WithdrawalQueued { account: ACCOUNT, amount: Fixed::ONE, requested_seq: 2 }],
                approved().to_vec(),
            ]
            .concat(),
        ] {
            let mut state = AccessAlerts { failed: true };
            note(&mut state, &queued);
            assert!(!state.failed);
        }
        assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), 2);
    }

    #[test]
    fn a_payer_less_hold_leaves_the_source_episode_as_it_was() {
        let hold = |reason| Event::DepositHeld {
            chain_ref: "legacy".into(),
            account: Some(ACCOUNT),
            payer: None,
            amount: Fixed::ONE,
            reason,
        };
        let held = hold(AccessReject::PayerMissing);
        for failed in [true, false] {
            let mut state = AccessAlerts { failed };
            note(&mut state, std::slice::from_ref(&held));
            assert_eq!(state.failed, failed, "a hold decided before any evidence is read");
        }
        let sanctioned = hold(AccessReject::PayerSanctioned);
        let mut state = AccessAlerts { failed: true };
        note(&mut state, &[sanctioned]);
        assert!(!state.failed, "a verified sanction hold proves the source answered");
    }
}
