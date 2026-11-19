//! Authenticated client actions with sequenced access evidence.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use axum::extract::{ConnectInfo, State};
use axum::{Extension, Json};
use engine_core::Engine;
use engine_core::access::{AccessDeposit, AccessEvidence, EligibilityRequest, SourceFailure};
use engine_core::command::{
    CancelAllRequest, CancelRequest, CloseAllRequest, Command, OrderRequest, PositionLevelsRequest, WithdrawalRequest,
};
use engine_core::eip712::{
    self, cancel_all_hash, cancel_hash, close_all_hash, deposit_screening_hash, eligibility_hash, order_hash,
    position_levels_hash, withdrawal_hash,
};
use engine_core::event::Event;
use engine_core::types::{AccountId, MarketId};
use tracing::field::display;
use tracing::{Span, debug};

use crate::access::{AccessClient, Claim, ScreeningRequest, ScreeningSlot, SlotKind, SlotRefusal, screen};
use crate::api::state::BeforeCredit;
use crate::api::{AmountBody, ApiError, ApiResult, ApiState, SessionKeyRequest, SessionKeyRevocationRequest};
use crate::clock::now_ms;

/// The wait an account or client over its share of the screening slots is told to retry after: a
/// slot is given back as soon as the source answers.
const SHARE_RETRY: Duration = Duration::from_secs(1);

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
    req.auth = s
        .authorise(req.account, auth, true, |domain, auth| order_hash(domain, &req, auth.nonce, auth.expires_at))
        .await?;
    let cmd = if s.access_enforced() {
        let auth = req.auth.as_ref().ok_or(ApiError::Unauthorized("missing signature"))?;
        let hash = order_hash(&signature_domain(&s), &req, auth.nonce, auth.expires_at);
        let kind = if req.reduce_only {
            let (account, market) = (req.account, req.market);
            reducing_slot(&s, move |engine| holds_position(engine, account, market)).await?
        } else {
            SlotKind::Opening
        };
        let evidence = api_screen(&s, peer, req.account, hash, kind).await?;
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
        .authorise(b.account, b.auth, true, |domain, auth| {
            cancel_hash(domain, b.account, b.order_id, auth.nonce, auth.expires_at)
        })
        .await?;
    Ok(Json(s.submit(Command::CancelOrder { account: b.account, order_id: b.order_id, auth }).await?))
}

/// Cancels every resting order the account holds, on one signature and one nonce.
/// Error cases:
/// - the request is not authorised for its account
/// - the sequencer has stopped, or sequencing is halted
pub(super) async fn cancel_all(State(s): State<ApiState>, Json(b): Json<CancelAllRequest>) -> ApiResult<Vec<Event>> {
    let auth = s
        .authorise(b.account, b.auth, true, |domain, auth| {
            cancel_all_hash(domain, b.account, auth.nonce, auth.expires_at)
        })
        .await?;
    Ok(Json(s.submit(Command::CancelAll { account: b.account, auth }).await?))
}

/// Closes the account's whole position in each market the request names (from `V45` at most
/// `Fixed::MAX_INPUT` units of it), on one signature, one nonce, and one screening; the request
/// counts once against the rate limits.
/// Error cases:
/// - the request names more markets than are listed
/// - the request is not authorised for its account
/// - access screening refuses the account, or its capacity or source is unavailable
/// - the sequencer has stopped, or sequencing is halted
pub(super) async fn close_all(
    State(s): State<ApiState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(mut req): Json<CloseAllRequest>,
) -> ApiResult<Vec<Event>> {
    // A request signed over more entries than there are markets can only be refused, so it is
    // refused before its signature is hashed or a nonce is used.
    Span::current().record("id", display(req.account));
    let listed = s.query(|engine| engine.markets().len()).await?;
    if req.closes.len() > listed {
        debug!(entries = req.closes.len(), listed, "request refused: more closes than listed markets");
        return Err(ApiError::BadRequest("more closes than listed markets".into()));
    }
    let auth = req.auth.take();
    req.auth = s
        .authorise(req.account, auth, true, |domain, auth| close_all_hash(domain, &req, auth.nonce, auth.expires_at))
        .await?;
    let auth = req.auth.as_ref().ok_or(ApiError::Unauthorized("missing signature"))?;
    let hash = close_all_hash(&signature_domain(&s), &req, auth.nonce, auth.expires_at);
    let (account, markets) = (req.account, req.closes.iter().map(|close| close.market).collect::<Vec<_>>());
    let held = move |engine: &Engine| markets.iter().any(|&market| holds_position(engine, account, market));
    let kind = reducing_slot(&s, held).await?;
    let evidence = api_screen(&s, peer, req.account, hash, kind).await?;
    let cmd = Command::AccessCloseAll { request: req, evidence };
    Ok(Json(s.submit(cmd).await?))
}

