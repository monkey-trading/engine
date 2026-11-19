//! `GET /v1/accounts/{address}`: one account's balance, equity, margin, initial and maintenance
//! requirements, withdrawable cash and maximum leverage in each market, positions, orders, toxicity,
//! each position's close-out and close fee, from `V59` its volume tier, from `V60` the weekend
//! margin each position needs, from `V79` each position's take profit and stop loss, and under a
//! grid in force its portfolio requirement.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use engine_core::account::{Position, RestingOrder};
use engine_core::levels::PositionLevels;
use engine_core::portfolio::PortfolioStanding;
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
    /// From `V79`, each position's take profit and stop loss, by market, each left out while unset.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    levels: BTreeMap<MarketId, PositionLevels>,
    /// From `V81`, while the parameters name a grid, the portfolio requirement at the market's bands and
    /// at initial bands, the close-out level's share of it, and each offset group's part. The
    /// maintenance requirement and the close-outs read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    portfolio_margin: Option<PortfolioStanding>,
}

/// One account's balance, equity, margin, initial and maintenance requirements, withdrawable cash,
/// maximum leverage in each market, positions, orders, each position's close-out and close fee, its
/// volume tier, the weekend margin each position needs, each position's levels, and, from `V81` while
/// the parameters name a grid, its portfolio requirement.
/// Error cases:
/// - the address is malformed
/// - the account has never been credited
/// - the sequencer has stopped
pub(super) async fn account(State(s): State<ApiState>, Path(address): Path<String>) -> ApiResult<AccountView> {
    let id: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(id));
    let view = s.read(move |e| account_view(e, id)).await?;
    Ok(Json(view.ok_or(ApiError::NotFound)?))
}

/// `account`'s read from `e`, or `None` for an account never credited.
pub(crate) fn account_view(e: &Engine, id: AccountId) -> Option<AccountView> {
    let acct = e.accounts().get(&id)?;
    let (equity, margin) = e.equity_and_margin(acct, None);
    let rules = e.portfolio_rules();
    let requirement = if rules.is_some() { e.equity_and_closeout_requirement(acct).1 } else { margin };
    let standing = crate::stats::AccountStanding { equity, requirement, per_position: margin };
    let closeouts = crate::stats::account_closeouts(e, acct, standing);
    // The engine refuses any listing or parameters the grid does not fit, so the read always fits.
    let portfolio_margin = rules.and_then(|grid| e.portfolio_standing(id, grid).ok().flatten());
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
        levels: acct.levels.clone(),
        portfolio_margin,
    })
}
