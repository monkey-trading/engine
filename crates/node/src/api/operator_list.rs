//! `GET /v1/operators`: the registry's approved candidates with their stake and role, and its switch,
//! parameters and operators, as the custody quorum read them at one final block.

use axum::Json;
use axum::extract::State;

use crate::api::{ApiError, ApiResult, ApiState};
use crate::custody::{OperatorList, Unavailable};

/// The operator list the last read made, and the block and time it names.
/// Error cases:
/// - no `custody.evm.staking.registry.lens` is configured (404)
/// - no read has succeeded yet, or the last is older than `custody.evm.finality_stalled_after` (503)
pub(super) async fn operators(State(s): State<ApiState>) -> ApiResult<OperatorList> {
    let feed = s.operators.as_ref().ok_or(ApiError::NotFound)?;
    match feed.current() {
        Ok(list) => Ok(Json((*list).clone())),
        Err(Unavailable::NotRead) => Err(ApiError::NotReady("the operator list has not been read yet")),
        Err(Unavailable::Stale) => Err(ApiError::NotReady("the operator list is stale: the last read is too old")),
    }
}
