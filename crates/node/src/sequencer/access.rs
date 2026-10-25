//! Access-source alert episodes from authenticated engine decisions.

use engine_core::access::AccessReject;
use engine_core::event::{Event, Reject};
use tracing::{info, warn};

#[derive(Default)]
pub(super) struct AccessAlerts {
    failed: bool,
}

impl AccessAlerts {
    /// Restores an unresolved outage so the first fresh decision resolves it after restart.
    pub(super) fn recovered(pending: bool) -> Self {
        Self { failed: pending }
    }

    /// Raises a source failure episode and resolves it after a verified fresh source decision.
    pub(super) fn note(&mut self, event: &Event) {
        let result = match event {
            Event::EligibilityAttested { .. }
            | Event::OrderAccepted { .. }
            | Event::WithdrawalApproved { .. }
            | Event::Deposited { .. }
            | Event::HouseCapitalMoved { .. } => Some(Ok(())),
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
                _ => Some(Ok(())),
            },
            _ => None,
        };
        match result {
            Some(Err(reason)) => {
                self.failed = true;
                warn!(alert = "access_source_failed", "access source refused: {reason:?}; request remains unexecuted");
            }
            Some(Ok(())) if std::mem::take(&mut self.failed) => {
                info!(alert_resolved = "access_source_failed", "authenticated fresh access evidence is available");
            }
            _ => {}
        }
    }
}

/// Distinguishes provider failures from an available source's excluding decision.
fn source_result(reason: AccessReject) -> Option<Result<(), AccessReject>> {
    match reason {
        reason if reason.is_source_failure() => Some(Err(reason)),
        AccessReject::DepositHeld
        | AccessReject::DepositPending
        | AccessReject::ProtocolInactive
        | AccessReject::AlreadyAttested => None,
        _ => Some(Ok(())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::Fixed;
    use engine_core::access::EligibilityClass;
    use engine_core::types::AccountId;

    #[test]
    fn source_failure_episodes_survive_unauthenticated_requests_and_resolve_on_verified_decisions() {
        let (logs, _guard) = crate::trusted_time::tests::logs::Logs::capture();
        let account = AccountId([1; 20]);
        // Every client action screened at the API: an order, a withdrawal and a declaration.
        let refusals: [fn(Reject) -> Event; 3] = [
            |reason| Event::OrderRejected { account: AccountId([1; 20]), client_id: 1, reason },
            |reason| Event::WithdrawalRejected { account: AccountId([1; 20]), amount: Fixed::ONE, reason },
            |reason| Event::EligibilityRejected { account: AccountId([1; 20]), reason },
        ];
        for (episode, rejected) in (1..).zip(refusals) {
            let mut state = AccessAlerts::default();
            state.note(&rejected(Reject::Access { reason: AccessReject::SourceUnavailable }));
            assert_eq!(logs.count("alert=\"access_source_failed\""), episode);
            assert!(state.failed);
            state.note(&rejected(Reject::InvalidSignature));
            assert!(state.failed, "a forged claimant says nothing about source recovery");
            state.note(&rejected(Reject::Access { reason: AccessReject::AlreadyAttested }));
            assert!(state.failed, "a repeat declaration says nothing about source recovery");
            assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), episode - 1);
            state.note(&rejected(Reject::Access { reason: AccessReject::UnitedStates }));
            assert!(!state.failed, "a verified excluding decision still proves the source recovered");
            state.note(&rejected(Reject::Access { reason: AccessReject::UnitedStates }));
            assert!(!state.failed);
            assert_eq!(logs.count("alert_resolved=\"access_source_failed\""), episode);
        }
        let mut state = AccessAlerts { failed: true };
        state.note(&Event::EligibilityAttested { account, class: EligibilityClass::Professional });
        assert!(!state.failed, "a recorded declaration is a verified fresh decision");
    }
}
