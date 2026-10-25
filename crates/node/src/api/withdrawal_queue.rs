//! Public FIFO withdrawal reservations and an account's positions within that queue.

use axum::Json;
use axum::extract::{Path, Query, State};
use engine_core::types::AccountId;
use engine_core::withdrawal::QueuedWithdrawal;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult, ApiState};

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 200;

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Params {
    after: Option<u64>,
    limit: Option<usize>,
}

#[derive(Serialize)]
pub(super) struct Position {
    /// One-based position in the complete FIFO at this sequence.
    position: u64,
    #[serde(flatten)]
    request: QueuedWithdrawal,
}

#[derive(Serialize)]
pub(super) struct Page {
    seq: Option<u64>,
    queue: Vec<Position>,
    next: Option<u64>,
}

/// Returns a page of the global public queue in request sequence order.
/// Error cases:
/// - invalid pagination, or the sequencer has stopped
pub(super) async fn queue(State(state): State<ApiState>, Query(params): Query<Params>) -> ApiResult<Page> {
    page(state, params, None).await
}

/// Returns an account's queued requests with positions measured against the complete FIFO.
/// Error cases:
/// - invalid address/pagination, or the sequencer has stopped
pub(super) async fn account(
    State(state): State<ApiState>,
    Path(address): Path<String>,
    Query(params): Query<Params>,
) -> ApiResult<Page> {
    let account = address.parse().map_err(|error| ApiError::BadRequest(format!("{error}")))?;
    page(state, params, Some(account)).await
}

