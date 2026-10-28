//! Receipt transitions built from a revision that a screening retry may supersede before they are
//! sequenced.

use anyhow::anyhow;
use engine_core::deposits::{DepositReceipt, DepositReject, DepositStatus, DepositUpdate, RecordedDeposit};
use engine_core::event::Event;
use tokio::sync::mpsc;

use crate::custody::credit::query;
use crate::custody::stall::PollError;
use crate::sequencer::Request;

/// The receipt revision (the sequence of its last change) `update` was built from.
pub(super) fn observed_revision(update: &DepositUpdate) -> Option<u64> {
    match update {
        DepositUpdate::Credit { .. } => None,
        DepositUpdate::Finalise { observed_at_seq, .. }
        | DepositUpdate::Reverse { observed_at_seq, .. }
        | DepositUpdate::Replace { observed_at_seq, .. } => Some(*observed_at_seq),
        DepositUpdate::Reinstate { reversed_at_seq, .. } => Some(*reversed_at_seq),
    }
}

/// The reason a transition was refused, if it was.
pub(super) fn refused(events: &[Event]) -> Option<DepositReject> {
    events.iter().find_map(|event| match event {
        Event::CustodyDepositRejected { reason, .. } => Some(reason.clone()),
        _ => None,
    })
}

/// What a refused transition means. A stale-revision refusal of a receipt that has since changed
/// is a race with a screening retry, which credits or holds a pending receipt between the watcher's
/// read and its transition, so the next poll reloads it. Any other refusal, or a stale one of a
/// receipt that has not changed, contradicts the chain data and stalls as inconsistent.
/// Error cases:
/// - the sequencer stops while the receipt is reloaded
pub(super) async fn refusal_stall(
    tx: &mpsc::Sender<Request>,
    receipt: &DepositReceipt,
    observed: Option<u64>,
    reason: &DepositReject,
    what: &str,
) -> PollError {
    if let (DepositReject::SettledReceipt, Some(observed)) = (reason, observed) {
        let reference = receipt.reference();
        match query(tx, move |engine| engine.recorded_deposit(&reference).map(|recorded| recorded.updated_at_seq)).await
        {
            Ok(Some(current)) if current != observed => {
                return PollError::unsettled(anyhow!(
                    "{what}: receipt changed at seq {current} after revision {observed}"
                ));
            }
            Ok(_) => {}
            Err(error) => return PollError::Fatal(error),
        }
    }
    PollError::inconsistent(anyhow!("{what}"))
}

/// Moves an orphan to its canonical replacement in one transition, naming the replacement's own
/// reversal when that exact receipt was credited and reversed before.
/// Error cases:
/// - the sequencer stops, or the replacement is already live beside its orphan
pub(super) async fn replacement_update(
    tx: &mpsc::Sender<Request>,
    orphan: &RecordedDeposit,
    replacement: DepositReceipt,
) -> Result<DepositUpdate, PollError> {
    let reference = replacement.reference();
    let current =
        query(tx, move |engine| engine.recorded_deposit(&reference).cloned()).await.map_err(PollError::Fatal)?;
    let replacement_reversed_at_seq = match current {
        None => None,
        Some(recorded) if recorded.status == DepositStatus::Reversed => Some(recorded.updated_at_seq),
        Some(_) => return Err(PollError::inconsistent(anyhow!("a replacement receipt is live beside its orphan"))),
    };
    Ok(DepositUpdate::Replace {
        receipt: orphan.receipt.clone(),
        observed_at_seq: orphan.updated_at_seq,
        replacement,
        replacement_reversed_at_seq,
    })
}
