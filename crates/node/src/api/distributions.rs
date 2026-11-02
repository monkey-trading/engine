//! The account's cumulative USDC distribution with its proof, for the vault's `claimDistribution`.

use axum::Json;
use axum::extract::{Path, State};
use engine_core::merkle::DistributionProof;
use engine_core::types::AccountId;
use tokio::sync::oneshot;
use tracing::Span;
use tracing::field::display;

use crate::api::{ApiError, ApiResult, ApiState};
use crate::sequencer::{ClaimAnswer, Request};

/// The proof of the account's cumulative USDC distribution, as the vault's `claimDistribution`
/// takes it, answered as the MONKEY entitlement proof is: at the checkpoint the vault last recorded
/// in the signer epoch in force when the node retained one for the account, else the newest one's and
/// not `claimable`.
/// Error cases:
/// - the address is malformed
/// - no checkpoint holds a distribution for the account
/// - the sequencer has stopped
pub(super) async fn distributions(
    State(s): State<ApiState>,
    Path(address): Path<String>,
) -> ApiResult<ClaimAnswer<DistributionProof>> {
    let account: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(account));
    let (reply, rx) = oneshot::channel();
    s.tx.send(Request::DistributionClaim { account, reply }).await.map_err(|_| ApiError::Unavailable)?;
    rx.await.map_err(|_| ApiError::Unavailable)?.map(Json).ok_or(ApiError::NotFound)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::body::{Body, to_bytes};
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::{Request as HttpRequest, StatusCode};
    use engine_core::staking::UsdcUnits;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;
    use crate::api::tests::api_state;

    const STAKER: AccountId = AccountId([7; 20]);

    /// Serves the distribution route against a fake sequencer that answers `reply`.
    async fn get(reply: Option<ClaimAnswer<DistributionProof>>) -> (StatusCode, Value) {
        let (state, mut requests) = api_state(true);
        let app = crate::api::router(state).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        let sequencer = tokio::spawn(async move {
            if let Some(Request::DistributionClaim { account, reply: tx }) = requests.recv().await {
                assert_eq!(account, STAKER);
                tx.send(reply).unwrap();
            }
        });
        let uri = format!("/v1/accounts/{STAKER}/distributions");
        let response = app.oneshot(HttpRequest::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = response.status();
        let body = serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap());
        sequencer.await.unwrap();
        (status, body.unwrap_or_default())
    }

    #[tokio::test]
    async fn the_answer_carries_the_proof_with_what_it_is_proven_against() {
        let proof = DistributionProof {
            seq: 10,
            root: [1; 32].into(),
            account: STAKER,
            entitled: UsdcUnits(5_000),
            siblings: vec![[2; 32].into()],
        };
        let answer = ClaimAnswer {
            proof: proof.clone(),
            accepted_seq: Some(10),
            newest_seq: 12,
            newest_entitled: UsdcUnits(9_000),
            claimable: true,
        };
        let (status, body) = get(Some(answer)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["seq"], json!(10));
        assert_eq!(body["entitled"], json!("5000"));
        assert_eq!(body["newest_entitled"], json!("9000"));
        assert_eq!(
            (&body["accepted_seq"], &body["newest_seq"], &body["claimable"]),
            (&json!(10), &json!(12), &json!(true))
        );
        assert_eq!(serde_json::from_value::<ClaimAnswer<DistributionProof>>(body).unwrap().proof, proof);
    }

    #[tokio::test]
    async fn an_account_without_a_distribution_is_not_found() {
        assert_eq!(get(None).await.0, StatusCode::NOT_FOUND);
    }
}
