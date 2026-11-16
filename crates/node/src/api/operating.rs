//! `GET /v1/status`: the server's operating state, signed like every other answer; the signed
//! stream carries the same body as `status` frames (`stream/status.rs`).

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;

use crate::api::{ApiError, ApiResult, ApiState};
use crate::status::StatusView;

impl ApiState {
    /// The operating state now.
    /// Error cases:
    /// - the sequencer has stopped
    pub(super) async fn operating_state(&self) -> Result<StatusView, ApiError> {
        let (status, time) = (self.status.clone(), self.signing.time_state());
        self.read(move |engine| status.view(engine, time)).await
    }
}

impl ApiState {
    /// `operating_state`, read at most once every `max_age` however many callers ask: the read in
    /// flight is waited for, and the view it made is served until it is `max_age` old.
    /// Error cases:
    /// - the sequencer has stopped
    pub(super) async fn shared_operating_state(&self, max_age: Duration) -> Result<StatusView, ApiError> {
        let mut shared = self.status.shared.lock().await;
        if let Some((read_at, view)) = shared.as_ref()
            && read_at.elapsed() < max_age
        {
            return Ok(view.clone());
        }
        let view = self.operating_state().await?;
        *shared = Some((Instant::now(), view.clone()));
        Ok(view)
    }
}

/// Whether the server takes orders and, if it does not, why; each price feed's venue and symbols with
/// their state; trusted time; and the open markets with no price.
/// Error cases:
/// - the sequencer has stopped
pub(super) async fn status(State(s): State<ApiState>) -> ApiResult<StatusView> {
    Ok(Json(s.operating_state().await?))
}
