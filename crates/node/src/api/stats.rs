//! The House and MONKEY pages' data beyond `/v1/house`: `GET /v1/monkey`, `GET /v1/house/positions`,
//! `GET /v1/stats/series` and `GET /v1/monkey/activity`. Each reads the in-memory record on the
//! sequencer's thread, between batches, and is signed like every other `/v1` answer.

use axum::Json;
use axum::extract::{Query, State};
use engine_core::types::{MarketId, Millis};
use serde::Deserialize;
use tracing::Span;
use tracing::field::display;

use crate::api::{ApiError, ApiResult, ApiState};
use crate::stats::{ActivityPage, MonkeyView, PositionsView, SeriesPage, SeriesQuery};

/// Activity entries per page when the request sets no limit.
const DEFAULT_ACTIVITY_LIMIT: usize = 50;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SeriesParams {
    /// The metric's wire name.
    metric: String,
    /// The market a per-market metric covers; `volume` covers every market without one.
    market: Option<MarketId>,
    /// The bucket width: `1m`, `1h`, or `1d`.
    resolution: String,
    /// Only buckets at or after this time, in milliseconds since the epoch.
    from_ms: Option<Millis>,
    /// Only buckets at or before this time.
    to_ms: Option<Millis>,
    /// Most entries in the page.
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PositionsParams {
    market: MarketId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ActivityParams {
    limit: Option<usize>,
    /// Only entries older than this id, as a previous page's `next`.
    before: Option<u64>,
}

/// One page of a metric's series, oldest first, with where its data begins.
/// Error cases:
/// - an unknown metric or resolution, a market the metric needs and lacks or does not take or the
///   engine does not list, a limit outside range, or `from_ms` after `to_ms`
/// - the sequencer has stopped
pub(super) async fn series(State(s): State<ApiState>, Query(params): Query<SeriesParams>) -> ApiResult<SeriesPage> {
    let query = SeriesQuery {
        metric: params.metric,
        market: params.market,
        resolution: params.resolution,
        from_ms: params.from_ms,
        to_ms: params.to_ms,
        limit: params.limit,
    };
    let stats = s.stats.clone();
    let page = s.query(move |engine| stats.series(engine, &query)).await?;
    page.map(Json).map_err(ApiError::BadRequest)
}

/// The emission schedule and MONKEY totals, staking, and the distribution state.
/// Error cases:
/// - the sequencer has stopped
pub(super) async fn monkey(State(s): State<ApiState>) -> ApiResult<MonkeyView> {
    let stats = s.stats.clone();
    Ok(Json(s.query(move |engine| stats.monkey(engine)).await?))
}

/// A market's aggregate positions by close-out price and by profit, from the last scan.
/// Error cases:
/// - the market is not listed
/// - no scan has run yet
/// - the sequencer has stopped
pub(super) async fn positions(
    State(s): State<ApiState>,
    Query(params): Query<PositionsParams>,
) -> ApiResult<PositionsView> {
    Span::current().record("id", display(params.market));
    let stats = s.stats.clone();
    let view = s.query(move |engine| stats.positions(engine, params.market)).await?;
    match view.map_err(ApiError::BadRequest)? {
        Some(view) => Ok(Json(view)),
        None => Err(ApiError::NotReady("no position snapshot yet")),
    }
}

/// The newest MONKEY events, newest first.
/// Error cases:
/// - the limit is outside range
/// - the sequencer has stopped
pub(super) async fn activity(
    State(s): State<ApiState>,
    Query(params): Query<ActivityParams>,
) -> ApiResult<ActivityPage> {
    let limit = params.limit.unwrap_or(DEFAULT_ACTIVITY_LIMIT);
    let stats = s.stats.clone();
    let page = s.query(move |_| stats.activity(params.before, limit)).await?;
    page.map(Json).map_err(ApiError::BadRequest)
}
