//! Authenticated client actions with sequenced access evidence.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::{Extension, Json};
use engine_core::access::{AccessDeposit, AccessEvidence, EligibilityRequest, SourceFailure};
use engine_core::command::{CancelRequest, Command, OrderRequest, WithdrawalRequest};
use engine_core::eip712::{self, cancel_hash, deposit_screening_hash, eligibility_hash, order_hash, withdrawal_hash};
use engine_core::event::Event;
use tracing::Span;
use tracing::field::display;

use crate::access::{ScreeningRequest, screen};
use crate::api::{AmountBody, ApiError, ApiResult, ApiState, SessionKeyRequest, SessionKeyRevocationRequest};
use crate::clock::now_ms;

/// Places an order and returns the events it produced.
/// Error cases:
/// - the request is not authorised for its account
/// - the sequencer has stopped, or sequencing is halted
pub(super) async fn place_order(
    State(s): State<ApiState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(mut req): Json<OrderRequest>,
) -> ApiResult<Vec<Event>> {
    let auth = req.auth.take();
    req.auth =
        s.authorise(req.account, auth, |domain, auth| order_hash(domain, &req, auth.nonce, auth.expires_at)).await?;
    let cmd = if s.access_enforced() {
        let auth = req.auth.as_ref().ok_or(ApiError::Unauthorized("missing signature"))?;
        let hash = order_hash(&signature_domain(&s), &req, auth.nonce, auth.expires_at);
        let evidence = api_screen(&s, peer, req.account, hash).await?;
        Command::AccessOrder { request: req, evidence }
    } else {
        Command::PlaceOrder(req)
    };
    Ok(Json(s.submit(cmd).await?))
}

/// Cancels a resting order.
/// Error cases:
/// - the request is not authorised for its account
/// - the sequencer has stopped, or sequencing is halted
pub(super) async fn cancel_order(State(s): State<ApiState>, Json(b): Json<CancelRequest>) -> ApiResult<Vec<Event>> {
    let auth = s
        .authorise(b.account, b.auth, |domain, auth| {
            cancel_hash(domain, b.account, b.order_id, auth.nonce, auth.expires_at)
        })
        .await?;
    Ok(Json(s.submit(Command::CancelOrder { account: b.account, order_id: b.order_id, auth }).await?))
}

/// Registers a session key the account signed for: the key may place and cancel orders for the
/// account until `expires_at` or an account-signed revocation.
/// Error cases:
/// - the registration is not authorised by the account key
/// - the sequencer has stopped, or sequencing is halted
pub(super) async fn register_session_key(
    State(s): State<ApiState>,
    Json(b): Json<SessionKeyRequest>,
) -> ApiResult<Vec<Event>> {
    let auth = s
        .authorise(b.account, Some(b.auth), |domain, auth| {
            eip712::session_key_registration_hash(
                domain,
                b.account,
                b.session_key,
                b.expires_at,
                auth.nonce,
                auth.expires_at,
            )
        })
        .await?;
    Ok(Json(
        s.submit(Command::RegisterSessionKey { account: b.account, session_key: b.session_key, expires_at: b.expires_at, auth })
            .await?,
    ))
}

/// Revokes a session key the account signed for; the key's later requests refuse distinctly.
/// Error cases:
/// - the revocation is not authorised by the account key
/// - the sequencer has stopped, or sequencing is halted
pub(super) async fn revoke_session_key(
    State(s): State<ApiState>,
    Json(b): Json<SessionKeyRevocationRequest>,
) -> ApiResult<Vec<Event>> {
    let auth = s
        .authorise(b.account, Some(b.auth), |domain, auth| {
            eip712::session_key_revocation_hash(domain, b.account, b.session_key, auth.nonce, auth.expires_at)
        })
        .await?;
    Ok(Json(s.submit(Command::RevokeSessionKey { account: b.account, session_key: b.session_key, auth }).await?))
}

/// Requests a withdrawal; an approval is signed and streamed.
/// Error cases:
/// - custody cannot pay withdrawals out yet (EVM custody without the finalized checkpoint service)
/// - the request is not authorised for its account
/// - the sequencer has stopped, or sequencing is halted
pub(super) async fn withdraw(
    State(s): State<ApiState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(b): Json<WithdrawalRequest>,
) -> ApiResult<Vec<Event>> {
    let access = s.access_enforced();
    if !access && !s.payouts_enabled {
        return Err(ApiError::NotImplemented("withdrawals require the finalized checkpoint service"));
    }
    let auth = s
        .authorise(b.account, b.auth.clone(), |domain, auth| {
            withdrawal_hash(domain, b.account, b.amount, auth.nonce, auth.expires_at)
        })
        .await?;
    let cmd = if access {
        let auth = auth.as_ref().ok_or(ApiError::Unauthorized("missing signature"))?;
        let hash = withdrawal_hash(&signature_domain(&s), b.account, b.amount, auth.nonce, auth.expires_at);
        let evidence = api_screen(&s, peer, b.account, hash).await?;
        Command::AccessWithdraw { request: b, evidence, payout_available: s.payouts_enabled }
    } else {
        Command::Withdraw { account: b.account, amount: b.amount, auth }
    };
    Ok(Json(s.submit(cmd).await?))
}

