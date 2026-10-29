//! Signed answers: the enclave key signs every `/v1` response a client relies on, over the request
//! it answers with its body, the client's challenge, the node's time, the log head, and the status
//! and body as sent (`engine_wire::response_digest`). Health, the attestation and its collateral,
//! and the archive stay unsigned: the attestation and the archive's catalog authenticate
//! themselves, and health carries nothing to rely on. This layer sits inside the per-client rate
//! limit and the body read, so a refused request (429, 408, 413) costs no signature, and its
//! unsigned refusal is the client's to discard.
//!
//! The time is the host clock, which the sequencer checks its stamps against trusted time; the
//! API reads it unchecked, so a client takes freshness from its challenge, not from the time.
//! TODO: bound the signed time by the sequencer's latest trusted time sample.

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use engine_core::types::AccountId;
use engine_wire::{
    CHALLENGE_HEADER, Connection, Deployment, SEQ_HEADER, SIGNATURE_HEADER, TIME_HEADER, frame_digest, parse_challenge,
    response_digest,
};
use tracing::error;

use crate::api::{ApiError, ApiState};
use crate::tee::EnclaveKey;

/// Routes whose responses go unsigned; `/v1/archive/` and everything under it is unsigned too.
const UNSIGNED_ROUTES: [&str; 3] = ["/v1/health", "/v1/attestation", "/v1/attestation/collateral"];
const ARCHIVE: &str = "/v1/archive/";

/// What the API signs answers with: the enclave key, the deployment every signature names, and how
/// often a signed stream connection gets a heartbeat.
pub(crate) struct Signing {
    key: Arc<EnclaveKey>,
    deployment: Deployment,
    pub(crate) heartbeat_interval: Duration,
}

impl Signing {
    /// Signs with `key` for the vault `vault` on `chain_id`.
    pub(crate) fn new(key: Arc<EnclaveKey>, chain_id: u64, vault: AccountId, heartbeat_interval: Duration) -> Self {
        Self { key, deployment: Deployment { chain_id, vault: vault.0 }, heartbeat_interval }
    }

    /// The enclave key's signature over `response`.
    pub(crate) fn sign_response(&self, response: &engine_wire::Response<'_>) -> String {
        self.key.sign_digest(response_digest(&self.deployment, response))
    }

    /// The digest of frame `n` of `connection`, chained to `previous`, and the enclave key's
    /// signature over it.
    pub(crate) fn sign_frame(
        &self,
        connection: &Connection,
        previous: &[u8; 32],
        n: u64,
        time_ms: u64,
        msg: &str,
    ) -> ([u8; 32], String) {
        let digest = frame_digest(&self.deployment, connection, previous, n, time_ms, msg);
        (digest, self.key.sign_digest(digest))
    }
}

/// Signs the response to a `/v1` request outside `UNSIGNED_ROUTES` and the archive, and adds the
/// signature, the time, the log head's seq, and the challenge it covers as `X-Engine-*` headers. A
/// request without `X-Engine-Challenge` is answered under 32 zero bytes; one with a challenge that
/// is not 64 hex characters gets an unsigned 400, since there is no challenge to sign it under.
pub(super) async fn sign_responses(State(s): State<ApiState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if !path.starts_with("/v1/") || UNSIGNED_ROUTES.contains(&path) || path.starts_with(ARCHIVE) {
        return next.run(req).await;
    }
    let challenge = match req.headers().get(CHALLENGE_HEADER) {
        None => [0; 32],
        Some(value) => match value.to_str().ok().and_then(parse_challenge) {
            Some(challenge) => challenge,
            None => {
                let refusal = format!("{CHALLENGE_HEADER} must be 64 hex characters");
                return ApiError::BadRequest(refusal).into_response();
            }
        },
    };
    let method = req.method().clone();
    let target = req.uri().path_and_query().map_or(path, |target| target.as_str()).to_owned();
    // `read_body` has already read the request body, so this only takes it out of the request.
    let (parts, request_body) = req.into_parts();
    let request_body = match collect(request_body, "request").await {
        Ok(body) => body,
        Err(refusal) => return refusal,
    };
    let response = next.run(Request::from_parts(parts, Body::from(request_body.clone()))).await;
    let (mut parts, body) = response.into_parts();
    // Every signed route answers from memory; only the archive streams its body.
    let body = match collect(body, "response").await {
        Ok(body) => body,
        Err(refusal) => return refusal,
    };
    let time_ms = u64::try_from((s.clock)()).expect("the node's clock reads after the epoch");
    let head_seq = s.manifest.borrow().flushed.map_or(0, |head| head.seq);
    let response = engine_wire::Response {
        challenge,
        time_ms,
        head_seq,
        status: parts.status.as_u16(),
        method: method.as_str(),
        path_and_query: &target,
        request_body: &request_body,
        // The router drops a HEAD response's body after this layer, so none is sent.
        body: if method == Method::HEAD { &[] } else { &body },
    };
    let signature = s.signing.sign_response(&response);
    let headers = &mut parts.headers;
    headers.insert(SIGNATURE_HEADER, HeaderValue::from_str(&signature).expect("0x hex is a header value"));
    headers.insert(TIME_HEADER, HeaderValue::from(time_ms));
    headers.insert(SEQ_HEADER, HeaderValue::from(head_seq));
    headers.insert(CHALLENGE_HEADER, HeaderValue::from_str(&hex::encode(challenge)).expect("hex is a header value"));
    Response::from_parts(parts, Body::from(body))
}

/// The whole of `body`, a `what` body held in memory.
/// Error cases:
/// - the body fails while it is read: a Critical alert, and a 500 to send in its place
async fn collect(body: Body, what: &str) -> Result<Bytes, Response> {
    to_bytes(body, usize::MAX).await.map_err(|e| {
        error!(alert = "response_signing_failed", "reading a {what} body to sign: {e}");
        ApiError::Internal.into_response()
    })
}
