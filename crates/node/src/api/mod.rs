//! Client API: REST for actions and reads, the operator's admin route, a WebSocket that streams
//! the public log (every sequenced command with its events, every signed checkpoint, every signed
//! withdrawal), the log archive, and account history. Every request passes the limits in `limits`.

mod admin;
mod archive;
mod benchmark_schedule;
mod history;
mod stream;
mod withdrawal_queue;
pub(crate) use stream::{Feed, MAX_BACKFILLS};

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::StatusCode;
use axum::http::header::RETRY_AFTER;
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use engine_core::account::{Position, RestingOrder};
use engine_core::auth::{ClientAuth, Operator, RequestAuth};
use engine_core::command::{CancelRequest, Command, OrderRequest, WithdrawalRequest};
use engine_core::eip712::{B256, Eip712Domain, cancel_hash, order_hash, withdrawal_hash};
use engine_core::event::{Event, Reject};
use engine_core::market::MarketSpec;
use engine_core::merkle::AccountProof;
use engine_core::types::{AccountId, MarketId, Millis, OrderId};
use engine_core::{Engine, Fixed};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Semaphore, broadcast, mpsc, oneshot, watch};
use tracing::field::{Empty, display};
use tracing::{Instrument, Span, debug, info_span};

use crate::clock::now_ms;
use crate::history::Reader;
use crate::limits::{ApiLimits, limit_client, read_body};
use crate::log::Manifest;
use crate::sequencer::{self, StreamMsg};
use crate::tee::Attestation;

type ApiResult<T> = Result<Json<T>, ApiError>;

#[derive(Debug)]
pub enum ApiError {
    Unavailable,
    /// The history index is still taking the commands the log held at startup.
    CatchingUp,
    NotFound,
    BadRequest(String),
    /// The node failed to serve the request; the cause is logged where it happened.
    Internal,
    Unauthorized(&'static str),
    Forbidden(&'static str),
    /// The node does not offer this yet.
    NotImplemented(&'static str),
    /// A signed request whose nonce is not the account's next.
    InvalidNonce {
        expected: u64,
    },
    /// Over a rate limit; the client may retry after the wait.
    RateLimited(Duration),
    TooManySubscribers,
    BodyTooLarge,
    /// The client did not send the request body within the read timeout.
    BodyTimeout,
}

#[derive(Clone)]
pub struct ApiState {
    pub tx: mpsc::Sender<sequencer::Request>,
    pub stream: broadcast::Sender<Arc<StreamMsg>>,
    pub attestation: Arc<Attestation>,
    /// The client auth the log records; `Unsigned` trusts the `account` field.
    pub auth: ClientAuth,
    /// The operator the log records, whose signature every admin request needs.
    pub operator: Operator,
    /// Mock custody credits dev deposits and pays withdrawals by logging them; EVM custody does
    /// neither until it pays withdrawals on-chain.
    pub mock_custody: bool,
    /// Mock payouts or EVM custody with the finalized checkpoint gate configured.
    pub payouts_enabled: bool,
    /// The clock a signed request's expiry is checked against; the node gives the sequencer the same
    /// one to stamp commands with.
    pub clock: fn() -> Millis,
    pub log_dir: PathBuf,
    /// The log writer's manifest, held in enclave memory.
    pub manifest: watch::Receiver<Manifest>,
    /// Permits for stream subscribers reading from disk.
    pub backfills: Arc<Semaphore>,
    pub limits: Arc<ApiLimits>,
    pub history: Reader,
}

#[derive(Serialize)]
struct MarketView {
    #[serde(flatten)]
    spec: MarketSpec,
    open: bool,
    /// Whether the market takes no new risk: its own flag, or every market's.
    reduce_only: bool,
    mark: Option<Fixed>,
    /// The FX index a market quoted in another currency than USD is valued at.
    fx_mark: Option<Fixed>,
    stress_level: usize,
}

#[derive(Serialize)]
struct HouseView {
    cash: Fixed,
    nav: Fixed,
    gross_exposure: Fixed,
}

#[derive(Serialize)]
struct AccountView {
    balance: Fixed,
    equity: Fixed,
    margin: Fixed,
    positions: BTreeMap<MarketId, Position>,
    orders: BTreeMap<OrderId, RestingOrder>,
    toxicity_bps: i64,
}

#[derive(Serialize)]
struct NonceView {
    next_nonce: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AmountBody {
    account: AccountId,
    amount: Fixed,
}

impl IntoResponse for ApiError {
    /// A JSON `{ "error": … }` body with the matching status; an invalid nonce also carries the
    /// nonce expected, and a rate-limited one `Retry-After` in whole seconds, rounded up.
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::InvalidNonce { expected } => {
                let body = json!({ "error": "invalid nonce", "expected": expected });
                return (StatusCode::CONFLICT, Json(body)).into_response();
            }
            ApiError::RateLimited(wait) => {
                let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
                let body = Json(json!({ "error": "rate limited" }));
                return (StatusCode::TOO_MANY_REQUESTS, [(RETRY_AFTER, secs.max(1).to_string())], body).into_response();
            }
            ApiError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "engine unavailable".to_owned()),
            ApiError::CatchingUp => (StatusCode::SERVICE_UNAVAILABLE, "history is catching up with the log".to_owned()),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_owned()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_owned()),
            ApiError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, m.to_owned()),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, m.to_owned()),
            ApiError::TooManySubscribers => (StatusCode::SERVICE_UNAVAILABLE, "too many stream subscribers".to_owned()),
            ApiError::BodyTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "request body too large".to_owned()),
            ApiError::BodyTimeout => (StatusCode::REQUEST_TIMEOUT, "request body not received in time".to_owned()),
            ApiError::NotImplemented(m) => (StatusCode::NOT_IMPLEMENTED, m.to_owned()),
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

