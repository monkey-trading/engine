//! An account's unpaid payouts and the latest signed authorization of each.

use axum::Json;
use axum::extract::{Path, Query, State};
use engine_core::types::AccountId;
use serde::Deserialize;
use tokio::sync::oneshot;

use crate::api::{ApiError, ApiResult, ApiState};
use crate::sequencer::{AccountPayouts, Request};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Params {
    after: Option<u64>,
    limit: Option<usize>,
}

/// Returns the account's unpaid approvals, oldest first, each with the latest authorization the
/// enclave signed (`null` until one is). The sequencer answers from the checkpoint gate's memory.
/// A nonce is absent only when `loaded` is true and no earlier page has a `next` cursor still open:
/// the approval is then paid.
/// Error cases:
/// - invalid address or pagination, or the sequencer has stopped
pub(super) async fn payouts(
    State(state): State<ApiState>,
    Path(address): Path<String>,
    Query(params): Query<Params>,
) -> ApiResult<AccountPayouts> {
    let account: AccountId = address.parse().map_err(|error| ApiError::BadRequest(format!("{error}")))?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest(format!("limit must be from 1 to {MAX_LIMIT}")));
    }
    let (reply, rx) = oneshot::channel();
    state
        .tx
        .send(Request::AccountPayouts { account, after: params.after, limit, reply })
        .await
        .map_err(|_| ApiError::Unavailable)?;
    Ok(Json(rx.await.map_err(|_| ApiError::Unavailable)?))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::body::{Body, to_bytes};
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::{Request as HttpRequest, StatusCode};
    use engine_core::Fixed;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;
    use crate::api::tests::api_state;
    use crate::sequencer::PayoutView;

    const ACCOUNT: AccountId = AccountId([1; 20]);

    /// Serves `uri` against a fake sequencer that answers `reply` and reports the request it saw.
    async fn get(uri: &str, reply: AccountPayouts) -> (StatusCode, Value, Option<(AccountId, Option<u64>, usize)>) {
        let (state, mut requests) = api_state(true);
        let app = crate::api::router(state).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        let sequencer = tokio::spawn(async move {
            let Some(Request::AccountPayouts { account, after, limit, reply: tx }) = requests.recv().await else {
                return None;
            };
            tx.send(reply).unwrap();
            Some((account, after, limit))
        });
        let response = app.oneshot(HttpRequest::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = response.status();
        let body =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap_or_default();
        sequencer.abort();
        (status, body, sequencer.await.ok().flatten())
    }

    fn page() -> AccountPayouts {
        AccountPayouts {
            seq: Some(7),
            loaded: true,
            payouts: vec![PayoutView { nonce: 3, amount: Fixed::from_int(40), withdrawal: None }],
            next: Some(3),
        }
    }

    #[tokio::test]
    async fn the_sequencers_page_is_served_in_the_documented_shape() {
        let (status, body, seen) = get(&format!("/v1/accounts/{ACCOUNT}/payouts?after=2&limit=5"), page()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({
                "seq": 7,
                "loaded": true,
                "payouts": [{ "nonce": 3, "amount": "40", "withdrawal": null }],
                "next": 3,
            })
        );
        assert_eq!(seen, Some((ACCOUNT, Some(2), 5)));
        let (_, _, seen) = get(&format!("/v1/accounts/{ACCOUNT}/payouts"), page()).await;
        assert_eq!(seen, Some((ACCOUNT, None, DEFAULT_LIMIT)));
    }

    #[tokio::test]
    async fn a_bad_address_or_page_never_reaches_the_sequencer() {
        for uri in [
            "/v1/accounts/nonsense/payouts".to_owned(),
            format!("/v1/accounts/{ACCOUNT}/payouts?limit=0"),
            format!("/v1/accounts/{ACCOUNT}/payouts?limit={}", MAX_LIMIT + 1),
            format!("/v1/accounts/{ACCOUNT}/payouts?after=abc"),
        ] {
            let (status, _, seen) = get(&uri, page()).await;
            assert!(status.is_client_error() && seen.is_none(), "{uri}: {status}");
        }
    }
}
