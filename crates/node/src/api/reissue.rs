//! A holder's request for a fresh authorization of an approval whose last one expired unclaimed.

use axum::Json;
use axum::extract::{Path, State};
use engine_core::types::AccountId;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::api::{ApiError, ApiResult, ApiState};
use crate::sequencer::{Reissue, Request};

/// Queues the approval for signing again; the authorization arrives on the stream. The node never
/// reissues on a schedule, so this is the only way an expired authorization is replaced short of
/// a signer rotation.
/// Error cases:
/// - the address is invalid, no unpaid approval is loaded under the nonce, or its latest
///   authorization is still usable
/// - payouts are not enabled, or the sequencer has stopped
pub(super) async fn reissue(
    State(s): State<ApiState>,
    Path((address, nonce)): Path<(String, u64)>,
) -> ApiResult<Value> {
    let account: AccountId = address.parse().map_err(|error| ApiError::BadRequest(format!("{error}")))?;
    let (reply, rx) = oneshot::channel();
    s.tx.send(Request::ReissueWithdrawal { account, nonce, reply }).await.map_err(|_| ApiError::Unavailable)?;
    match rx.await.map_err(|_| ApiError::Unavailable)? {
        Reissue::Queued => Ok(Json(json!({ "status": "queued" }))),
        Reissue::Unknown => Err(ApiError::NotFound),
        Reissue::StillValid { expires_at } => {
            Err(ApiError::BadRequest(format!("the latest authorization is usable until {expires_at}")))
        }
        Reissue::Unavailable => Err(ApiError::Unavailable),
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    use super::*;
    use crate::api::tests::api_state;

    const ADDRESS: &str = "0x0101010101010101010101010101010101010101";

    /// The route's status for a sequencer that answers `outcome`, and the request it relayed.
    async fn answer(outcome: Reissue) -> (StatusCode, AccountId, u64) {
        let (state, mut requests) = api_state(true);
        let handler = tokio::spawn(reissue(State(state), Path((ADDRESS.into(), 7))));
        let Some(Request::ReissueWithdrawal { account, nonce, reply }) = requests.recv().await else {
            panic!("the route asks the sequencer");
        };
        reply.send(outcome).unwrap();
        let status = match handler.await.unwrap() {
            Ok(_) => StatusCode::OK,
            Err(error) => error.into_response().status(),
        };
        (status, account, nonce)
    }

    #[tokio::test]
    async fn each_sequencer_answer_maps_to_its_status() {
        for (outcome, status) in [
            (Reissue::Queued, StatusCode::OK),
            (Reissue::Unknown, StatusCode::NOT_FOUND),
            (Reissue::StillValid { expires_at: 9 }, StatusCode::BAD_REQUEST),
            (Reissue::Unavailable, StatusCode::SERVICE_UNAVAILABLE),
        ] {
            assert_eq!(answer(outcome).await, (status, AccountId([1; 20]), 7), "{outcome:?}");
        }
    }

    #[tokio::test]
    async fn an_invalid_address_never_reaches_the_sequencer() {
        let (state, mut requests) = api_state(true);
        let result = reissue(State(state), Path(("nonsense".into(), 1))).await;
        assert!(matches!(result, Err(ApiError::BadRequest(_))));
        assert!(requests.try_recv().is_err());
    }
}
