//! `GET /v1/markets/{id}/marks`: a market's index-mark history from the derived marks cache, one
//! resolution and one bounded window at a time, every entry labeled log-derived or provider-seeded
//! so a client can weigh seeded history: a bucket a log mark reached holds exactly what the log
//! proves. Like every other read, the answer is signed; a window wider than the page cap pages by
//! following `next` as `from_ms`.

use axum::Json;
use axum::extract::{Path, Query, State};
use engine_core::types::{MarketId, Millis};
use serde::Deserialize;
use tracing::field::display;
use tracing::{Span, error};

use crate::api::{ApiError, ApiResult, ApiState};
use crate::history::marks::{MarksPage, Resolution};

/// Entries per page when the request sets no limit.
const DEFAULT_LIMIT: usize = 500;
/// Most entries per page, which bounds what one request reads and serves.
pub(crate) const MAX_LIMIT: usize = 2000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarksParams {
    /// The bucket width: `1s`, `1m`, `1h`, or `1d`.
    pub(crate) resolution: String,
    /// Only buckets at or after this time, in milliseconds since the epoch.
    pub(crate) from_ms: Option<Millis>,
    /// Only buckets at or before this time.
    pub(crate) to_ms: Option<Millis>,
    /// Most entries in the page: 1 to `MAX_LIMIT`, `DEFAULT_LIMIT` when absent.
    pub(crate) limit: Option<usize>,
}

/// One page of a market's marks at one resolution, oldest first, over the inclusive window
/// [`from_ms`, `to_ms`], at most `MAX_LIMIT` entries; a longer window pages by following `next`.
/// A market no bucket has reached yet answers an empty page.
/// Error cases:
/// - the market id is not a number, the resolution unknown, the limit outside 1 to `MAX_LIMIT`,
///   or `from_ms` after `to_ms`
/// - the cache cannot be read
pub async fn marks(
    State(s): State<ApiState>,
    Path(id): Path<String>,
    Query(params): Query<MarksParams>,
) -> ApiResult<MarksPage> {
    let market: MarketId = id.parse().map_err(|_| ApiError::BadRequest(format!("market id {id:?} is not a number")))?;
    Span::current().record("id", display(market));
    let resolution = Resolution::parse(&params.resolution)
        .ok_or_else(|| ApiError::BadRequest("resolution must be one of 1s, 1m, 1h, 1d".to_owned()))?;
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest(format!("limit must be from 1 to {MAX_LIMIT}")));
    }
    let (from, to) = (params.from_ms.unwrap_or(i64::MIN), params.to_ms.unwrap_or(i64::MAX));
    if from > to {
        return Err(ApiError::BadRequest("from_ms must not be after to_ms".to_owned()));
    }
    s.marks.page(market, resolution, from, to, limit, MAX_LIMIT).await.map(Json).map_err(|e| {
        error!(alert = "marks_unreadable", "{e:#}");
        ApiError::Internal
    })
}
