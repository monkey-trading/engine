//! `GET /v1/accounts/{address}/financing/{seq}`: from `V80`, an account's financing at each rollover
//! the command at `seq` ran, with the path from its leaf to the root that rollover's
//! `financing_applied` logs, so a client checks the `financing` rows its history lists against the
//! public log without the other accounts' rows. The node answers once the command is durable in the
//! log, from the derived rows it keeps for the newest commands (`history::derived`), and for an older
//! command within the history's retention from the rows the history stored, once they give the root
//! the log holds (`RowsStore`).

use axum::Json;
use axum::extract::{Path, State};
use engine_core::eip712::B256;
use engine_core::financing::FinancingProof;
use engine_core::types::{AccountId, Millis};
use serde::Serialize;
use tokio::task::spawn_blocking;
use tracing::field::display;
use tracing::{Span, debug, error, warn};

use crate::api::{ApiError, ApiResult, ApiState};
use crate::history::derived::Derived;
use crate::log::Offset;

/// An account's financing at the rollovers of one command.
#[derive(Debug, Serialize)]
pub struct FinancingView {
    pub seq: u64,
    pub rollovers: Vec<RolloverView>,
}

/// One rollover's root, as its `financing_applied` logs it, and the account's place under it.
#[derive(Debug, Serialize)]
pub struct RolloverView {
    /// Where the rollover's `financing_applied` sits in the log, `<seq>:<index>`.
    pub event: Offset,
    pub at: Millis,
    pub root: B256,
    /// The account's charges at the rollover and their path to `root`; none when it charged the
    /// account nothing.
    pub proof: Option<FinancingProof>,
}

/// `address`'s financing at each rollover of the command at `seq`, with its proofs.
/// Error cases:
/// - the address is malformed
/// - the command is not durable, ran no rollover that charged anything, or its rows are neither kept
///   nor stored as the log holds them
pub async fn financing(
    State(s): State<ApiState>,
    Path((address, seq)): Path<(String, u64)>,
) -> ApiResult<FinancingView> {
    let account: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(account));
    // The sequencer keeps a command's rows from its stamp: only a durable one's are answered.
    let durable = s.manifest.borrow().flushed.is_some_and(|flushed| flushed.seq >= seq);
    if !durable {
        return Err(ApiError::NotFound);
    }
    let kept = s.history.derived().borrow().get(seq).cloned();
    let (stored, dir, manifest) = (s.history.stored().cloned(), s.log_dir.clone(), s.manifest.clone());
    // Stored reads go one at a time, and wait for their turn holding no shared permit.
    let turn = match (&kept, &stored) {
        (None, Some(store)) => Some(store.turn().await),
        _ => None,
    };
    // The first read of a rollover builds its tree over every row, and a stored one reads the log and
    // the rows' file, so it runs on a blocking thread, admitted by a permit the log's other reads share.
    let permit = s.backfills.clone().acquire_owned().await.map_err(|_| ApiError::Internal)?;
    let span = Span::current();
    let answer = spawn_blocking(move || {
        let (_held, _turn, _span) = (permit, turn, span.enter());
        let found = kept.or_else(|| {
            stored?.derived_at(&dir, manifest, seq).unwrap_or_else(|e| {
                // A command before the first this node's start verified answers no rows, as one not on this host does.
                match e.downcast_ref::<crate::log::NotLocal>() {
                    Some(_) => debug!(seq, "the financing read is before what this node serves: {e:#}"),
                    None => warn!(
                        alert = "financing_rows_rejected",
                        seq, "the financing read cannot prove the stored rows: {e:#}"
                    ),
                }
                None
            })
        });
        found.map(|derived| view(seq, &derived, account))
    });
    let view = answer.await.map_err(|e| {
        error!("the financing read's task failed: {e:#}");
        ApiError::Internal
    })?;
    Ok(Json(view.ok_or(ApiError::NotFound)?))
}

/// `account`'s financing at each rollover of `derived`, the command at `seq`'s rows.
fn view(seq: u64, derived: &Derived, account: AccountId) -> FinancingView {
    let rollovers = derived
        .aggregates
        .iter()
        .zip(derived.trees())
        .map(|(aggregate, tree)| RolloverView {
            event: Offset { seq, index: aggregate.index },
            at: aggregate.at,
            root: aggregate.root,
            proof: tree.proof(derived.rows_of(aggregate), account),
        })
        .collect();
    FinancingView { seq, rollovers }
}

#[cfg(test)]
mod tests;
