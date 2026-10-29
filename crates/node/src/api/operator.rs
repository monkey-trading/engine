//! The operator key in effect, its pending rotation, and the operator registry as the log mirrors
//! it: whether rotation is on, and the last election.

use axum::Json;
use axum::extract::State;
use engine_core::election::Election;
use engine_core::operator_bounds::PendingRotation;
use engine_core::types::AccountId;
use serde::Serialize;

use crate::api::{ApiResult, ApiState};

/// What `GET /v1/operator` returns.
#[derive(Serialize)]
pub(super) struct OperatorView {
    /// The key whose signed admin commands apply now: the one the log records, or before it records
    /// one, the configured key.
    operator: AccountId,
    /// The key that takes over at its `effective_at`, by an election or, while rotation is off, a
    /// signed rotation.
    pending_rotation: Option<PendingRotation>,
    rotation_enabled: bool,
    last_election: Option<Election>,
}

/// The operator in effect, the pending rotation and its time, whether rotation is on, and the last
/// election, from the engine between batches.
/// Error cases:
/// - the sequencer has stopped
pub(super) async fn operator(State(s): State<ApiState>) -> ApiResult<OperatorView> {
    let configured = s.operator.address;
    let view = s
        .query(move |e| OperatorView {
            operator: e.operator().map_or(configured, |operator| operator.address),
            pending_rotation: e.operator_bounds().rotation,
            rotation_enabled: e.operator_election().rotation_enabled,
            last_election: e.operator_election().last,
        })
        .await?;
    Ok(Json(view))
}