/// Sets, moves or removes the take profit and stop loss of the account's position in one market, on one
/// signature, one nonce and one screening: from `V79` the position takes exactly the levels the request
/// names.
/// Error cases:
/// - the request is not authorised for its account
/// - the API's screening capacity is full
/// - the sequencer has stopped, or sequencing is halted
pub(super) async fn set_position_levels(
    State(s): State<ApiState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(mut req): Json<PositionLevelsRequest>,
) -> ApiResult<Vec<Event>> {
    let auth = req.auth.take();
    req.auth = s
        .authorise(req.account, auth, true, |domain, auth| {
            position_levels_hash(domain, &req, auth.nonce, auth.expires_at)
        })
        .await?;
    let auth = req.auth.as_ref().ok_or(ApiError::Unauthorized("missing signature"))?;
    let hash = position_levels_hash(&signature_domain(&s), &req, auth.nonce, auth.expires_at);
    // A level arranges a close, so it takes a slot as a close does.
    let (account, market) = (req.account, req.market);
    let kind = reducing_slot(&s, move |engine| holds_position(engine, account, market)).await?;
    let evidence = api_screen(&s, peer, req.account, hash, kind).await?;
    Ok(Json(s.submit(Command::AccessSetPositionLevels { request: req, evidence }).await?))
}

/// Registers a session key the account signed for: the key may place and cancel orders for the
/// account until `expires_at` or an account-signed revocation. A never-credited account may register
/// one once it has declared; the key can act only after the account's first credit.
/// Error cases:
/// - the registration is not authorised by the account key
/// - a never-credited account has not declared, or is over its client's share or the shared budget
/// - the sequencer has stopped, or sequencing is halted
pub(super) async fn register_session_key(
    State(s): State<ApiState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(b): Json<SessionKeyRequest>,
) -> ApiResult<Vec<Event>> {
    // Key management is the account key's alone: a session key's signature never reaches the log.
    let auth = s
        .authorise_request(
            b.account,
            b.auth,
            |domain, auth| {
                eip712::session_key_registration_hash(
                    domain,
                    b.account,
                    b.session_key,
                    b.expires_at,
                    auth.nonce,
                    auth.expires_at,
                )
            },
            false,
            BeforeCredit::SessionKeys,
            client_ip(&peer),
        )
        .await?;
    Ok(Json(
        s.submit(Command::RegisterSessionKey {
            account: b.account,
            session_key: b.session_key,
            expires_at: b.expires_at,
            auth,
        })
        .await?,
    ))
}

/// Revokes a session key the account signed for; the key's later requests refuse distinctly.
/// Error cases:
/// - the revocation is not authorised by the account key
/// - a never-credited account has not declared, or is over its client's share or the shared budget
/// - the sequencer has stopped, or sequencing is halted
pub(super) async fn revoke_session_key(
    State(s): State<ApiState>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(b): Json<SessionKeyRevocationRequest>,
) -> ApiResult<Vec<Event>> {
    let auth = s
        .authorise_request(
            b.account,
            b.auth,
            |domain, auth| {
                eip712::session_key_revocation_hash(domain, b.account, b.session_key, auth.nonce, auth.expires_at)
            },
            false,
            BeforeCredit::SessionKeys,
            client_ip(&peer),
        )
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
        .authorise(b.account, b.auth.clone(), true, |domain, auth| {
            withdrawal_hash(domain, b.account, b.amount, auth.nonce, auth.expires_at)
        })
        .await?;
    let cmd = if access {
        let auth = auth.as_ref().ok_or(ApiError::Unauthorized("missing signature"))?;
        let hash = withdrawal_hash(&signature_domain(&s), b.account, b.amount, auth.nonce, auth.expires_at);
        let account = b.account;
        let kind = reducing_slot(&s, move |engine| engine.withdrawable(account).is_positive()).await?;
        let evidence = api_screen(&s, peer, b.account, hash, kind).await?;
        Command::AccessWithdraw { request: b, evidence, payout_available: s.payouts_enabled }
    } else {
        Command::Withdraw { account: b.account, amount: b.amount, auth }
    };
    Ok(Json(s.submit(cmd).await?))
}

