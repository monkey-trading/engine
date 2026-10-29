//! `GET /v1/attestation/collateral`: 404 when the quote has no collateral, 503 until the first
//! fetch succeeds, then the served JSON.

use super::*;

use axum::body::to_bytes;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;

/// The route's response, error or not.
async fn respond(state: ApiState) -> Response {
    match attestation_collateral(State(state)).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

#[tokio::test]
async fn the_collateral_route_answers_404_503_then_the_served_bytes() {
    let (mut state, _rx) = api_state(true);
    assert_eq!(respond(state.clone()).await.status(), StatusCode::NOT_FOUND, "a mock attestor has no collateral");

    let (tx, served) = watch::channel(None);
    state.collateral = Some(served);
    let pending = respond(state.clone()).await;
    assert_eq!(pending.status(), StatusCode::SERVICE_UNAVAILABLE, "before the first fetch");
    assert_eq!(pending.headers()[RETRY_AFTER], "60");
    let body = to_bytes(pending.into_body(), usize::MAX).await.unwrap();
    assert!(String::from_utf8_lossy(&body).contains("collateral not fetched yet"), "{body:?}");

    tx.send_replace(Some(axum::body::Bytes::from_static(b"{\"vcek\":\"0x01\"}")));
    let served = respond(state).await;
    assert_eq!(served.status(), StatusCode::OK);
    assert_eq!(served.headers()[CONTENT_TYPE], "application/json");
    assert_eq!(to_bytes(served.into_body(), usize::MAX).await.unwrap(), &b"{\"vcek\":\"0x01\"}"[..]);
}
