//! An account's signed request for a fresh authorization of an approval whose last one expired
//! unclaimed.

use std::net::SocketAddr;

use alloy::primitives::{Address, keccak256};
use alloy::sol_types::SolValue;
use axum::extract::{ConnectInfo, Path, State};
use axum::{Extension, Json};
use engine_core::eip712::{self, Signature};
use engine_core::types::AccountId;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tracing::{Span, debug, field::display};

use crate::api::actions::{api_screen, signature_domain};
use crate::api::{ApiError, ApiResult, ApiState};
use crate::sequencer::{Reissue, ReissueScreening, Request};

/// The account's request to replace the authorization of its payout `payoutNonce` that expires at
/// `replacesExpiresAt` (0 for an approval never signed). The replaced expiry makes one signature
/// good for one reissue.
const TYPE: &str = "ReissueRequest(address account,uint64 payoutNonce,uint64 replacesExpiresAt)";

/// The signed body: the expiry of the authorization being replaced, and the account's signature.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Body {
    replaces_expires_at: u64,
    signature: Signature,
}

/// The digest an account signs to replace the authorization expiring at `replaces_expires_at`.
pub(crate) fn reissue_hash(
    domain: &eip712::Eip712Domain,
    account: AccountId,
    payout_nonce: u64,
    replaces_expires_at: u64,
) -> eip712::B256 {
    let value = keccak256((keccak256(TYPE), Address::from(account.0), payout_nonce, replaces_expires_at).abi_encode());
    keccak256([&[0x19, 0x01][..], domain.hash_struct().as_slice(), value.as_slice()].concat())
}

