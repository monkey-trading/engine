//! Signed answers: what the response signature covers, which responses carry one, and that the
//! node's signatures are the published vectors' byte for byte.

use super::*;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::Path;

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::connect_info::MockConnectInfo;
use axum::http::header::{ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, ORIGIN};
use axum::http::{HeaderMap, Request as HttpRequest};
use engine_wire::{
    CHALLENGE_HEADER, Connection, Deployment, Error, SEQ_HEADER, SIGNATURE_HEADER, SignedFrame, TIME_HEADER,
    verify_response,
};
use governor::Quota;
use governor::clock::DefaultClock;
use tower::ServiceExt;

use crate::config::RateLimit;
use crate::history::INDEX_FILE;
use crate::tee::{AttestationBundle, EnclaveKey, MockAttestor, attest_bundle};
use crate::test_support::{TestSequencer, dev_config, scratch_dir, signing};

const CHALLENGE: &str = "8f0e5c2a9b7d41e6a3c0f19d2e84b7c65a1d0e3f9b2c7a48e6d1f05b3a9c2e71";
/// The time the API reads in these tests: Wed 28 Oct 2026 00:00 UTC.
const NOW: Millis = 1_793_145_600_000;
const VECTORS: &str = include_str!("../../../wire/tests/vectors.json");

#[tokio::test]
async fn a_read_is_signed_over_its_request_challenge_time_and_log_head() {
    let (node, app) = node("read", &dev_config().api);
    let target = "/v1/markets?view=all";
    let (status, headers, body) = call(&app, "GET", target, Some(CHALLENGE), Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[CHALLENGE_HEADER], CHALLENGE);
    assert_eq!(headers[TIME_HEADER], NOW.to_string());
    let head = node.manifest.borrow().flushed.expect("genesis is in the log").seq;
    assert_eq!(headers[SEQ_HEADER], head.to_string());
    let challenge = parse(CHALLENGE);
    assert_eq!(verify(("GET", target, b""), challenge, status, &headers, &body), Ok(()));

    let mut altered = body.to_vec();
    altered[1] ^= 1;
    for (what, sent, challenge, body) in [
        ("body", ("GET", target, &b""[..]), challenge, &altered[..]),
        ("request body", ("GET", target, &b"{}"[..]), challenge, &body[..]),
        ("challenge", ("GET", target, &b""[..]), [0xc5; 32], &body[..]),
        ("query", ("GET", "/v1/markets", &b""[..]), challenge, &body[..]),
        ("method", ("POST", target, &b""[..]), challenge, &body[..]),
    ] {
        assert_eq!(
            verify(sent, challenge, status, &headers, body),
            Err(Error::WrongSigner),
            "the response verified under another {what}"
        );
    }
    assert_eq!(
        verify(("GET", target, b""), challenge, StatusCode::NOT_FOUND, &headers, &body),
        Err(Error::WrongSigner),
        "the response verified under another status"
    );
}

#[tokio::test]
async fn a_response_is_signed_over_the_request_body_it_answers() {
    let (_node, app) = node("request-body", &dev_config().api);
    let sent = br#"{"account":"0x0707070707070707070707070707070707070707","amount":"oops"}"#;
    let (status, headers, body) = call(&app, "POST", "/v1/withdrawals", Some(CHALLENGE), Body::from(&sent[..])).await;
    assert!(status.is_client_error(), "{status}");
    assert_eq!(verify(("POST", "/v1/withdrawals", sent), parse(CHALLENGE), status, &headers, &body), Ok(()));
    // The same answer for a substituted body, under the same request line and challenge.
    let other = br#"{"account":"0x0808080808080808080808080808080808080808","amount":"oops"}"#;
    assert_eq!(
        verify(("POST", "/v1/withdrawals", other), parse(CHALLENGE), status, &headers, &body),
        Err(Error::WrongSigner)
    );
}

