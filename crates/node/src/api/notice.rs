//! `GET /v1/notice`: the operator's status notice, signed like every other answer; the signed stream
//! carries the same body as `notice` frames (`stream/notice.rs`).

use axum::Json;
use axum::extract::State;
use engine_core::notice::Notice;
use serde::Serialize;

use crate::api::{ApiResult, ApiState};

/// What `GET /v1/notice` returns, and a `notice` frame carries.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub(super) struct NoticeView {
    /// The notice in force and the sequenced time it was set, or `null` when there is none.
    pub(super) notice: Option<Notice>,
}

impl ApiState {
    /// The notice in force, as the stats worker's engine copy has it, which applies only durable
    /// batches.
    /// Error cases:
    /// - the sequencer has stopped
    pub(super) async fn current_notice(&self) -> Result<NoticeView, crate::api::ApiError> {
        self.read(|engine| NoticeView { notice: engine.notice().cloned() }).await
    }
}

/// The operator's status notice, or none.
/// Error cases:
/// - the sequencer has stopped
pub(super) async fn notice(State(s): State<ApiState>) -> ApiResult<NoticeView> {
    Ok(Json(s.current_notice().await?))
}
