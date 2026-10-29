//! Browser access: the origins `api.allowed_origins` lists may read the REST API (CORS) and open
//! the stream, which browsers do not subject to CORS, so the stream checks `Origin` itself. With no
//! origin listed the layer does nothing. The operator's `/v1/admin` route is never opened to a
//! browser. A preflight is answered without passing the rate limiter; it reads no state.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE, ORIGIN, VARY,
};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tracing::debug;

use crate::api::ApiError;
use crate::limits::ApiLimits;

const ADMIN_PATH: &str = "/v1/admin";
const STREAM_PATH: &str = "/v1/stream";
/// The response headers a page may read beyond the safelisted ones: the rate limiter's wait, and the
/// `X-Engine-*` headers a signed answer carries (`engine_wire`).
pub(super) const EXPOSED_HEADERS: &str =
    "Retry-After, x-engine-signature, x-engine-time, x-engine-seq, x-engine-challenge";
const ALLOWED_METHODS: &str = "GET, POST, OPTIONS";
const ALLOWED_HEADERS: &str = "Content-Type, X-Engine-Challenge";
/// Seconds a browser may cache a preflight answer.
const PREFLIGHT_MAX_AGE: &str = "600";

/// Gives a response to a listed origin its CORS headers, answers its preflight, and refuses a
/// stream upgrade from an unlisted one. Sits outside the rate limit and the body read, so their
/// refusals carry the headers too.
pub(super) async fn browser_origin(State(limits): State<Arc<ApiLimits>>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if !limits.has_allowed_origins() || path == ADMIN_PATH {
        return next.run(req).await;
    }
    let origin = req.headers().get(ORIGIN).cloned();
    let listed = origin.clone().filter(|o| o.to_str().is_ok_and(|o| limits.origin_allowed(o)));
    let mut response = if origin.is_some() && listed.is_none() && path == STREAM_PATH {
        debug!(origin = ?origin, "stream refused: origin not allowed");
        ApiError::Forbidden("origin not allowed").into_response()
    } else if listed.is_some() && req.method() == Method::OPTIONS && path.starts_with("/v1/") {
        preflight()
    } else {
        next.run(req).await
    };
    let headers = response.headers_mut();
    headers.append(VARY, HeaderValue::from_static("Origin"));
    if let Some(origin) = listed {
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        headers.insert(ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static(EXPOSED_HEADERS));
    }
    response
}

/// The 204 answer to a preflight from a listed origin.
fn preflight() -> Response {
    let headers = [
        (ACCESS_CONTROL_ALLOW_METHODS, ALLOWED_METHODS),
        (ACCESS_CONTROL_ALLOW_HEADERS, ALLOWED_HEADERS),
        (ACCESS_CONTROL_MAX_AGE, PREFLIGHT_MAX_AGE),
    ];
    (StatusCode::NO_CONTENT, headers).into_response()
}
