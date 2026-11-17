//! `GET /v1/accounts/{address}`: one account's balance, equity, margin, initial and maintenance
//! requirements, withdrawable cash and maximum leverage in each market, positions, orders, toxicity,
//! each position's close-out and close fee, from `V59` its volume tier, from `V60` the weekend
//! margin each position needs, and under a `[portfolio_margin]` grid its portfolio requirement.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use engine_core::account::{Position, RestingOrder};
use engine_core::portfolio::{PortfolioGrid, PortfolioStanding};
use engine_core::types::{AccountId, MarketId, OrderId};
use engine_core::{Engine, Fixed};
use serde::Serialize;
use tracing::field::display;
use tracing::{Span, warn};

use crate::api::{ApiError, ApiResult, ApiState};

#[derive(Serialize)]
pub(crate) struct AccountView {
    balance: Fixed,
    #[serde(skip_serializing_if = "Fixed::is_zero")]
    unfinalised_deposits: Fixed,
    equity: Fixed,
    margin: Fixed,
    /// The initial requirement, from `V64` at the account's tier's margin floor, the maintenance
    /// requirement a close-out reads, the withdrawable cash and each market's maximum leverage.
    #[serde(flatten)]
    standing: engine_core::engine::MarginStanding,
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
    /// From `V60`, for each position in a market with a weekend margin, when it applies and the
    /// margin requirement the account must meet when its weekend gap starts, at its marks now.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    weekend_margins: BTreeMap<MarketId, engine_core::weekend::WeekendRequirement>,
    /// Under the configured grid, the portfolio requirement at the market's bands and at initial
    /// bands, the close-out level's share of it, and each offset group's part; nothing decides on it.
    #[serde(skip_serializing_if = "Option::is_none")]
    portfolio_margin: Option<PortfolioStanding>,
    /// Why the configured grid gave no portfolio requirement: it does not fit the listings.
    #[serde(skip)]
    portfolio_refusal: Option<String>,
}

/// One account's balance, equity, margin, initial and maintenance requirements, withdrawable cash,
/// maximum leverage in each market, positions, orders, each position's close-out and close fee, its
/// volume tier, the weekend margin each position needs, and its portfolio requirement under the
/// configured grid. A grid that does not fit the listings gives none and raises Warning
/// `portfolio_grid_mismatch`.
/// Error cases:
/// - the address is malformed
/// - the account has never been credited
/// - the sequencer has stopped
pub(super) async fn account(State(s): State<ApiState>, Path(address): Path<String>) -> ApiResult<AccountView> {
    let id: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(id));
    let grid = s.portfolio_margin.clone();
    let view = s.read(move |e| account_view(e, id, grid.as_deref())).await?.ok_or(ApiError::NotFound)?;
    if let Some(reason) = &view.portfolio_refusal {
        warn!(
            alert = "portfolio_grid_mismatch",
            %reason,
            "the portfolio margin grid does not fit the listings, so the account read gives no portfolio requirement"
        );
    }
    Ok(Json(view))
}

/// `account`'s read from `e`, with its portfolio requirement under `grid` when one is configured, or
/// `None` for an account never credited.
pub(crate) fn account_view(e: &Engine, id: AccountId, grid: Option<&PortfolioGrid>) -> Option<AccountView> {
    let acct = e.accounts().get(&id)?;
    let (equity, margin) = e.equity_and_margin(acct, None);
    let closeouts = crate::stats::account_closeouts(e, acct, (equity, margin));
    let (portfolio_margin, portfolio_refusal) = match grid.map(|grid| e.portfolio_standing(id, grid)) {
        Some(Ok(standing)) => (standing, None),
        Some(Err(reason)) => (None, Some(reason)),
        None => (None, None),
    };
    Some(AccountView {
        balance: acct.balance,
        unfinalised_deposits: acct.unfinalised_deposits,
        equity,
        margin,
        standing: e.margin_standing(id)?,
        positions: acct.positions.clone(),
        orders: acct.orders.clone(),
        toxicity_bps: acct.toxicity_bps,
        toxicity: e.toxicity_standing(id),
        closeouts,
        close_fees: e.close_fees(id),
        volume: e.volume_standing(id),
        weekend_margins: e.weekend_requirements(id),
        portfolio_margin,
        portfolio_refusal,
    })
}