impl ApiState {
    /// Checks the request may act for `account`, records it as the request's id, counts it against
    /// the account's rate, and returns the auth to sequence with it: the client's, or none where
    /// the node trusts the `account` field.
    /// `hash` gives the digest the request signs under a domain with its nonce and expiry. A signed
    /// request stays out of the log unless its account has been credited, its nonce is the
    /// account's next, and it has not expired: otherwise it could do nothing now, a nonce ahead of
    /// the next would stay replayable from the public log, and free keys or copied requests could
    /// grow the log and engine state. A request refused this way uses no nonce, so it stays valid
    /// until its expiry. The engine checks the nonce and expiry again at the sequenced time.
    /// Error cases:
    /// - signatures are required and the request has none, or one that is not `account`'s
    /// - the request is signed and the engine has never credited `account`
    /// - the request is signed with a nonce other than the account's next
    /// - the request is signed with an expiry already past by the clock or the last sequenced time
    /// - the account is over its rate
    /// - the sequencer has stopped
    async fn authorise(
        &self,
        account: AccountId,
        auth: Option<RequestAuth>,
        hash: impl FnOnce(&Eip712Domain, &RequestAuth) -> B256,
    ) -> Result<Option<RequestAuth>, ApiError> {
        Span::current().record("id", display(account));
        if self.auth == ClientAuth::Unsigned {
            // An unsigned request may act for any account it names, so the named one is counted.
            self.limits.check_account(account)?;
            return Ok(None);
        }
        if let Err(reason) = self.auth.check(account, auth.as_ref(), hash) {
            debug!(?reason, "request refused");
            return Err(ApiError::Unauthorized(match reason {
                Reject::MissingSignature => "missing signature",
                _ => "invalid signature",
            }));
        }
        let auth = auth.expect("check refuses an unsigned request wherever signatures are required");
        // Accounts are never removed and nonces only grow, so neither check can pass a request
        // that the engine then sees ahead of its account's next nonce.
        let (credited, next, last_sequenced) =
            self.query(move |e| (e.accounts().contains_key(&account), e.next_request_nonce(account), e.now())).await?;
        if !credited {
            debug!("request refused: account never credited");
            return Err(ApiError::Forbidden("account has never been credited"));
        }
        self.check_fresh(&auth, next, last_sequenced)?;
        // Counted last, so only a request the node would sequence spends the account's rate. Every
        // signed request is in the public log, so anyone can replay one; a replay fails the nonce
        // or expiry check first and spends nothing.
        self.limits.check_account(account)?;
        Ok(Some(auth))
    }

    /// Checks a signed request carries its signer's next nonce, `next`, and has not expired by the
    /// clock or by `last_sequenced`, the last sequenced time.
    /// Error cases:
    /// - a nonce other than `next`
    /// - an expiry already past
    fn check_fresh(&self, auth: &RequestAuth, next: u64, last_sequenced: Millis) -> Result<(), ApiError> {
        if auth.nonce != next {
            debug!(nonce = auth.nonce, expected = next, "request refused: invalid nonce");
            return Err(ApiError::InvalidNonce { expected: next });
        }
        // The sequencer stamps no earlier than its clock reads now or the last sequenced time, so a
        // request already expired by then can never run, unless the host turns its clock back (the
        // sequencer's trusted-time TODO). An expiry too far ahead is left to the engine, which uses
        // the nonce as it refuses it, so the request cannot run later once its expiry comes within
        // the bound.
        let now = (self.clock)().max(last_sequenced);
        if let Err(Reject::Expired { expires_at }) = auth.check_expiry(now) {
            debug!(expires_at, "request refused: expired");
            return Err(ApiError::BadRequest(format!("request expired at {expires_at}")));
        }
        Ok(())
    }

