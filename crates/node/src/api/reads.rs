//! `GET /v1/markets`: every market with its live state, and from `V60` when its weekend margin
//! applies.

use axum::Json;
use axum::extract::State;
use engine_core::Fixed;
use engine_core::market::MarketSpec;
use engine_core::weekend::WeekendWindow;
use serde::Serialize;

use crate::api::{ApiResult, ApiState};

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
    /// From `V60`, for a market with a weekend margin, when it next applies: to new risk in any
    /// market and to withdrawals from `from`, and to every holder when the weekend gap starts at
    /// `close`, until the reopen. Absent before `V60` activates.
    #[serde(skip_serializing_if = "Option::is_none")]
    weekend_margin: Option<WeekendWindow>,
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