#[tokio::test]
async fn a_request_without_a_challenge_is_answered_under_zero_bytes_and_an_uppercase_one_is_echoed_in_lowercase() {
    let (_node, app) = node("no-challenge", &dev_config().api);
    let (status, headers, body) = call(&app, "GET", "/v1/house", None, Body::empty()).await;
    assert_eq!(headers[CHALLENGE_HEADER], "0".repeat(64));
    assert_eq!(verify(("GET", "/v1/house", b""), [0; 32], status, &headers, &body), Ok(()));

    let upper = CHALLENGE.to_uppercase();
    let (status, headers, body) = call(&app, "GET", "/v1/house", Some(&upper), Body::empty()).await;
    assert_eq!(headers[CHALLENGE_HEADER], CHALLENGE);
    assert_eq!(verify(("GET", "/v1/house", b""), parse(CHALLENGE), status, &headers, &body), Ok(()));
}

#[tokio::test]
async fn a_malformed_challenge_gets_an_unsigned_400() {
    let (_node, app) = node("bad-challenge", &dev_config().api);
    for bad in [&CHALLENGE[1..], &format!("{CHALLENGE}0"), &format!("0x{}", &CHALLENGE[2..]), &"zz".repeat(32), ""] {
        let (status, headers, _) = call(&app, "GET", "/v1/markets", Some(bad), Body::empty()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}");
        assert!(!headers.contains_key(SIGNATURE_HEADER), "{bad:?}");
    }
}

#[tokio::test]
async fn error_responses_from_handlers_and_the_router_are_signed() {
    let (_node, app) = node("errors", &dev_config().api);
    for (method, target, expected) in [
        ("GET", "/v1/accounts/not-an-address", StatusCode::BAD_REQUEST),
        ("GET", "/v1/accounts/0x0707070707070707070707070707070707070707", StatusCode::NOT_FOUND),
        ("GET", "/v1/no-such-route", StatusCode::NOT_FOUND),
        ("POST", "/v1/markets", StatusCode::METHOD_NOT_ALLOWED),
    ] {
        let (status, headers, sent) = call(&app, method, target, Some(CHALLENGE), Body::empty()).await;
        assert_eq!(status, expected, "{method} {target}");
        let verified = verify((method, target, b""), parse(CHALLENGE), status, &headers, &sent);
        assert_eq!(verified, Ok(()), "{method} {target}");
    }
}

#[tokio::test]
async fn self_authenticating_routes_and_refusals_outside_the_signer_carry_no_signature() {
    let mut api = dev_config().api;
    let six = NonZeroU32::new(6).unwrap();
    api.per_ip = RateLimit { per_second: six, burst: six };
    let (_node, app) = node("unsigned", &api);
    for target in ["/v1/health", "/v1/attestation", "/v1/attestation/collateral", "/v1/archive/manifest.json"] {
        let (_, headers, _) = call(&app, "GET", target, Some(CHALLENGE), Body::empty()).await;
        assert!(!headers.contains_key(SIGNATURE_HEADER), "{target}");
    }
    // The body read refuses an oversized body before the signer sees the request.
    let too_large = Body::from(vec![b' '; api.max_body_bytes.get() + 1]);
    let (status, headers, _) = call(&app, "POST", "/v1/orders", Some(CHALLENGE), too_large).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!headers.contains_key(SIGNATURE_HEADER));
    let (status, headers, _) = call(&app, "GET", "/v1/markets", Some(CHALLENGE), Body::empty()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.contains_key(SIGNATURE_HEADER));
    // The six requests above spent the burst; the limiter refuses the seventh before it is signed.
    let (status, headers, _) = call(&app, "GET", "/v1/markets", Some(CHALLENGE), Body::empty()).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(!headers.contains_key(SIGNATURE_HEADER));
}

#[tokio::test]
async fn a_head_request_is_signed_over_the_empty_body_it_sends() {
    let (_node, app) = node("head", &dev_config().api);
    let (status, headers, body) = call(&app, "HEAD", "/v1/markets", Some(CHALLENGE), Body::empty()).await;
    assert_eq!((status, body.len()), (StatusCode::OK, 0));
    assert_eq!(verify(("HEAD", "/v1/markets", b""), parse(CHALLENGE), status, &headers, &body), Ok(()));
    let (_, _, get) = call(&app, "GET", "/v1/markets", Some(CHALLENGE), Body::empty()).await;
    let verified = verify(("HEAD", "/v1/markets", b""), parse(CHALLENGE), status, &headers, &get);
    assert_eq!(verified, Err(Error::WrongSigner));
}