    /// Sequences a command and waits for its events.
    /// Error cases:
    /// - the sequencer has stopped, or refused the command because sequencing is halted
    async fn submit(&self, cmd: Command) -> Result<Vec<Event>, ApiError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(sequencer::Request::Submit { cmd, reply: Some(reply) })
            .await
            .map_err(|_| ApiError::Unavailable)?;
        rx.await.map_err(|_| ApiError::Unavailable)
    }

    /// Runs a read on the sequencer between batches, so it sees a consistent, already-durable
    /// state.
    /// Error cases:
    /// - the sequencer has stopped
    async fn query<T: Send + 'static>(&self, f: impl FnOnce(&Engine) -> T + Send + 'static) -> Result<T, ApiError> {
        let (reply, rx) = oneshot::channel();
        let q = Box::new(move |e: &Engine| {
            let _ = reply.send(f(e));
        });
        self.tx.send(sequencer::Request::Query(q)).await.map_err(|_| ApiError::Unavailable)?;
        rx.await.map_err(|_| ApiError::Unavailable)
    }
}

/// Every route, with the request span outermost, then the per-client rate, then the body read.
pub fn router(state: ApiState) -> Router {
    let limits = state.limits.clone();
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/attestation", get(attestation))
        .route("/v1/markets", get(markets))
        .route("/v1/benchmarks", get(benchmarks))
        .route("/v1/benchmarks/schedule", get(benchmark_schedule::schedule))
        .route("/v1/house", get(house))
        .route("/v1/accounts/{address}", get(account))
        .route("/v1/accounts/{address}/nonce", get(nonce))
        .route("/v1/accounts/{address}/history", get(history::history))
        .route("/v1/accounts/{address}/proof", get(proof))
        .route("/v1/orders", post(place_order))
        .route("/v1/orders/cancel", post(cancel_order))
        .route("/v1/withdrawals", get(withdrawal_queue::queue).post(withdraw))
        .route("/v1/accounts/{address}/withdrawals", get(withdrawal_queue::account))
        .route("/v1/admin", post(admin::admin))
        .route("/v1/dev/deposit", post(dev_deposit))
        .route("/v1/stream", get(stream::stream))
        .route("/v1/archive/{file}", get(archive::archive))
        .with_state(state)
        .layer(DefaultBodyLimit::max(limits.max_body_bytes))
        .layer(from_fn_with_state(limits.clone(), read_body))
        .layer(from_fn_with_state(limits, limit_client))
        .layer(from_fn(trace_request))
}

/// Opens the request's correlation span; handlers record the account as its `id`.
async fn trace_request(req: Request, next: Next) -> Response {
    let span = info_span!("http", method = %req.method(), path = %req.uri().path(), id = Empty);
    next.run(req).instrument(span).await
}

/// Liveness.
async fn health() -> &'static str {
    "ok"
}

/// The attestation produced at boot.
async fn attestation(State(s): State<ApiState>) -> Json<Attestation> {
    Json((*s.attestation).clone())
}

/// Every market with its live state.
/// Error cases:
/// - the sequencer has stopped
async fn markets(State(s): State<ApiState>) -> ApiResult<Vec<MarketView>> {
    let views = s
        .query(|e| {
            e.markets()
                .values()
                .map(|m| MarketView {
                    spec: m.spec.clone(),
                    open: m.open,
                    reduce_only: e.is_reduce_only(m),
                    mark: m.mark,
                    fx_mark: m.fx_mark,
                    stress_level: m.stress.level,
                })
                .collect()
        })
        .await?;
    Ok(Json(views))
}

/// Benchmark history and the next rollover; available while startup synchronization waits.
/// Error cases:
/// - the sequencer has stopped
async fn benchmarks(State(s): State<ApiState>) -> ApiResult<engine_core::benchmark::BenchmarkState> {
    Ok(Json(s.query(Engine::benchmark_state).await?))
}

/// House cash, NAV, and gross exposure.
/// Error cases:
/// - the sequencer has stopped
async fn house(State(s): State<ApiState>) -> ApiResult<HouseView> {
    let view = s
        .query(|e| HouseView { cash: e.house_cash(), nav: e.house_nav(), gross_exposure: e.house_gross_exposure() })
        .await?;
    Ok(Json(view))
}

