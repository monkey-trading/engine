//! `GET /v1/house/state`: the protocol parameters and the latest checkpoint, from memory.

use axum::Json;
use axum::extract::State;
use tokio::sync::oneshot;

use crate::api::{ApiError, ApiResult, ApiState};
use crate::sequencer::Request;
use crate::sequencer::house::HouseState;

/// The seq the read is at, the parameters in force with the seq that set them, and the latest
/// checkpoint in the stream's form; the sequencer answers once the batch before it is flushed.
/// Error cases:
/// - the sequencer has stopped
pub(super) async fn house_state(State(s): State<ApiState>) -> ApiResult<HouseState> {
    let (reply, answer) = oneshot::channel();
    s.tx.send(Request::HouseState(reply)).await.map_err(|_| ApiError::Unavailable)?;
    Ok(Json(answer.await.map_err(|_| ApiError::Unavailable)?))
}
