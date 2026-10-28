//! The account's cumulative USDC distribution with its proof against the last checkpoint's state
//! root, for the vault's `claimDistribution`.

use axum::Json;
use axum::extract::{Path, State};
use engine_core::merkle::DistributionProof;
use engine_core::types::AccountId;
use tracing::Span;
use tracing::field::display;

use crate::api::{ApiError, ApiResult, ApiState};

/// The proof of the account's cumulative USDC distribution against the state root of the last
/// checkpoint, as the vault's `claimDistribution` takes it. The vault checks it against its latest
/// posted root, so a claim waits until the vault's `latestSeq` is the proof's `seq`.
/// Error cases:
/// - the address is malformed
/// - no checkpoint holds a distribution for the account
/// - the sequencer has stopped
pub(super) async fn distributions(
    State(s): State<ApiState>,
    Path(address): Path<String>,
) -> ApiResult<DistributionProof> {
    let id: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(id));
    let proof = s.query(move |e| e.distribution_proof(id)).await?;
    proof.map(Json).ok_or(ApiError::NotFound)
}
