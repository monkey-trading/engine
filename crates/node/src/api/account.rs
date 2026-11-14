//! `GET /v1/accounts/{address}`: one account's balance, equity, margin, positions, orders, toxicity,
//! each position's close-out and close fee, and from `V59` its volume tier.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use engine_core::account::{Position, RestingOrder};
use engine_core::types::{AccountId, MarketId, OrderId};
use engine_core::{Engine, Fixed};
use serde::Serialize;
use tracing::Span;
use tracing::field::display;

use crate::api::{ApiError, ApiResult, ApiState};

#[derive(Serialize)]
pub(crate) struct AccountView {
    balance: Fixed,
    #[serde(skip_serializing_if = "Fixed::is_zero")]
    unfinalised_deposits: Fixed,
    equity: Fixed,
    margin: Fixed,
    positions: BTreeMap<MarketId, Position>,
    orders: BTreeMap<OrderId, RestingOrder>,
    /// The `V11` score in whole bps, before `V54`; zero from it.
    toxicity_bps: i64,
    /// From `V54`, the toxicity score, its decayed fills, its tier and the spread the tier adds.
    #[serde(skip_serializing_if = "Option::is_none")]
    toxicity: Option<engine_core::toxicity::ToxicityStanding>,
    /// Where each position's market must reach for the account to fall below its close-out level.
    closeouts: Vec<crate::stats::PositionCloseout>,
    /// From `V55`, what closing each position pays now: its open time, the fee rate and when it falls.
    close_fees: BTreeMap<MarketId, engine_core::holding_fee::CloseFee>,
    /// From `V59`, the volume tier, the counted and pending volume, the progress towards each tier and
    /// each market's best-price bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    volume: Option<engine_core::volume_tier::VolumeStanding>,
}

/// One account's balance, equity, margin, positions, orders, each position's close-out and close fee,
/// and its volume tier.
/// Error cases:
/// - the address is malformed
/// - the account has never been credited
/// - the sequencer has stopped
pub(super) async fn account(State(s): State<ApiState>, Path(address): Path<String>) -> ApiResult<AccountView> {
    let id: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(id));
    let view = s.read(move |e| account_view(e, id)).await?;
    view.map(Json).ok_or(ApiError::NotFound)
}

/// `account`'s read from `e`, or `None` for an account never credited.
pub(crate) fn account_view(e: &Engine, id: AccountId) -> Option<AccountView> {
    let acct = e.accounts().get(&id)?;
    let (equity, margin) = e.equity_and_margin(acct, None);
    let closeouts = crate::stats::account_closeouts(e, acct, (equity, margin));
    Some(AccountView {
        balance: acct.balance,
        unfinalised_deposits: acct.unfinalised_deposits,
        equity,
        margin,
        positions: acct.positions.clone(),
        orders: acct.orders.clone(),
        toxicity_bps: acct.toxicity_bps,
        toxicity: e.toxicity_standing(id),
        closeouts,
        close_fees: e.close_fees(id),
        volume: e.volume_standing(id),
    })
}