/// Screens an account's signed declaration like an order and sequences it with the evidence; the
/// engine records it only on a verified, allowed and clear screening, which its first credit, order
/// and, for a never-credited account, session keys need.
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
        false,
        BeforeCredit::Declaration,
        client_ip(&peer),
    )
    .await?;
    // A wallet declares for free, so declarations hold at most half the API's screening slots while
    // they are screened: however many arrive, orders keep a slot.
    let auth = &request.auth;
    let hash = eligibility_hash(&signature_domain(&s), request.account, request.class, auth.nonce, auth.expires_at);
    let evidence = api_screen(&s, peer, request.account, hash, SlotKind::Declaration).await?;
    Ok(Json(s.submit(Command::AttestEligibility { request, evidence }).await?))
}

/// The slot a request that claims to reduce risk takes: one that may be the reserve's (`Reducing`) when
/// `can_reduce` holds on the API's engine copy, and otherwise one outside it, as an order's (`Opening`).
/// Error cases:
/// - the sequencer has stopped
async fn reducing_slot(
    s: &ApiState,
    can_reduce: impl FnOnce(&Engine) -> bool + Send + 'static,
) -> Result<SlotKind, ApiError> {
    if s.read(can_reduce).await? {
        return Ok(SlotKind::Reducing);
    }
    debug!("request takes a slot outside the screening reserve: the account has nothing it reduces");
    Ok(SlotKind::Opening)
}

/// Whether `account` holds a position in `market`.
fn holds_position(engine: &Engine, account: AccountId, market: MarketId) -> bool {
    engine.accounts().get(&account).is_some_and(|held| !held.position(market).qty.is_zero())
}

/// The address a request's client is counted under, when its connection has one.
fn client_ip(peer: &Option<Extension<ConnectInfo<SocketAddr>>>) -> Option<IpAddr> {
    peer.as_ref().map(|Extension(ConnectInfo(peer))| peer.ip())
}

/// The existing recorded deployment domain also binds development declarations.
pub(super) fn signature_domain(state: &ApiState) -> eip712::Eip712Domain {
    state.auth.domain().unwrap_or_else(|| eip712::domain(state.operator.chain_id, state.operator.vault))
}

/// Screens the client address: the socket peer, or the one a trusted proxy's PROXY protocol header
/// names (`ConnectInfo`), never a user-supplied forwarding header, while the request holds a screening
/// slot as `kind`.
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
    kind: SlotKind,
) -> Result<AccessEvidence, ApiError> {
    let Some(Extension(ConnectInfo(peer))) = peer else {
        return Ok(AccessEvidence::Failed { reason: SourceFailure::MissingPeer });
    };
    let claim = Claim { account, client: Some(peer.ip()), request: request_hash, kind };
    let _slot = state.access.as_deref().map(|client| take_slot(client, claim)).transpose()?;
    Ok(screen(
        state.access.as_deref(),
        &ScreeningRequest { request_hash, account: Some(account), payer: None, peer: Some(peer.ip()) },
    )
    .await)
}

/// A screening slot for `claim`, or the answer its refusal gets: a copy of a request still being
/// screened conflicts with it, an account or client over its share is told to slow down, and a request
/// that finds every slot it may take held is told to retry.
/// Error cases:
/// - a copy of the request holds a slot: `Conflict`
/// - the request's account or client holds its share: `RateLimited`
/// - declarations hold their half, or every slot the request may take is held: `NotReady`
fn take_slot(client: &AccessClient, claim: Claim) -> Result<ScreeningSlot<'_>, ApiError> {
    client.api_slots.take(claim).map_err(|refusal| match refusal {
        SlotRefusal::Duplicate => ApiError::Conflict("the same request is already being screened"),
        SlotRefusal::Share => ApiError::RateLimited(SHARE_RETRY),
        SlotRefusal::Declarations => ApiError::NotReady("declaration screening is at capacity"),
        SlotRefusal::Full => ApiError::NotReady("screening is at capacity"),
    })
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
        let request_hash = deposit_screening_hash(&deposit);
        let claim = Claim { account: b.account, client: None, request: request_hash, kind: SlotKind::Opening };
        let _slot = s.access.as_deref().map(|client| take_slot(client, claim)).transpose()?;
        let evidence = screen(
            s.access.as_deref(),
            &ScreeningRequest { request_hash, account: deposit.account, payer: deposit.payer, peer: None },
        )
        .await;
        Command::AccessDeposit { deposit, evidence }
    } else {
        Command::Deposit { account: b.account, amount: b.amount, chain_ref }
    };
    Ok(Json(s.submit(cmd).await?))
}
