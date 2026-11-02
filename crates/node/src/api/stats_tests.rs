//! The dashboard routes through the router, over a stand-in sequencer that answers queries from an
//! engine the test built.

use std::net::SocketAddr;

use axum::body::{Body, to_bytes};
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::sequencer::Request as SequencerRequest;
use crate::stats::tests::{BTC, Rig, T0};

/// The router over `rig`'s engine and record, and the stand-in sequencer's task.
fn serve(rig: Rig) -> (axum::Router, tokio::task::JoinHandle<()>) {
    let (mut state, mut requests) = crate::api::tests::api_state(true);
    state.stats = rig.stats.clone();
    let owner = tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let SequencerRequest::Query(query) = request else { panic!("the dashboard routes only read") };
            query(&rig.engine);
        }
    });
    let app = crate::api::router(state).layer(MockConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()));
    (app, owner)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, Value) {
    let response = app.clone().oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, headers, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn positions_wait_for_the_first_scan_and_then_name_the_market() {
    let (app, owner) = serve(Rig::new());
    let (status, headers, body) = get(&app, &format!("/v1/house/positions?market={BTC}")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(headers.get("retry-after").unwrap(), "5");
    let (status, ..) = get(&app, "/v1/house/positions?market=777").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "an unlisted market is refused before any scan matters");
    let (status, ..) = get(&app, "/v1/house/positions").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "the market is required");
    owner.abort();
}

#[tokio::test]
async fn the_routes_refuse_what_they_cannot_serve_and_answer_the_rest() {
    let mut rig = Rig::new();
    rig.price(T0 + 61_000, 100_100);
    let (app, owner) = serve(rig);
    let (status, _, body) = get(&app, &format!("/v1/house/positions?market={BTC}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["market"], BTC);
    for refused in [
        "/v1/stats/series?metric=long&resolution=1m",
        "/v1/stats/series?metric=nav&resolution=1s",
        "/v1/stats/series?metric=nav&resolution=1m&limit=0",
        "/v1/stats/series?metric=nav&resolution=1m&extra=1",
        "/v1/monkey/activity?before=nope",
        "/v1/monkey/activity?limit=500",
    ] {
        assert_eq!(get(&app, refused).await.0, StatusCode::BAD_REQUEST, "{refused}");
    }
    let (status, _, series) = get(&app, "/v1/stats/series?metric=nav&resolution=1m").await;
    assert_eq!((status, series["from_genesis"].clone()), (StatusCode::OK, true.into()));
    let (status, _, monkey) = get(&app, "/v1/monkey").await;
    assert_eq!((status, monkey["emissions"]["period_ms"].clone()), (StatusCode::OK, 600_000.into()));
    let (status, _, house) = get(&app, "/v1/house").await;
    assert_eq!(status, StatusCode::OK);
    assert!(house["cash"].is_string() && house["markets"].is_array() && house["snapshot_ms"].is_i64(), "{house}");
    owner.abort();
}