/// One account's balance, equity, margin, positions, and orders.
/// Error cases:
/// - the address is malformed
/// - the account has never been credited
/// - the sequencer has stopped
async fn account(State(s): State<ApiState>, Path(address): Path<String>) -> ApiResult<AccountView> {
    let id: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(id));
    let view = s
        .query(move |e| {
            let acct = e.accounts().get(&id)?;
            let (equity, margin) = e.equity_and_margin(acct, None);
            Some(AccountView {
                balance: acct.balance,
                equity,
                margin,
                positions: acct.positions.clone(),
                orders: acct.orders.clone(),
                toxicity_bps: acct.toxicity_bps,
            })
        })
        .await?;
    view.map(Json).ok_or(ApiError::NotFound)
}

/// The nonce the account's next signed request must carry.
/// Error cases:
/// - the address is malformed
/// - the sequencer has stopped
async fn nonce(State(s): State<ApiState>, Path(address): Path<String>) -> ApiResult<NonceView> {
    let id: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(id));
    let next_nonce = s.query(move |e| e.next_request_nonce(id)).await?;
    Ok(Json(NonceView { next_nonce }))
}

/// The proof of the account's balance and positions, and of its positions' marks, against the
/// state root of the last checkpoint.
/// Error cases:
/// - the address is malformed
/// - no checkpoint under protocol `V9` holds the account
/// - the sequencer has stopped
async fn proof(State(s): State<ApiState>, Path(address): Path<String>) -> ApiResult<AccountProof> {
    let id: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(id));
    let proof = s.query(move |e| e.account_proof(id)).await?;
    proof.map(Json).ok_or(ApiError::NotFound)
}

/// Places an order and returns the events it produced.
/// Error cases:
/// - the request is not authorised for its account
/// - the sequencer has stopped, or sequencing is halted
async fn place_order(State(s): State<ApiState>, Json(mut req): Json<OrderRequest>) -> ApiResult<Vec<Event>> {
    let auth = req.auth.take();
    req.auth =
        s.authorise(req.account, auth, |domain, auth| order_hash(domain, &req, auth.nonce, auth.expires_at)).await?;
    Ok(Json(s.submit(Command::PlaceOrder(req)).await?))
}

/// Cancels a resting order.
/// Error cases:
/// - the request is not authorised for its account
/// - the sequencer has stopped, or sequencing is halted
async fn cancel_order(State(s): State<ApiState>, Json(b): Json<CancelRequest>) -> ApiResult<Vec<Event>> {
    let auth = s
        .authorise(b.account, b.auth, |domain, auth| {
            cancel_hash(domain, b.account, b.order_id, auth.nonce, auth.expires_at)
        })
        .await?;
    Ok(Json(s.submit(Command::CancelOrder { account: b.account, order_id: b.order_id, auth }).await?))
}

/// Requests a withdrawal; an approval is signed and streamed.
/// Error cases:
/// - custody has no finalized checkpoint service
/// - the request is not authorised for its account
/// - the sequencer has stopped, or sequencing is halted
async fn withdraw(State(s): State<ApiState>, Json(b): Json<WithdrawalRequest>) -> ApiResult<Vec<Event>> {
    if !s.payouts_enabled {
        return Err(ApiError::NotImplemented("withdrawals require the finalized checkpoint service"));
    }
    let auth = s
        .authorise(b.account, b.auth, |domain, auth| {
            withdrawal_hash(domain, b.account, b.amount, auth.nonce, auth.expires_at)
        })
        .await?;
    Ok(Json(s.submit(Command::Withdraw { account: b.account, amount: b.amount, auth }).await?))
}

/// Mock custody only: credits an account without a chain deposit, under a fresh reference.
/// Error cases:
/// - custody is not the mock provider
/// - the sequencer has stopped, or sequencing is halted
async fn dev_deposit(State(s): State<ApiState>, Json(b): Json<AmountBody>) -> ApiResult<Vec<Event>> {
    if !s.mock_custody {
        return Err(ApiError::NotFound);
    }
    Span::current().record("id", display(b.account));
    let chain_ref = format!("dev:{}:{:016x}", now_ms(), rand::random::<u64>());
    Ok(Json(s.submit(Command::Deposit { account: b.account, amount: b.amount, chain_ref }).await?))
}

#[cfg(test)]
mod admin_tests;
#[cfg(test)]
mod auth_tests;
#[cfg(test)]
mod proof_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod benchmark_tests;