/// Queues the approval for signing again once the account proves control of it and, once access is
/// enforced, passes the account and peer screening a new withdrawal gets; the authorization
/// arrives on the stream. The node never reissues on a schedule, so this is the only way an expired
/// authorization is replaced.
/// Error cases:
/// - the address or nonce is invalid, the signature is not the account's, or the account is over
///   its rate
/// - screening refuses the account or its peer, or the screening capacity is full
/// - no unpaid approval is loaded under the nonce, or the request replaces an authorization other
///   than the latest, or the latest is still usable
/// - there is no checkpoint gate, or the sequencer has stopped
pub(super) async fn reissue(
    State(s): State<ApiState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Path((address, nonce)): Path<(String, String)>,
    Json(body): Json<Body>,
) -> ApiResult<Value> {
    let account: AccountId = address.parse().map_err(|error| ApiError::BadRequest(format!("{error}")))?;
    Span::current().record("id", display(account));
    let nonce: u64 = nonce.parse().map_err(|_| ApiError::BadRequest("the payout nonce is not a number".into()))?;
    let hash = reissue_hash(&signature_domain(&s), account, nonce, body.replaces_expires_at);
    if eip712::recover(&hash, &body.signature) != Some(account) {
        debug!("reissue refused: the signature is not the account's");
        return Err(ApiError::Unauthorized("invalid signature"));
    }
    s.limits.check_account(account)?;
    let screening = if s.access_enforced() {
        Some(ReissueScreening { request_hash: hash, evidence: api_screen(&s, peer, account, hash).await? })
    } else {
        None
    };
    let (reply, rx) = oneshot::channel();
    let request = Request::ReissueWithdrawal { account, nonce, replaces: body.replaces_expires_at, screening, reply };
    s.tx.send(request).await.map_err(|_| ApiError::Unavailable)?;
    let outcome = rx.await.map_err(|_| ApiError::Unavailable)?;
    debug!(nonce, ?outcome, "reissue request answered");
    match outcome {
        Reissue::Queued => Ok(Json(json!({ "status": "queued", "resend_after_restart": true }))),
        Reissue::Unknown => Err(ApiError::NotFound),
        Reissue::Stale { current } => {
            Err(ApiError::BadRequest(format!("the latest authorization expires at {current}, not the one replaced")))
        }
        Reissue::StillValid { expires_at } => {
            Err(ApiError::BadRequest(format!("the latest authorization is usable until {expires_at}")))
        }
        Reissue::Refused { reason } => Err(ApiError::AccessRefused(reason)),
        Reissue::Unavailable => Err(ApiError::Unavailable),
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::body::Body as HttpBody;
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::{Request as HttpRequest, StatusCode};
    use engine_core::access::AccessReject;
    use k256::ecdsa::SigningKey;
    use tower::ServiceExt;

    use super::*;
    use crate::api::tests::api_state;

    const ACCOUNT: u8 = 1;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_slice(&[seed; 32]).unwrap()
    }

    fn signed(state: &ApiState, signer: &SigningKey, account: AccountId, nonce: u64, replaces: u64) -> Value {
        let hash = reissue_hash(&signature_domain(state), account, nonce, replaces);
        json!({ "replaces_expires_at": replaces, "signature": eip712::sign(signer, &hash) })
    }

    /// The router's status for a post, and the request the sequencer saw, if it saw one. The fake
    /// sequencer answers `outcome`.
    async fn post(path: String, body: Value, outcome: Reissue) -> (StatusCode, Option<(AccountId, u64, u64)>) {
        let (state, mut requests) = api_state(true);
        let app = crate::api::router(state).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9))));
        let sequencer = tokio::spawn(async move {
            let Some(Request::ReissueWithdrawal { account, nonce, replaces, reply, .. }) = requests.recv().await else {
                return None;
            };
            reply.send(outcome).unwrap();
            Some((account, nonce, replaces))
        });
        let request = HttpRequest::post(path)
            .header("content-type", "application/json")
            .body(HttpBody::from(body.to_string()))
            .unwrap();
        let status = app.oneshot(request).await.unwrap().status();
        sequencer.abort();
        (status, sequencer.await.ok().flatten())
    }

    #[tokio::test]
    async fn each_sequencer_answer_maps_to_its_status_through_the_router() {
        let signer = key(ACCOUNT);
        let account = eip712::address(signer.verifying_key());
        let (state, _) = api_state(true);
        let body = signed(&state, &signer, account, 7, 90);
        for (outcome, status) in [
            (Reissue::Queued, StatusCode::OK),
            (Reissue::Unknown, StatusCode::NOT_FOUND),
            (Reissue::Stale { current: 9 }, StatusCode::BAD_REQUEST),
            (Reissue::StillValid { expires_at: 9 }, StatusCode::BAD_REQUEST),
            (Reissue::Refused { reason: AccessReject::AccountSanctioned }, StatusCode::FORBIDDEN),
            (Reissue::Unavailable, StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let path = format!("/v1/accounts/{account}/withdrawals/7/reissue");
            let (got, seen) = post(path, body.clone(), outcome).await;
            assert_eq!((got, seen), (status, Some((account, 7, 90))), "{outcome:?}");
        }
    }

    #[tokio::test]
    async fn a_request_not_signed_by_the_account_never_reaches_the_sequencer() {
        let owner = key(ACCOUNT);
        let account = eip712::address(owner.verifying_key());
        let (state, _) = api_state(true);
        let path = format!("/v1/accounts/{account}/withdrawals/7/reissue");
        // Another key, another nonce, and another replaced expiry each recover a different signer.
        for body in [
            signed(&state, &key(2), account, 7, 90),
            signed(&state, &owner, account, 8, 90),
            json!({ "replaces_expires_at": 91, "signature": signed(&state, &owner, account, 7, 90)["signature"] }),
        ] {
            let (status, seen) = post(path.clone(), body, Reissue::Queued).await;
            assert_eq!((status, seen), (StatusCode::UNAUTHORIZED, None));
        }
        let (status, seen) = post(path, json!({ "replaces_expires_at": 90 }), Reissue::Queued).await;
        assert_eq!((status, seen), (StatusCode::UNPROCESSABLE_ENTITY, None), "an unsigned body is refused");
    }

    #[tokio::test]
    async fn malformed_addresses_and_nonces_are_json_bad_requests() {
        let signer = key(ACCOUNT);
        let account = eip712::address(signer.verifying_key());
        let (state, _) = api_state(true);
        let body = signed(&state, &signer, account, 7, 0);
        for path in [
            "/v1/accounts/nonsense/withdrawals/7/reissue".to_owned(),
            format!("/v1/accounts/{account}/withdrawals/abc/reissue"),
        ] {
            let (status, seen) = post(path, body.clone(), Reissue::Queued).await;
            assert_eq!((status, seen), (StatusCode::BAD_REQUEST, None));
        }
    }
}
