//! `GET /v1/attestation` and `GET /v1/attestation/collateral`.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::{ApiError, ApiState};
use crate::tee::Attestation;

/// The attestation produced at boot.
pub(super) async fn attestation(State(s): State<ApiState>) -> Json<Attestation> {
    Json((*s.attestation).clone())
}

/// The vendor certificate chain and revocation list a verifier needs for the attestation's quote,
/// as the node last fetched them. It carries no per-account data, and its integrity rests on the
/// vendor roots the verifier pins, not on the node.
/// Error cases:
/// - the attestation has no collateral (404)
/// - no fetch has succeeded yet (503)
pub(super) async fn attestation_collateral(State(s): State<ApiState>) -> Result<Response, ApiError> {
    let served = s.collateral.as_ref().ok_or(ApiError::NotFound)?;
    let Some(body) = served.borrow().clone() else {
        let pending = Json(json!({ "error": "attestation collateral not fetched yet" }));
        return Ok((StatusCode::SERVICE_UNAVAILABLE, [(RETRY_AFTER, "60")], pending).into_response());
    };
    Ok(([(CONTENT_TYPE, "application/json")], body).into_response())
}
