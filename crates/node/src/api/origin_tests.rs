//! Browser origins: CORS headers for the listed ones, refused stream upgrades for the rest.

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE, AsHeaderName, ORIGIN, VARY,
};
use axum::http::{Method, Request as HttpRequest, Response, StatusCode};
use governor::Quota;
use governor::clock::DefaultClock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tower::ServiceExt;

use super::tests::{api_state, serve_state};
use super::*;
use crate::config::{ApiSection, RateLimit};
use crate::test_support::dev_config;

const LISTED: &str = "http://localhost:3000";
const UNLISTED: &str = "http://evil.test";

/// State whose `[api]` section lists no origin, which the dev config otherwise does.
fn unlisted_state() -> ApiState {
    let mut api = dev_config().api;
    api.allowed_origins.clear();
    let (mut state, _rx) = api_state(true);
    state.limits = Arc::new(ApiLimits::new(&api));
    state
}

/// The router over state that lists `LISTED`, with `tighten` applied to the `[api]` section first.
fn app(tighten: impl FnOnce(&mut ApiSection)) -> Router {
    let mut api = dev_config().api;
    api.allowed_origins = vec![LISTED.to_owned()];
    tighten(&mut api);
    let (mut state, _rx) = api_state(true);
    // Buckets that refill hourly, so only the burst counts however long the requests take.
    let hourly = |rate: RateLimit| Quota::with_period(Duration::from_secs(3600)).unwrap().allow_burst(rate.burst);
    state.limits = Arc::new(ApiLimits::with_quota(&api, hourly, DefaultClock::default()));
    router(state).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))))
}

async fn send(app: &Router, method: Method, path: &str, origin: Option<&str>, body: &str) -> Response<Body> {
    let mut req = HttpRequest::builder().method(method).uri(path).header("content-type", "application/json");
    if let Some(origin) = origin {
        req = req.header(ORIGIN, origin);
    }
    app.clone().oneshot(req.body(Body::from(body.to_owned())).unwrap()).await.unwrap()
}

fn header(response: &Response<Body>, name: impl AsHeaderName) -> Option<&str> {
    response.headers().get(name).map(|v| v.to_str().unwrap())
}

