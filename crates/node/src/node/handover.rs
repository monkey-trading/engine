//! The incumbent's election watcher: alerts while an election naming another operator is mirrored
//! and unhanded.

use std::time::Duration;

use engine_core::types::Millis;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};

use crate::sequencer::Request;
use crate::trusted_time::TrustedClock;
use engine_core::types::AccountId;

/// The alert episode an incumbent's election implies: none while the election names this operator
/// or is cancelled, `elected` once another operator is named, and `overdue` past the registry's
/// effective time — this node still running is then the proof no handover happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Episode {
    None,
    Elected,
    Overdue,
}

/// Maps the mirrored election and trusted time to the episode, pure so the escalation is testable.
pub(super) fn episode(
    last: Option<engine_core::election::Election>,
    operator: AccountId,
    now: Option<Millis>,
) -> Episode {
    let Some(last) = last.filter(|last| !last.cancelled && last.admin_key != operator) else {
        return Episode::None;
    };
    // The registry's effective time is when the vault names the successor's release: from then a
    // handover can complete, so the incumbent is overdue once trusted time passes it, whatever the
    // engine's own deferral logged later.
    match now.is_some_and(|now| now >= last.registry_effective_at) {
        true => Episode::Overdue,
        false => Episode::Elected,
    }
}

/// Watches the mirrored election an incumbent with sealed keys runs under: raises `operator_elected`
/// (Warning) once an election names another operator, and `handover_overdue` (Critical) once trusted
/// time passes the registry's effective time with this node still running, on any transition into
/// `Overdue`. A cancellation, or an election naming this operator's key again, resolves both alerts
/// with the fields they fired under, so alert routing can close the episodes.
/// Error cases:
/// - none; it runs until the node stops
pub(super) async fn watch_handover(
    tx: mpsc::Sender<Request>,
    operator: AccountId,
    trusted: TrustedClock,
    every: Duration,
) -> anyhow::Result<()> {
    let mut current = Episode::None;
    let mut fired: Option<engine_core::election::Election> = None;
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        let (reply, answered) = oneshot::channel();
        if tx
            .send(Request::Query(Box::new(move |engine| {
                let _ = reply.send(engine.operator_election().last);
            })))
            .await
            .is_err()
        {
            return Ok(());
        }
        let last = match answered.await {
            Ok(last) => last,
            Err(_) => return Ok(()),
        };
        let challenger = last.filter(|last| !last.cancelled && last.admin_key != operator);
        let next = episode(last, operator, trusted.now_ms());
        match (current, next) {
            (Episode::None, Episode::Elected) => {
                if let Some(last) = challenger {
                    warn!(
                        alert = "operator_elected",
                        epoch = last.epoch,
                        elected = %last.candidate,
                        effective_at = last.registry_effective_at,
                        "an election names another operator: a successor enclave is expected to connect at                          this node's handover endpoint by the effective time"
                    );
                    fired = Some(last);
                }
            }
            (from, Episode::Overdue) if from != Episode::Overdue => {
                if let Some(last) = challenger {
                    error!(
                        alert = "handover_overdue",
                        epoch = last.epoch,
                        effective_at = last.registry_effective_at,
                        "the election's effective time passed with no successor confirmed: this node still                          holds the keys the elected operator's enclave should have taken; check its                          handover endpoint and archive reachability"
                    );
                    fired = Some(last);
                }
            }
            (Episode::Elected | Episode::Overdue, Episode::None) => {
                if let Some(last) = fired.take() {
                    // The resolutions carry the fields the alerts fired under, so routing closes
                    // the same episodes.
                    let epoch = last.epoch;
                    let elected = last.candidate;
                    let effective_at = last.registry_effective_at;
                    info!(
                        alert_resolved = "operator_elected",
                        epoch,
                        elected = %elected,
                        effective_at,
                        "the election naming another operator was cancelled or superseded: this operator stays in effect"
                    );
                    if current == Episode::Overdue {
                        info!(
                            alert_resolved = "handover_overdue",
                            epoch, effective_at, "the overdue handover episode ends with the election's end"
                        );
                    }
                }
            }
            _ => {}
        }
        current = next;
    }
}
