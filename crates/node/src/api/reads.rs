//! The market list and the account read.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Path, State};
use engine_core::account::{Position, RestingOrder};
use engine_core::market::MarketSpec;
use engine_core::types::{AccountId, MarketId, OrderId};
use engine_core::weekend::{WeekendRequirement, WeekendWindow};
use engine_core::{Engine, Fixed};
use serde::Serialize;
use tracing::Span;
use tracing::field::display;

use crate::api::{ApiError, ApiResult, ApiState};

#[derive(Serialize)]
pub(super) struct MarketView {
    #[serde(flatten)]
    spec: MarketSpec,
    open: bool,
    /// Whether the market takes no new risk: its own flag, or every market's.
    reduce_only: bool,
    mark: Option<Fixed>,
    /// The FX index a market quoted in another currency than USD is valued at.
    fx_mark: Option<Fixed>,
    stress_level: usize,
    /// From `V59`, for a market with a weekend margin, when it next applies: to opens and adds
    /// from `from`, and to every holder when the weekend gap starts at `close`, until the reopen.
    #[serde(skip_serializing_if = "Option::is_none")]
    weekend_margin: Option<WeekendWindow>,
}

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
    /// From `V59`, for each position in a market with a weekend margin, when it applies and the
    /// margin requirement the account must meet when its weekend gap starts, at its marks now.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    weekend_margins: BTreeMap<MarketId, WeekendRequirement>,
}

/// Every market with its live state.
/// Error cases:
/// - the sequencer has stopped
pub(super) async fn markets(State(s): State<ApiState>) -> ApiResult<Vec<MarketView>> {
    let views = s
        .read(|e| {
            e.markets()
                .values()
                .map(|m| MarketView {
                    spec: m.spec.clone(),
                    open: m.open,
                    reduce_only: e.is_reduce_only(m),
                    mark: m.mark,
                    fx_mark: m.fx_mark,
                    stress_level: m.stress.level,
                    weekend_margin: m.weekend_window(),
                })
                .collect()
        })
        .await?;
    Ok(Json(views))
}

/// One account's balance, equity, margin, positions, orders, each position's close-out and close fee,
/// and from `V59` the weekend margin each position needs.
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
        weekend_margins: e.weekend_requirements(id),
    })
}
