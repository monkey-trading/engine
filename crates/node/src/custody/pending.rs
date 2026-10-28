//! Log-derived retries of pending deposits run independently of vault ingestion and never revisit
//! held funds.

use std::time::Duration;

use engine_core::access::{AccessEvidence, AccessReject};
use engine_core::command::Command;
use engine_core::eip712::deposit_screening_hash;
use engine_core::event::Event;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tracing::{Instrument, debug, info, info_span, warn};

use crate::access::{AccessClient, ScreeningRequest, screen};
use crate::custody::credit::{query, submit};
use crate::custody::stall::PollError;
use crate::sequencer::Request;

pub(super) const RETRY_BATCH: usize = 8;
/// The wait between turns while retries sequence nothing that stays pending.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// The longest wait while sequenced evidence keeps failing verification, which bounds how fast
/// retries that change nothing can grow the public log.
pub(super) const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(300);

/// What one retry turn found.
pub(crate) struct RetryTurn {
    /// A sequenced retry left its deposit pending, so the next turn waits longer.
    pub(super) unsettled: bool,
    /// A screening failed, or a sequenced retry left its deposit pending.
    pub(super) failing: bool,
}

/// One attempt to settle a pending deposit.
enum Attempt {
    /// The screening failed, so nothing was sequenced.
    Failed,
    /// The source cannot classify a subject yet, which leaves the deposit pending, so nothing was
    /// sequenced.
    Unclassified,
    /// The sequenced retry left the deposit pending.
    Unsettled,
    /// The retry settled the deposit, or the sequencer refused its stamp until a later turn.
    Done,
}

/// The episode in which pending deposits stay uncredited because their retries keep failing.
#[derive(Default)]
pub(super) struct RetryAlert {
    failing: bool,
}

impl RetryAlert {
    /// Raises the alert on the first failing turn and resolves it on the next turn that does not fail.
    pub(super) fn note(&mut self, failing: bool) {
        match (failing, self.failing) {
            (true, false) => {
                warn!(alert = "access_deposit_retry_failed", "pending deposits stay uncredited: screening retries fail")
            }
            (false, true) => {
                info!(alert_resolved = "access_deposit_retry_failed", "pending deposit screening retries succeed again")
            }
            _ => {}
        }
        self.failing = failing;
    }
}

/// Restarts with the authoritative pending set from replay; only the fair scan position is transient.
/// Error cases:
/// - the sequencer stops or its durable submission fails
pub(super) async fn run(access: Option<&AccessClient>, tx: mpsc::Sender<Request>) -> anyhow::Result<()> {
    let mut after = None;
    let mut wait = RETRY_INTERVAL;
    let mut alert = RetryAlert::default();
    loop {
        sleep(wait).await;
        let turn = retry_batch(access, &tx, &mut after).await?;
        wait = next_wait(wait, !turn.unsettled);
        alert.note(turn.failing);
    }
}

/// Resets the wait after a turn that sequenced nothing left pending, and doubles it otherwise.
pub(super) fn next_wait(wait: Duration, settled: bool) -> Duration {
    if settled { RETRY_INTERVAL } else { (wait * 2).min(MAX_RETRY_INTERVAL) }
}

/// Screens at most one bounded page per turn, without holding the ingestion cursor, skipping
/// deposits whose account has not declared yet. A failed screening, or one that still cannot
/// classify a subject, changes nothing, so it is not sequenced. The turn ends once a sequenced retry
/// leaves its deposit pending, so the caller backs off.
/// Error cases:
/// - the sequencer stops or its durable submission fails
pub(crate) async fn retry_batch(
    access: Option<&AccessClient>,
    tx: &mpsc::Sender<Request>,
    after: &mut Option<String>,
) -> anyhow::Result<RetryTurn> {
    let pivot = after.clone().unwrap_or_default();
    let batch = query(tx, move |engine| engine.retryable_pending(&pivot, RETRY_BATCH)).await?;
    let mut turn = RetryTurn { unsettled: false, failing: false };
    for (reference, deposit) in batch {
        *after = Some(reference.clone());
        let span = info_span!("deposit_screening_retry", id = %reference);
        let attempt = async {
            let evidence = screen(
                access,
                &ScreeningRequest {
                    request_hash: deposit_screening_hash(&deposit),
                    account: deposit.account,
                    payer: deposit.payer,
                    peer: None,
                },
            )
            .await;
            let unclassified = match &evidence {
                AccessEvidence::Failed { reason } => {
                    debug!(?reason, "screening still failing; nothing sequenced");
                    return Ok(Attempt::Failed);
                }
                AccessEvidence::Screened(screening) => screening.sanctions_refusal(deposit.account.is_some(), true),
            };
            if let Some(reason @ (AccessReject::AccountUnknown | AccessReject::PayerUnknown)) = unclassified {
                debug!(?reason, "source cannot classify the deposit yet; nothing sequenced");
                return Ok(Attempt::Unclassified);
            }
            match submit(tx, Command::RetryDepositScreening { chain_ref: reference, evidence }).await {
                Ok(events) if events.iter().any(|event| matches!(event, Event::DepositPending { .. })) => {
                    Ok(Attempt::Unsettled)
                }
                Ok(_) | Err(PollError::Stalled(_)) => Ok(Attempt::Done),
                Err(PollError::Fatal(error)) => Err(error),
            }
        }
        .instrument(span)
        .await?;
        match attempt {
            Attempt::Failed => turn.failing = true,
            Attempt::Unsettled => return Ok(RetryTurn { unsettled: true, failing: true }),
            Attempt::Unclassified | Attempt::Done => {}
        }
    }
    Ok(turn)
}
