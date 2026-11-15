//! `POST /v1/standby/vouch`: the operator's enclave vouches for a candidate's standby key
//! (`standby_key::vouch::answer`), so the candidate can register it with the schedule. A refusal says
//! why; a node without the rule's chain access answers 404.

use axum::Json;
use axum::extract::State;

use crate::api::{ApiError, ApiState};
use crate::standby_key::vouch::{VouchReply, VouchRequest};

/// The vouch for the request's standby key.
/// Error cases:
/// - the node does not vouch (404), or the vouch is refused (400, naming why)
pub(super) async fn vouch(
    State(s): State<ApiState>,
    Json(request): Json<VouchRequest>,
) -> Result<Json<VouchReply>, ApiError> {
    let service = s.signing.vouching().ok_or(ApiError::NotFound)?;
    service.vouch(&request).await.map(Json).map_err(|e| ApiError::BadRequest(format!("{e:#}")))
}