#[tokio::test]
async fn a_listed_origin_can_read_a_signed_answer_and_gets_its_preflight_unsigned_with_no_body() {
    let mut api = dev_config().api;
    let origin = "http://localhost:3000";
    api.allowed_origins = vec![origin.to_owned()];
    let (_node, app) = node("origins", &api);
    let send = |method: &str, extra: Option<(&'static str, &'static str)>| {
        let mut request = HttpRequest::builder().method(method).uri("/v1/markets").header(ORIGIN, origin);
        request = request.header(CHALLENGE_HEADER, CHALLENGE);
        if let Some((name, value)) = extra {
            request = request.header(name, value);
        }
        app.clone().oneshot(request.body(Body::empty()).unwrap())
    };
    let response = send("GET", None).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(headers[ACCESS_CONTROL_ALLOW_ORIGIN], origin);
    let exposed = headers[ACCESS_CONTROL_EXPOSE_HEADERS].to_str().unwrap().to_ascii_lowercase();
    for name in [SIGNATURE_HEADER, TIME_HEADER, SEQ_HEADER, CHALLENGE_HEADER] {
        assert!(exposed.split(", ").any(|exposed| exposed == name), "{name} is not exposed: {exposed}");
    }
    assert_eq!(verify(("GET", "/v1/markets", b""), parse(CHALLENGE), status, &headers, &body), Ok(()));

    let preflight = send("OPTIONS", Some(("access-control-request-method", "GET"))).await.unwrap();
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert!(!preflight.headers().contains_key(SIGNATURE_HEADER));
    assert!(to_bytes(preflight.into_body(), usize::MAX).await.unwrap().is_empty());
}

#[test]
fn the_node_signs_the_published_vectors_byte_for_byte() {
    let vectors: serde_json::Value = serde_json::from_str(VECTORS).unwrap();
    assert_eq!(vectors["signer"].as_str(), Some(EnclaveKey::test_key().address_hex().as_str()));
    let signing_for = |v: &serde_json::Value| {
        let vault: AccountId = v["vault"].as_str().unwrap().parse().unwrap();
        let chain_id = v["chain_id"].as_u64().unwrap();
        Signing::new(Arc::new(EnclaveKey::test_key()), chain_id, vault, Duration::from_secs(5))
    };
    let valid = |v: &&serde_json::Value| v["error"].is_null();
    let responses = vectors["responses"].as_array().unwrap();
    for v in responses.iter().filter(valid) {
        let response = engine_wire::Response {
            challenge: parse(v["challenge"].as_str().unwrap()),
            time_ms: v["time_ms"].as_u64().unwrap(),
            head_seq: v["head_seq"].as_u64().unwrap(),
            status: u16::try_from(v["status"].as_u64().unwrap()).unwrap(),
            method: v["method"].as_str().unwrap(),
            path_and_query: v["path_and_query"].as_str().unwrap(),
            request_body: v["request_body"].as_str().unwrap().as_bytes(),
            body: v["body"].as_str().unwrap().as_bytes(),
        };
        assert_eq!(signing_for(v).sign_response(&response), v["signature"].as_str().unwrap(), "{}", v["name"]);
    }
    let connections = vectors["connections"].as_array().unwrap();
    let mut frames = 0;
    for v in connections.iter().filter(valid) {
        let connection =
            Connection::new(parse(v["challenge"].as_str().unwrap()), v["path_and_query"].as_str().unwrap());
        let mut previous = [0; 32];
        for frame in v["frames"].as_array().unwrap() {
            let frame: SignedFrame = serde_json::from_value(frame.clone()).unwrap();
            let (digest, sig) = signing_for(v).sign_frame(&connection, &previous, frame.n, frame.time, &frame.msg);
            assert_eq!(sig, frame.sig, "{} frame {}", v["name"], frame.n);
            (previous, frames) = (digest, frames + 1);
        }
    }
    assert!(responses.iter().filter(valid).count() >= 3 && frames >= 3);
}

