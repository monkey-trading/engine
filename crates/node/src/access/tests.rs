use super::*;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use engine_core::access::{Jurisdiction, SanctionsStatus};
use engine_core::eip712::Signature;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::subscriber::set_default;
use tracing_subscriber::Registry;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

use crate::test_support::Written;
use crate::test_tracing::keep_callsites_interested;

/// A personal value the source must never see the node log.
const MARKER: &str = "client-203.0.113.9-personal-value";

#[derive(Clone, Copy)]
enum Behavior {
    Valid,
    Invalid,
    Mismatch,
    Oversized,
    Chunked,
    Unavailable,
    Never,
    Redirect,
    TypedValue,
}

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Value>>>,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn source_requests_carry_the_actual_peer_and_responses_keep_the_sources_observation_time() {
    let server = server(Behavior::Valid).await;
    let client = client(&server.url, 4096);
    let request = query();
    let AccessEvidence::Screened(screening) = client.screen(&request).await else { panic!("valid source response") };
    assert_eq!(screening.observed_at, 1234, "retrieval does not refresh old source evidence");
    assert_eq!(screening.jurisdiction, Jurisdiction::Unknown);
    assert_eq!(screening.request_hash, request.request_hash);
    let requests = server.requests.lock().unwrap();
    assert_eq!(
        requests.as_slice(),
        [json!({"request_hash":request.request_hash,"account":request.account,
        "payer":null,"peer":"203.0.113.9"})]
    );
}

#[tokio::test]
async fn malformed_mismatched_oversized_chunked_failed_and_timed_out_sources_fail_closed() {
    for (behavior, expected) in [
        (Behavior::Invalid, SourceFailure::InvalidResponse),
        (Behavior::Mismatch, SourceFailure::InvalidResponse),
        (Behavior::Oversized, SourceFailure::InvalidResponse),
        (Behavior::Chunked, SourceFailure::InvalidResponse),
        (Behavior::Unavailable, SourceFailure::Unavailable),
        (Behavior::Never, SourceFailure::Unavailable),
        (Behavior::Redirect, SourceFailure::Unavailable),
    ] {
        let server = server(behavior).await;
        let client = client(&format!("{}?credential=private-fixture-value", server.url), 2048);
        let result = client.screen(&query()).await;
        assert_eq!(result, AccessEvidence::Failed { reason: expected });
        assert!(!serde_json::to_string(&result).unwrap().contains("private-fixture-value"));
        assert_eq!(
            server.requests.lock().unwrap().len(),
            1,
            "redirects and retries do not issue extra source requests"
        );
    }
}

#[tokio::test]
async fn absent_sources_refuse_without_network_and_api_capacity_does_not_consume_custody_capacity() {
    let request = query();
    assert_eq!(screen(None, &request).await, AccessEvidence::Failed { reason: SourceFailure::NotConfigured });
    let server = server(Behavior::Valid).await;
    let client = client(&server.url, 4096);
    let _all_api_slots = client.api_slots.acquire_many(2).await.unwrap();
    assert!(client.api_slots.try_acquire().is_err());
    assert!(
        matches!(client.screen(&request).await, AccessEvidence::Screened(_)),
        "custody retains an independent path"
    );
}

#[tokio::test]
async fn decode_failures_log_the_error_position_but_no_response_values() {
    keep_callsites_interested();
    let server = server(Behavior::TypedValue).await;
    let client = client(&server.url, 2048);
    let written = Written::default();
    let _logs = set_default(Registry::default().with(fmt::layer().with_writer(written.clone()).with_ansi(false)));
    let result = client.screen(&query()).await;
    assert_eq!(result, AccessEvidence::Failed { reason: SourceFailure::InvalidResponse });
    let text = written.text();
    assert!(!text.contains(MARKER), "the decode error quoted a response value:\n{text}");
    assert!(text.contains("access source response is not a screening"), "{text}");
    assert!(text.contains("category=Data") && text.contains("line=1") && text.contains("column="), "{text}");
}

fn query() -> ScreeningRequest {
    ScreeningRequest {
        request_hash: B256::repeat_byte(1),
        account: Some(AccountId([2; 20])),
        payer: None,
        peer: Some("203.0.113.9".parse().unwrap()),
    }
}

fn client(endpoint: &str, limit: usize) -> AccessClient {
    let config: AccessSection = toml::from_str(&format!(
        "source='0x{}'\nendpoint='{endpoint}'\ntimeout='100ms'\nmax_age='1s'\nmax_response_bytes={limit}\nmax_in_flight=2",
        "11".repeat(20),
    )).unwrap();
    AccessClient::new(&config).unwrap()
}

async fn server(behavior: Behavior) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let state = (behavior, requests.clone());
    let app = Router::new().route("/", post(answer)).with_state(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server { url, requests, task }
}

async fn answer(
    State((behavior, requests)): State<(Behavior, Arc<Mutex<Vec<Value>>>)>,
    Json(request): Json<Value>,
) -> Response {
    requests.lock().unwrap().push(request.clone());
    match behavior {
        Behavior::Invalid => "invalid json with credential=private-fixture-value".into_response(),
        Behavior::Oversized => "x".repeat(4096).into_response(),
        Behavior::Chunked => {
            let chunks = futures_util::stream::iter([Ok::<_, std::io::Error>(vec![b'x'; 1024]), Ok(vec![b'y'; 2048])]);
            Body::from_stream(chunks).into_response()
        }
        Behavior::Unavailable => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        // A typed field holding a personal value: the whole serde error would quote it.
        Behavior::TypedValue => json!({"observed_at": MARKER}).to_string().into_response(),
        Behavior::Never => std::future::pending().await,
        Behavior::Redirect => (StatusCode::TEMPORARY_REDIRECT, [("location", "/redirect-target")]).into_response(),
        Behavior::Valid | Behavior::Mismatch => {
            let screening = Screening {
                source: AccountId([0x11; 20]),
                request_hash: if matches!(behavior, Behavior::Mismatch) {
                    B256::ZERO
                } else {
                    serde_json::from_value(request["request_hash"].clone()).unwrap()
                },
                observed_at: 1234,
                account: serde_json::from_value(request["account"].clone()).unwrap(),
                payer: None,
                jurisdiction: Jurisdiction::Unknown,
                account_status: SanctionsStatus::Clear,
                payer_status: SanctionsStatus::Unknown,
                signature: Signature::ZERO,
            };
            Json(screening).into_response()
        }
    }
}
