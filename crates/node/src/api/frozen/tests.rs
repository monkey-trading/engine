use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::Request as HttpRequest;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use tower::ServiceExt;

use super::*;
use crate::test_support::dev_config;

/// A router answering 200 on a read, a client request and the routes a frozen node keeps open, behind the gate.
fn app(frozen: bool) -> Router {
    let limits = Arc::new(ApiLimits::new(&dev_config().api).frozen(frozen));
    Router::new()
        .route("/v1/health", get(|| async { "ok" }))
        .route("/v1/orders", post(|| async { "ok" }))
        .route("/v1/session-keys", post(|| async { "ok" }).delete(|| async { "ok" }))
        .route("/v1/admin", post(|| async { "ok" }))
        .route("/v1/standby/vouch", post(|| async { "ok" }))
        .layer(from_fn_with_state(limits, refuse_when_frozen))
}

async fn status(frozen: bool, method: &str, path: &str) -> (StatusCode, String) {
    let request = HttpRequest::builder().method(method).uri(path).body(Body::empty()).unwrap();
    let response = app(frozen).oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 16).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn a_frozen_node_refuses_every_client_request_and_keeps_reads_and_the_operators_routes() {
    for (method, path) in [("POST", "/v1/orders"), ("POST", "/v1/session-keys"), ("DELETE", "/v1/session-keys")] {
        let (code, body) = status(true, method, path).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{method} {path}");
        assert!(body.contains(FROZEN), "{body}");
        assert_eq!(status(false, method, path).await.0, StatusCode::OK, "{method} {path} unfrozen");
    }
    for (method, path) in [("GET", "/v1/health"), ("POST", "/v1/admin"), ("POST", "/v1/standby/vouch")] {
        assert_eq!(status(true, method, path).await.0, StatusCode::OK, "{method} {path}");
    }
}

#[tokio::test]
async fn the_api_router_refuses_a_client_request_while_frozen() {
    use axum::extract::ConnectInfo;
    let (mut state, _requests) = crate::api::tests::api_state(true);
    state.limits = Arc::new(ApiLimits::new(&dev_config().api).frozen(true));
    let app = crate::api::router(state);
    let request = HttpRequest::builder()
        .method("POST")
        .uri("/v1/orders")
        .header("content-type", "application/json")
        .extension(ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 9))))
        .body(Body::from("{}"))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), 1 << 16).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains(FROZEN));
}
