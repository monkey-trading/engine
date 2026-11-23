//! `GET /v1/awards` through the router, over a stand-in sequencer that answers queries from an engine the
//! test built.

use std::net::SocketAddr;

use axum::body::{Body, to_bytes};
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use super::*;
use crate::sequencer::Request as SequencerRequest;

/// An empty engine holding `awards`, by id, and the cap `cap` a vote set, if any.
fn engine_with(awards: Value, cap: Option<Value>) -> Engine {
    let mut state = serde_json::to_value(Engine::new()).unwrap();
    state["awards"] = awards;
    if let Some(cap) = cap {
        state["award_cap"] = cap;
    }
    serde_json::from_value(state).unwrap()
}

/// An award of `amount` USDC base units, `paid` of it so far.
fn award(proposal: u64, severity: &str, amount: u128, paid: u128) -> Value {
    json!({
        "proposal": proposal,
        "recipient": format!("0x{}", "7e".repeat(20)),
        "severity": severity,
        "advisory": format!("GHSA-{proposal}"),
        "amount": amount.to_string(),
        "paid": paid.to_string(),
        "approved_at": 1_800_000_000_000_i64 + proposal as i64,
    })
}

/// `GET /v1/awards` over `engine`.
async fn get(engine: Engine) -> (StatusCode, Value) {
    let (state, mut requests) = crate::api::tests::api_state(true);
    let owner = tokio::spawn(async move {
        while let Some(request) = requests.recv().await {
            let SequencerRequest::Query(query) = request else { panic!("the awards route only reads") };
            query(&engine);
        }
    });
    let app = crate::api::router(state).layer(MockConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()));
    let response = app.oneshot(Request::builder().uri("/v1/awards").body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    owner.abort();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn with_no_award_the_launch_cap_and_an_empty_queue_are_served() {
    let (status, body) = get(Engine::new()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let launch = json!({ "excess_bps": 500, "max": "50000000000" });
    assert_eq!(body, json!({ "cap": launch, "queue": [], "paid": [], "paid_total": "0" }));
}

#[tokio::test]
async fn the_queue_holds_what_is_owed_oldest_first_and_paid_awards_are_listed_apart() {
    let awards = json!({
        "0": award(5, "low", 1_000_000_000, 1_000_000_000),
        "1": award(6, "high", 50_000_000_000, 20_000_000_000),
        "2": award(9, "critical", 250_000_000_000, 0),
    });
    let cap = json!({ "excess_bps": 1_000, "max": "100000000000" });
    let (status, body) = get(engine_with(awards, Some(cap.clone()))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["cap"], cap);
    let ids = |list: &Value| list.as_array().unwrap().iter().map(|view| view["id"].clone()).collect::<Vec<_>>();
    assert_eq!(ids(&body["queue"]), [json!(1), json!(2)]);
    assert_eq!(ids(&body["paid"]), [json!(0)]);
    let front = &body["queue"][0];
    assert_eq!(
        (&front["proposal"], &front["severity"], &front["advisory"]),
        (&json!(6), &json!("high"), &json!("GHSA-6"))
    );
    assert_eq!(
        (&front["amount"], &front["paid"], &front["owed"]),
        (&json!("50000000000"), &json!("20000000000"), &json!("30000000000"))
    );
    assert_eq!(front["recipient"], json!(format!("0x{}", "7e".repeat(20))));
    assert_eq!(body["queue"][1]["owed"], json!("250000000000"));
    assert_eq!(body["paid"][0]["owed"], json!("0"));
    assert_eq!(body["paid_total"], json!("21000000000"));
}
