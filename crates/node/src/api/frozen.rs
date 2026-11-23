//! A frozen node's API (`run --frozen`): every client request, a POST or DELETE but the operator's admin route
//! and a standby's vouch, is refused with 503, so nothing a client sends changes the log before a new log
//! carries its state. Reads and the stream answer as before, and the node goes on stamping prices, mirroring
//! the chain and posting checkpoints.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::limits::ApiLimits;

/// The refusal's message.
pub(crate) const FROZEN: &str = "the node is frozen for a move to a new vault and takes no client request";
/// The POST routes a frozen node still takes: the operator's, and a standby's.
const OPEN: [&str; 2] = ["/v1/admin", "/v1/standby/vouch"];

/// Refuses a client request while the node is frozen.
pub(super) async fn refuse_when_frozen(State(limits): State<Arc<ApiLimits>>, req: Request, next: Next) -> Response {
    let client_request = matches!(*req.method(), Method::POST | Method::DELETE) && !OPEN.contains(&req.uri().path());
    if limits.frozen && client_request {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": FROZEN }))).into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests;