#[tokio::test]
async fn a_read_from_a_listed_origin_carries_that_origin_and_vary() {
    let app = app(|_| {});
    let response = send(&app, Method::GET, "/v1/health", Some(LISTED), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(header(&response, ACCESS_CONTROL_ALLOW_ORIGIN), Some(LISTED));
    assert_eq!(header(&response, ACCESS_CONTROL_EXPOSE_HEADERS), Some("Retry-After"));
    assert_eq!(header(&response, VARY), Some("Origin"));
    assert!(response.headers().get("access-control-allow-credentials").is_none());
}

#[tokio::test]
async fn a_request_from_an_unlisted_or_no_origin_gets_no_cors_headers() {
    let app = app(|_| {});
    for origin in [Some(UNLISTED), Some("http://localhost:3000/"), Some("null"), Some("HTTP://LOCALHOST:3000"), None] {
        let response = send(&app, Method::GET, "/v1/health", origin, "").await;
        assert_eq!(response.status(), StatusCode::OK, "{origin:?}");
        assert!(response.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none(), "{origin:?}");
        assert!(response.headers().get(ACCESS_CONTROL_EXPOSE_HEADERS).is_none(), "{origin:?}");
    }
}

#[tokio::test]
async fn a_post_from_a_listed_origin_carries_its_origin_on_the_refusal() {
    let app = app(|_| {});
    let listed = send(&app, Method::POST, "/v1/orders", Some(LISTED), "{}").await;
    assert!(listed.status().is_client_error(), "{}", listed.status());
    assert_eq!(header(&listed, ACCESS_CONTROL_ALLOW_ORIGIN), Some(LISTED));
    let unlisted = send(&app, Method::POST, "/v1/orders", Some(UNLISTED), "{}").await;
    assert!(unlisted.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
}

#[tokio::test]
async fn a_preflight_from_a_listed_origin_answers_204_and_others_get_no_grant() {
    let app = app(|_| {});
    for path in ["/v1/orders", "/v1/markets", "/v1/accounts/0x00/nonce", "/v1/stream"] {
        let response = send(&app, Method::OPTIONS, path, Some(LISTED), "").await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{path}");
        assert_eq!(header(&response, ACCESS_CONTROL_ALLOW_ORIGIN), Some(LISTED));
        assert_eq!(header(&response, ACCESS_CONTROL_ALLOW_METHODS), Some("GET, POST, OPTIONS"));
        assert_eq!(header(&response, ACCESS_CONTROL_ALLOW_HEADERS), Some("Content-Type, X-Engine-Challenge"));
        assert_eq!(header(&response, ACCESS_CONTROL_MAX_AGE), Some("600"));
        assert_eq!(header(&response, VARY), Some("Origin"));
    }
    for origin in [Some(UNLISTED), None] {
        let response = send(&app, Method::OPTIONS, "/v1/orders", origin, "").await;
        assert_ne!(response.status(), StatusCode::NO_CONTENT, "{origin:?}");
        assert!(response.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none(), "{origin:?}");
        assert!(response.headers().get(ACCESS_CONTROL_ALLOW_METHODS).is_none(), "{origin:?}");
    }
}

#[tokio::test]
async fn the_admin_route_never_answers_a_browser_origin() {
    let app = app(|_| {});
    let preflight = send(&app, Method::OPTIONS, "/v1/admin", Some(LISTED), "").await;
    assert_ne!(preflight.status(), StatusCode::NO_CONTENT);
    let post = send(&app, Method::POST, "/v1/admin", Some(LISTED), "{}").await;
    for response in [preflight, post] {
        assert!(response.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
        assert!(response.headers().get(ACCESS_CONTROL_ALLOW_METHODS).is_none());
        assert!(response.headers().get(VARY).is_none());
    }
}

#[tokio::test]
async fn a_rate_limited_or_oversized_refusal_from_a_listed_origin_carries_its_origin() {
    let one = NonZeroU32::new(1).unwrap();
    let app = app(|api| {
        api.per_ip = RateLimit { per_second: one, burst: one };
        api.max_body_bytes = NonZeroUsize::new(8).unwrap();
    });
    let big = "x".repeat(64);
    let too_large = send(&app, Method::POST, "/v1/orders", Some(LISTED), &big).await;
    assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(header(&too_large, ACCESS_CONTROL_ALLOW_ORIGIN), Some(LISTED));
    // The burst of one is spent, so the next request is limited, and from an unlisted origin carries nothing.
    let limited = send(&app, Method::GET, "/v1/health", Some(LISTED), "").await;
    assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&limited, ACCESS_CONTROL_ALLOW_ORIGIN), Some(LISTED));
    assert!(limited.headers().get("retry-after").is_some());
    for origin in [Some(UNLISTED), None] {
        let response = send(&app, Method::GET, "/v1/health", origin, "").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{origin:?}");
        assert!(response.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none(), "{origin:?}");
    }
}

#[tokio::test]
async fn with_no_origin_listed_nothing_changes() {
    let app = router(unlisted_state()).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
    let response = send(&app, Method::GET, "/v1/health", Some(UNLISTED), "").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    assert!(response.headers().get(VARY).is_none());
    let preflight = send(&app, Method::OPTIONS, "/v1/orders", Some(UNLISTED), "").await;
    assert_ne!(preflight.status(), StatusCode::NO_CONTENT);
}

/// The status of a stream upgrade request sent with `origin`, if any.
async fn stream_upgrade_status(addr: SocketAddr, origin: Option<&str>) -> u16 {
    let origin = origin.map_or_else(String::new, |o| format!("Origin: {o}\r\n"));
    let request = format!(
        "GET /v1/stream HTTP/1.1\r\nHost: {addr}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n{origin}\r\n"
    );
    let mut conn = TcpStream::connect(addr).await.unwrap();
    conn.write_all(request.as_bytes()).await.unwrap();
    let mut head = [0u8; 12];
    timeout(Duration::from_secs(10), conn.read_exact(&mut head)).await.expect("a response in time").unwrap();
    String::from_utf8_lossy(&head[9..12]).parse().unwrap()
}

#[tokio::test]
async fn the_stream_upgrade_is_refused_for_an_unlisted_origin_only() {
    let mut api = dev_config().api;
    api.allowed_origins = vec![LISTED.to_owned()];
    let (mut state, _rx) = api_state(true);
    state.limits = Arc::new(ApiLimits::new(&api));
    let addr = serve_state(state).await;
    assert_eq!(stream_upgrade_status(addr, Some(UNLISTED)).await, 403);
    assert_eq!(stream_upgrade_status(addr, Some("null")).await, 403);
    assert_eq!(stream_upgrade_status(addr, Some(LISTED)).await, 101);
    assert_eq!(stream_upgrade_status(addr, None).await, 101);
}

#[tokio::test]
async fn with_no_origin_listed_the_stream_refuses_any_origin_but_takes_none() {
    let addr = serve_state(unlisted_state()).await;
    assert_eq!(stream_upgrade_status(addr, Some(UNLISTED)).await, 403);
    assert_eq!(stream_upgrade_status(addr, Some(LISTED)).await, 403);
    assert_eq!(stream_upgrade_status(addr, Some("null")).await, 403);
    assert_eq!(stream_upgrade_status(addr, None).await, 101);
}

#[tokio::test]
async fn with_no_origin_listed_an_origin_on_another_route_changes_nothing() {
    let app = router(unlisted_state()).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
    let refused = send(&app, Method::GET, "/v1/stream", Some(UNLISTED), "").await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert!(refused.headers().get(VARY).is_none());
    assert!(refused.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
}

#[tokio::test]
async fn preflights_count_against_the_per_ip_rate() {
    let one = NonZeroU32::new(1).unwrap();
    let app = app(|api| api.per_ip = RateLimit { per_second: one, burst: NonZeroU32::new(3).unwrap() });
    // A flood of preflights from a listed origin runs out the burst, then is refused with 429 and
    // the origin's headers, whichever route it names.
    let mut statuses = Vec::new();
    for path in ["/v1/orders", "/v1/markets", "/v1/stream", "/v1/orders", "/v1/stream"] {
        statuses.push(send(&app, Method::OPTIONS, path, Some(LISTED), "").await.status());
    }
    let no_content = StatusCode::NO_CONTENT;
    let limited = StatusCode::TOO_MANY_REQUESTS;
    assert_eq!(statuses, [no_content, no_content, no_content, limited, limited]);
    let response = send(&app, Method::OPTIONS, "/v1/orders", Some(LISTED), "").await;
    assert_eq!(response.status(), limited);
    assert_eq!(header(&response, ACCESS_CONTROL_ALLOW_ORIGIN), Some(LISTED));
    assert!(response.headers().get("retry-after").is_some());
    assert!(response.headers().get(ACCESS_CONTROL_ALLOW_METHODS).is_none());
}

#[tokio::test]
async fn preflights_and_requests_share_one_per_ip_bucket() {
    let one = NonZeroU32::new(1).unwrap();
    let app = app(|api| api.per_ip = RateLimit { per_second: one, burst: NonZeroU32::new(2).unwrap() });
    assert_eq!(send(&app, Method::OPTIONS, "/v1/orders", Some(LISTED), "").await.status(), StatusCode::NO_CONTENT);
    assert_eq!(send(&app, Method::GET, "/v1/health", Some(LISTED), "").await.status(), StatusCode::OK);
    let preflight = send(&app, Method::OPTIONS, "/v1/orders", Some(LISTED), "").await;
    assert_eq!(preflight.status(), StatusCode::TOO_MANY_REQUESTS);
}
