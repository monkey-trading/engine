//! Every proof an account exits the vault with, against the last checkpoint's state root.

use axum::Json;
use axum::extract::{Path, State};
use engine_core::merkle::ExitProof;
use engine_core::types::AccountId;
use tracing::Span;
use tracing::field::display;

use crate::api::{ApiError, ApiResult, ApiState};

/// The exit terms and the account's balance and positions, approvals, queued requests,
/// distribution, and pending deposits, each with its proof against the state root of the last
/// checkpoint. The vault's exit checks them against its latest posted root, so they serve once the
/// vault's `latestSeq` is the proof's `seq`; `engine exit-proof` builds the same from the public log.
/// Error cases:
/// - the address is malformed
/// - no checkpoint under protocol `V38` exists yet
/// - the sequencer has stopped
pub(super) async fn exit(State(s): State<ApiState>, Path(address): Path<String>) -> ApiResult<ExitProof> {
    let id: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(id));
    let proof = s.query(move |e| e.exit_proof(id)).await?;
    proof.map(Json).ok_or(ApiError::NotFound)
}