// ── Helpers ──

/// A sequencer over the dev genesis, and its API answering at `NOW` under `api`'s bursts, which
/// refill hourly, so only the burst counts however long the requests take.
fn node(name: &str, api: &crate::config::ApiSection) -> (TestSequencer, Router) {
    let dir = scratch_dir(&format!("signing-{name}"));
    let node = TestSequencer::start_legacy(&dir, 16);
    let app = router(state(&node, &dir, api)).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
    (node, app)
}

pub(super) fn state(node: &TestSequencer, dir: &Path, api: &crate::config::ApiSection) -> ApiState {
    let bundle = AttestationBundle {
        enclave_address: EnclaveKey::test_key().address(),
        config_sha256: [0; 32],
        tls_public_key_sha256: [0; 32],
    };
    ApiState {
        protocol: engine_core::protocol::ProtocolVersion::V23,
        access: None,
        tx: node.tx.clone(),
        stream: node.stream.clone(),
        attestation: Arc::new(attest_bundle(&MockAttestor, &bundle).unwrap()),
        auth: ClientAuth::Unsigned,
        operator: dev_config().operator().unwrap(),
        mock_custody: true,
        payouts_enabled: true,
        clock: || NOW,
        log_dir: dir.to_owned(),
        manifest: node.manifest.clone(),
        backfills: Arc::new(Semaphore::new(MAX_BACKFILLS)),
        limits: Arc::new(ApiLimits::with_quota(api, hourly, DefaultClock::default())),
        history: Reader::new(dir.join(INDEX_FILE), watch::channel(true).1),
        emission_budget: None,
        listing: Default::default(),
        signing: signing(),
    }
}

/// Serves one request, with `challenge` as its `X-Engine-Challenge` when given: the status, the
/// headers, and the body as sent.
async fn call(
    app: &Router,
    method: &str,
    target: &str,
    challenge: Option<&str>,
    body: Body,
) -> (StatusCode, HeaderMap, Bytes) {
    let mut request = HttpRequest::builder().method(method).uri(target).header("content-type", "application/json");
    if let Some(challenge) = challenge {
        request = request.header(CHALLENGE_HEADER, challenge);
    }
    let response = app.clone().oneshot(request.body(body).unwrap()).await.unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    (status, headers, to_bytes(response.into_body(), usize::MAX).await.unwrap())
}

/// Verifies a response's `X-Engine-*` headers as a client of the dev deployment that sent `sent`,
/// its method, path and query, and body, with `challenge` would, against the published mock
/// enclave key.
pub(super) fn verify(
    sent: (&str, &str, &[u8]),
    challenge: [u8; 32],
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), Error> {
    let (method, target, request_body) = sent;
    let header = |name: &str| headers.get(name).unwrap_or_else(|| panic!("no {name}")).to_str().unwrap().to_owned();
    let response = engine_wire::Response {
        challenge,
        time_ms: header(TIME_HEADER).parse().unwrap(),
        head_seq: header(SEQ_HEADER).parse().unwrap(),
        status: status.as_u16(),
        method,
        path_and_query: target,
        request_body,
        body,
    };
    let cfg = dev_config();
    let deployment =
        Deployment { chain_id: cfg.custody.chain_id, vault: cfg.custody.vault.parse::<AccountId>().unwrap().0 };
    verify_response(&EnclaveKey::test_key().address(), &deployment, &response, &header(SIGNATURE_HEADER))
}

fn hourly(rate: RateLimit) -> Quota {
    Quota::with_period(Duration::from_secs(3600)).unwrap().allow_burst(rate.burst)
}

pub(super) fn parse(challenge: &str) -> [u8; 32] {
    engine_wire::parse_challenge(challenge).unwrap()
}
