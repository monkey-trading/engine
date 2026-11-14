//! The scenario node's HTTP helpers: the admin route, GETs and POSTs through its API's router.

use std::net::SocketAddr;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request as HttpRequest, StatusCode};
use axum::{Extension, Router};
use engine_core::command::AdminCommand;
use k256::ecdsa::SigningKey;
use serde_json::Value;
use tower::ServiceExt;

use super::Node;
use crate::api::router;

impl Node {
    /// Posts `command` signed by `key` with its next nonce through the admin route, returning the
    /// status and the last event.
    pub(in crate::custody::tests::election_scenario) async fn admin(
        &self,
        key: &SigningKey,
        command: AdminCommand,
    ) -> (StatusCode, Value) {
        let address = engine_core::eip712::address(key.verifying_key());
        let nonce = self.query(move |engine| engine.next_request_nonce(address)).await;
        let expires_at = self.state.clock.now_ms() + 60_000;
        let request = crate::admin::sign(key, &self.state.operator, command, nonce, expires_at);
        let body = serde_json::to_string(&request).unwrap();
        let request = HttpRequest::post("/v1/admin").header("content-type", "application/json").body(Body::from(body));
        let (status, events) = self.send(request.unwrap()).await;
        (status, events.as_array().and_then(|events| events.last()).cloned().unwrap_or(events))
    }

    /// The status and JSON body of a GET of `path`.
    pub(in crate::custody::tests::election_scenario) async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(HttpRequest::get(path).body(Body::empty()).unwrap()).await
    }

    /// The status, headers and body bytes of a GET of `path` under `challenge`, as a client saves them.
    pub(in crate::custody::tests::election_scenario) async fn get_signed(
        &self,
        path: &str,
        challenge: &str,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let request =
            HttpRequest::get(path).header(engine_wire::CHALLENGE_HEADER, challenge).body(Body::empty()).unwrap();
        let app: Router =
            router(self.state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
        let response = app.oneshot(request).await.unwrap();
        let (status, headers) = (response.status(), response.headers().clone());
        (status, headers, to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec())
    }

    /// The status and JSON body of a POST of `body` to `path`.
    pub(in crate::custody::tests::election_scenario) async fn post(
        &self,
        path: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let request =
            HttpRequest::post(path).header("content-type", "application/json").body(Body::from(body.to_string()));
        self.send(request.unwrap()).await
    }

    async fn send(&self, request: HttpRequest<Body>) -> (StatusCode, Value) {
        let app: Router =
            router(self.state.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
}