/// Takes a consistent view through the engine owner's query channel.
/// Error cases:
/// - invalid pagination, or the sequencer has stopped
async fn page(state: ApiState, params: Params, account: Option<AccountId>) -> ApiResult<Page> {
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest(format!("limit must be from 1 to {MAX_LIMIT}")));
    }
    let page = state
        .query(move |engine| {
            let page = engine.withdrawal_queue_page(account, params.after, limit);
            let queue = page
                .queue
                .into_iter()
                .map(|(position, request)| Position { position, request: *request })
                .collect::<Vec<_>>();
            Page { seq: engine.last_seq(), queue, next: page.next }
        })
        .await?;
    Ok(Json(page))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use axum::body::{Body, to_bytes};
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::{Request, StatusCode};
    use engine_core::auth::ClientAuth;
    use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced};
    use engine_core::event::Event;
    use engine_core::price::SourceQuote;
    use engine_core::protocol::ProtocolVersion;
    use engine_core::types::Side;
    use engine_core::{Engine, Fixed};
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;
    use crate::sequencer::Request as SequencerRequest;
    use crate::test_support::dev_config;

    const A: AccountId = AccountId([1; 20]);
    const B: AccountId = AccountId([2; 20]);
    const NOW: i64 = 1_792_504_800_000;

    #[tokio::test]
    async fn global_and_account_pages_preserve_fifo_positions_and_bounded_scan_cursors() {
        let (state, mut requests) = crate::api::tests::api_state(true);
        let engine = queued_engine();
        let expected = engine.withdrawal_queue().map(|q| q.requested_seq).collect::<Vec<_>>();
        let owner = tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                if let SequencerRequest::Query(query) = request {
                    query(&engine);
                } else {
                    panic!("read-only queue route");
                }
            }
        });
        let app = crate::api::router(state).layer(MockConnectInfo("127.0.0.1:1234".parse::<SocketAddr>().unwrap()));
        let first = get(app.clone(), "/v1/withdrawals?limit=1").await;
        assert_eq!(first["queue"][0]["account"], A.to_string());
        assert_eq!(first["queue"][0]["remaining"], "80");
        assert_eq!(first["queue"][0]["position"], 1);
        assert_eq!(first["next"], expected[0]);
        let second = get(app.clone(), &format!("/v1/withdrawals?limit=1&after={}", expected[0])).await;
        assert_eq!(second["queue"][0]["account"], B.to_string());
        assert_eq!(second["queue"][0]["position"], 2);
        assert_eq!(second["queue"][0]["remaining"], "110");
        assert!(second["next"].is_null());
        let empty = get(app.clone(), &format!("/v1/accounts/{B}/withdrawals?limit=1")).await;
        assert_eq!(empty["queue"], serde_json::json!([]));
        assert_eq!(empty["next"], expected[0], "an empty filtered page still advances");
        let account = get(app.clone(), &format!("/v1/accounts/{B}/withdrawals?limit=1&after={}", expected[0])).await;
        assert_eq!(account["queue"][0]["account"], B.to_string());
        assert_eq!(account["queue"][0]["position"], 2);
        assert_eq!(account["seq"], first["seq"]);
        let response =
            app.oneshot(Request::builder().uri("/v1/withdrawals?limit=0").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        owner.abort();
    }

    async fn get(app: axum::Router, uri: &str) -> Value {
        let response = app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        serde_json::from_slice(&to_bytes(response.into_body(), 16_384).await.unwrap()).unwrap()
    }

    /// Creates the shortfall through fee-free fills, then reserves both actual winning balances.
    fn queued_engine() -> Engine {
        let mut engine = Engine::new();
        let config = dev_config();
        apply(&mut engine, Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)));
        apply(&mut engine, Command::Admin(AdminCommand::SetOperator(config.operator().unwrap())));
        apply(&mut engine, Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V22 }));
        let mut params = config.genesis.params;
        params.profit_fee_bps = 0;
        params.stress_levels.truncate(1);
        params.toxicity.tiers.iter_mut().for_each(|tier| tier.extra_spread_bps = 0);
        params.house_total_cap_floor = Fixed::from_int(1_000_000);
        apply(&mut engine, Command::Admin(AdminCommand::SetParams(params)));
        let mut market = config.genesis.markets.into_iter().find(|market| market.id == 1).unwrap();
        market.house_cap_floor = Fixed::from_int(1_000_000);
        market.spread_tiers.iter_mut().for_each(|tier| tier.bps = 0);
        apply(&mut engine, Command::Admin(AdminCommand::UpsertMarket(market)));
        apply(&mut engine, Command::HouseCapital { amount: Fixed::from_int(10), chain_ref: "queue-api-house".into() });
        for account in [A, B] {
            apply(
                &mut engine,
                Command::Deposit { account, amount: Fixed::from_int(10), chain_ref: account.to_string() },
            );
        }
        price(&mut engine, 100);
        for account in [A, B] {
            trade(&mut engine, account, Side::Buy, 100);
        }
        price(&mut engine, 200);
        for account in [A, B] {
            trade(&mut engine, account, Side::Sell, 200);
        }
        for account in [A, B] {
            apply(&mut engine, Command::Withdraw { account, amount: Fixed::from_int(110), auth: None });
        }
        assert_eq!(engine.withdrawal_queue().count(), 2);
        engine
    }

    fn price(engine: &mut Engine, mid: i64) {
        let quotes = ["a", "b", "c"]
            .map(|source| SourceQuote { source: source.into(), mid: Fixed::from_int(mid), ts: NOW, contract: None })
            .to_vec();
        apply(engine, Command::Prices { market: 1, quotes });
    }

    fn trade(engine: &mut Engine, account: AccountId, side: Side, price: i64) {
        let events = apply(
            engine,
            Command::PlaceOrder(OrderRequest {
                account,
                client_id: 0,
                market: 1,
                side,
                qty: Fixed::ONE,
                kind: OrderKind::Market { quoted: Fixed::from_int(price), max_slippage_bps: None },
                reduce_only: false,
                auth: None,
            }),
        );
        assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
    }

    fn apply(engine: &mut Engine, cmd: Command) -> Vec<Event> {
        let events =
            engine.apply(&Sequenced { seq: engine.last_seq().map_or(0, |seq| seq + 1), ts: NOW, cmd }).unwrap();
        assert!(!events.iter().any(|event| matches!(event, Event::AdminRejected { .. })), "{events:?}");
        events
    }
}