/// Screens an account's signed declaration like an order and sequences it with the evidence; the
/// engine records it only on a verified, allowed and clear screening, which its first credit and
/// order need.
/// Error cases:
/// - authentication, nonce, expiry, rate, screening capacity, or sequencer availability fails
pub(super) async fn attest_eligibility(
    State(s): State<ApiState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(request): Json<EligibilityRequest>,
) -> ApiResult<Vec<Event>> {
    s.authorise_request(
        request.account,
        Some(request.auth.clone()),
        |domain, auth| eligibility_hash(domain, request.account, request.class, auth.nonce, auth.expires_at),
        true,
        peer.as_ref().map(|Extension(ConnectInfo(peer))| peer.ip()),
    )
    .await?;
    let auth = &request.auth;
    let hash = eligibility_hash(&signature_domain(&s), request.account, request.class, auth.nonce, auth.expires_at);
    let evidence = api_screen(&s, peer, request.account, hash).await?;
    Ok(Json(s.submit(Command::AttestEligibility { request, evidence }).await?))
}

/// The existing recorded deployment domain also binds development declarations.
pub(super) fn signature_domain(state: &ApiState) -> eip712::Eip712Domain {
    state.auth.domain().unwrap_or_else(|| eip712::domain(state.operator.chain_id, state.operator.vault))
}

/// Screens the client address: the socket peer, or the one a trusted proxy's PROXY protocol header
/// names (`ConnectInfo`), never a user-supplied forwarding header.
///
/// TODO: the host controls the network, so a malicious host can present any address as the peer, or
/// as the configured proxy and name any client in its header; a relay or NAT on the host can present
/// an allowed address for any client. The console's edge geo-block and the proxy do not change
/// that. Bind geography to something the host cannot rewrite, such as a signed assertion from an
/// independent edge, before relying on the API's jurisdiction screening.
pub(super) async fn api_screen(
    state: &ApiState,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    account: engine_core::types::AccountId,
    request_hash: eip712::B256,
) -> Result<AccessEvidence, ApiError> {
    let Some(Extension(ConnectInfo(peer))) = peer else {
        return Ok(AccessEvidence::Failed { reason: SourceFailure::MissingPeer });
    };
    let _permit = state
        .access
        .as_ref()
        .map(|client| client.api_slots.try_acquire().map_err(|_| ApiError::Unavailable))
        .transpose()?;
    Ok(screen(
        state.access.as_deref(),
        &ScreeningRequest { request_hash, account: Some(account), payer: None, peer: Some(peer.ip()) },
    )
    .await)
}

/// Mock custody only: credits an account without a chain deposit, under a fresh reference.
/// Error cases:
/// - custody is not the mock provider
/// - the sequencer has stopped, or sequencing is halted
pub(super) async fn dev_deposit(State(s): State<ApiState>, Json(b): Json<AmountBody>) -> ApiResult<Vec<Event>> {
    if !s.mock_custody {
        return Err(ApiError::NotFound);
    }
    Span::current().record("id", display(b.account));
    let chain_ref = format!("dev:{}:{:016x}", now_ms(), rand::random::<u64>());
    let cmd = if s.access_enforced() {
        let deposit = AccessDeposit { account: Some(b.account), payer: Some(b.account), amount: b.amount, chain_ref };
        let _permit = s
            .access
            .as_ref()
            .map(|client| client.api_slots.try_acquire().map_err(|_| ApiError::Unavailable))
            .transpose()?;
        let evidence = screen(
            s.access.as_deref(),
            &ScreeningRequest {
                request_hash: deposit_screening_hash(&deposit),
                account: deposit.account,
                payer: deposit.payer,
                peer: None,
            },
        )
        .await;
        Command::AccessDeposit { deposit, evidence }
    } else {
        Command::Deposit { account: b.account, amount: b.amount, chain_ref }
    };
    Ok(Json(s.submit(cmd).await?))
}
