//! Public, bounded access declarations and individual custody-hold evidence.

use axum::Json;
use axum::extract::{Path, State};
use engine_core::access::{EligibilityClass, HeldDeposit};
use engine_core::types::AccountId;
use serde::Serialize;
use tracing::Span;
use tracing::field::display;

use crate::api::{ApiError, ApiResult, ApiState};

#[derive(Serialize)]
pub(super) struct EligibilityView {
    class: Option<EligibilityClass>,
    statement: Option<&'static str>,
}

/// The recorded declaration is separate from the account's funded balance.
/// Error cases:
/// - malformed address or unavailable sequencer
pub(super) async fn eligibility(
    State(state): State<ApiState>,
    Path(address): Path<String>,
) -> ApiResult<EligibilityView> {
    let account: AccountId = address.parse().map_err(|_| ApiError::BadRequest("invalid account".into()))?;
    Span::current().record("id", display(account));
    let class = state.query(move |engine| engine.eligibility(account)).await?;
    Ok(Json(EligibilityView { class, statement: class.map(EligibilityClass::statement) }))
}

/// Returns one uncredited custody hold without scanning or exposing an unbounded list.
/// Error cases:
/// - missing hold or unavailable sequencer
pub(super) async fn hold(State(state): State<ApiState>, Path(chain_ref): Path<String>) -> ApiResult<HeldDeposit> {
    Span::current().record("id", display(&chain_ref));
    let hold = state.query(move |engine| engine.held_deposits().get(&chain_ref).cloned()).await?;
    hold.map(Json).ok_or(ApiError::NotFound)
}

/// One uncredited deposit pending after a source failure, from the authoritative public state.
/// Error cases:
/// - missing pending deposit or unavailable sequencer
pub(super) async fn pending(
    State(state): State<ApiState>,
    Path(chain_ref): Path<String>,
) -> ApiResult<engine_core::access::AccessDeposit> {
    Span::current().record("id", display(&chain_ref));
    state
        .query(move |engine| engine.pending_deposits().get(&chain_ref).cloned())
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}
