//! HTTP fixture calls and forwarding of actual checkpoint-worker startup verdicts.

use axum::body::{Body, to_bytes};
use axum::http::{Request as HttpRequest, StatusCode};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tower::ServiceExt;

use super::Scenario;
use crate::checkpoints::Health;
use crate::sequencer::Request;

/// Forwards every worker message and reports its first real collateral verdict after forwarding.
pub(super) fn status_channel(
    tx: mpsc::Sender<Request>,
) -> (mpsc::Sender<Request>, JoinHandle<()>, oneshot::Receiver<Health>) {
    let (status_tx, mut status_rx) = mpsc::channel(64);
    let (ready, observed) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut ready = Some(ready);
        while let Some(message) = status_rx.recv().await {
            let health = match &message {
                Request::CheckpointStatus { health, .. } => *health,
                _ => None,
            };
            if tx.send(message).await.is_err() {
                break;
            }
            if let Some(health) = health
                && let Some(ready) = ready.take()
            {
                let _ = ready.send(health);
            }
        }
    });
    (status_tx, task, observed)
}

impl Scenario {
    /// The status and JSON body of a POST of `body` to `path`.
    pub(super) async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let request =
            HttpRequest::post(path).header("content-type", "application/json").body(Body::from(body.to_string()));
        self.send(request.unwrap()).await
    }

    /// The status and JSON body of a GET of `path`.
    pub(super) async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send(HttpRequest::get(path).body(Body::empty()).unwrap()).await
    }

    /// Serves one request and reads its JSON response.
    async fn send(&self, request: HttpRequest<Body>) -> (StatusCode, Value) {
        let response = self.node().app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
}
