//! Log-derived retries of pending deposits run independently of vault ingestion and never revisit
//! held funds.

use std::ops::Bound;
use std::time::Duration;

use engine_core::Engine;
use engine_core::access::{AccessDeposit, AccessEvidence, AccessReject};
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
/// Retry turns between re-screens of deposits whose account has not declared, so they cost the
/// source at most one bounded page a minute while retries are quiet.
const UNDECLARED_EVERY: u32 = 12;
/// The most undeclared pending deposits one re-screen asks the source about.
pub(super) const UNDECLARED_BATCH: usize = RETRY_BATCH;
/// The most pending deposits one re-screen examines to find them.
pub(super) const UNDECLARED_SCAN: usize = 512;

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
    let (mut after, mut undeclared_after) = (None, None);
    let (mut wait, mut turns) = (RETRY_INTERVAL, 0u32);
    let mut alert = RetryAlert::default();
    loop {
        sleep(wait).await;
        let turn = retry_batch(access, &tx, &mut after).await?;
        turns = turns.wrapping_add(1);
        if turns % UNDECLARED_EVERY == 0 {
            rescreen_undeclared(access, &tx, &mut undeclared_after).await?;
        }
        wait = next_wait(wait, !turn.unsettled);
        alert.note(turn.failing);
    }
}

/// Resets the wait after a turn that sequenced nothing left pending, and doubles it otherwise.
pub(super) fn next_wait(wait: Duration, settled: bool) -> Duration {
    if settled { RETRY_INTERVAL } else { (wait * 2).min(MAX_RETRY_INTERVAL) }
}

/// Screens at most one bounded page per turn, without holding the ingestion cursor, skipping
/// deposits whose account has not declared yet (`rescreen_undeclared` covers those). A failed or
/// unverifiable screening, or one that still cannot classify a subject, changes nothing, so it is
/// not sequenced. The turn ends once a sequenced retry
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
            let evidence = screen_pending(access, &deposit).await;
            // The engine's own checks decide, before anything is sequenced: evidence that does not
            // verify is a failing source, and only a real "unknown" classification waits quietly.
            match refusal(tx, &deposit, &evidence).await? {
                Some(reason) if reason.is_source_failure() => {
                    debug!(?reason, "screening evidence does not verify; nothing sequenced");
                    return Ok(Attempt::Failed);
                }
                Some(reason @ (AccessReject::AccountUnknown | AccessReject::PayerUnknown)) => {
                    debug!(?reason, "source cannot classify the deposit yet; nothing sequenced");
                    return Ok(Attempt::Unclassified);
                }
                _ => {}
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

/// Screens a pending deposit's account and payer.
async fn screen_pending(access: Option<&AccessClient>, deposit: &AccessDeposit) -> AccessEvidence {
    screen(
        access,
        &ScreeningRequest {
            request_hash: deposit_screening_hash(deposit),
            account: deposit.account,
            payer: deposit.payer,
            peer: None,
        },
    )
    .await
}

/// What the engine's screening checks (source, request, signer, signature, freshness, sanctions)
/// make of `evidence` for `deposit` now, without sequencing anything.
/// Error cases:
/// - the sequencer stops
async fn refusal(
    tx: &mpsc::Sender<Request>,
    deposit: &AccessDeposit,
    evidence: &AccessEvidence,
) -> anyhow::Result<Option<AccessReject>> {
    let (deposit, evidence) = (deposit.clone(), evidence.clone());
    query(tx, move |engine| engine.screening_refusal(&deposit, &evidence)).await
}

/// Re-screens one bounded page of pending deposits whose account has not declared, which
/// `retry_batch` skips, and sequences a retry only for a verified confirmed sanction, so the deposit
/// is held and its alert fires. Anything else (a failing, unverifiable or unclassifying source, or
/// clear evidence) sequences nothing: an undeclared account stays pending until it declares.
/// Error cases:
/// - the sequencer stops or its durable submission fails
pub(super) async fn rescreen_undeclared(
    access: Option<&AccessClient>,
    tx: &mpsc::Sender<Request>,
    after: &mut Option<String>,
) -> anyhow::Result<()> {
    let pivot = after.clone().unwrap_or_default();
    let (batch, last) =
        query(tx, move |engine| undeclared_page(engine, &pivot, UNDECLARED_SCAN, UNDECLARED_BATCH)).await?;
    if last.is_some() {
        *after = last;
    }
    for (reference, deposit) in batch {
        let span = info_span!("undeclared_deposit_rescreen", id = %reference);
        async {
            let evidence = screen_pending(access, &deposit).await;
            match refusal(tx, &deposit, &evidence).await? {
                Some(reason) if reason.holds_deposit() => {
                    match submit(tx, Command::RetryDepositScreening { chain_ref: reference, evidence }).await {
                        Ok(_) | Err(PollError::Stalled(_)) => Ok(()),
                        Err(PollError::Fatal(error)) => Err(error),
                    }
                }
                reason => {
                    debug!(?reason, "no confirmed sanction for the undeclared deposit; nothing sequenced");
                    Ok(())
                }
            }
        }
        .instrument(span)
        .await?;
    }
    Ok(())
}

/// Up to `batch` pending deposits of undeclared accounts in reference order after `after`, wrapping
/// around, found by examining at most `scan` pending deposits, and the last reference examined, from
/// which the next page resumes.
pub(super) fn undeclared_page(
    engine: &Engine,
    after: &str,
    scan: usize,
    batch: usize,
) -> (Vec<(String, AccessDeposit)>, Option<String>) {
    let pending = engine.pending_deposits();
    let (mut found, mut last) = (Vec::new(), None);
    let order = pending
        .range::<str, _>((Bound::Excluded(after), Bound::Unbounded))
        .chain(pending.range::<str, _>((Bound::Unbounded, Bound::Included(after))));
    for (reference, deposit) in order.take(scan) {
        last = Some(reference.clone());
        if deposit.account.is_some_and(|account| engine.eligibility(account).is_none()) {
            found.push((reference.clone(), deposit.clone()));
            if found.len() == batch {
                break;
            }
        }
    }
    (found, last)
}
