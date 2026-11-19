//! `GET /v1/accounts/{address}/preview`: what a market order of an exact size would do now, from the same
//! pricing and checks a fill runs (`Engine::preview_order`): the price it would fill at, the account's equity
//! and its initial and maintenance requirements after it, and why it would be refused. Read on the API's
//! engine copy, off the sequencer's thread, and naming the seq it read. It is by address and unsigned, as
//! the account read is, and counts against its client's request rate, as every read does.

use axum::Json;
use axum::extract::{Path, Query, State};
use engine_core::types::{AccountId, MarketId, Side};
use engine_core::{Fixed, OrderPreview};
use serde::{Deserialize, Serialize};
use tracing::Span;
use tracing::field::display;

use crate::api::{ApiError, ApiResult, ApiState};

/// The order to preview: a market order of `qty` on `side` in `market`, reduce-only when `reduce_only`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PreviewParams {
    market: MarketId,
    side: Side,
    qty: Fixed,
    #[serde(default)]
    reduce_only: bool,
}

/// An order's preview, with the order it previews and the seq of the state it read.
#[derive(Serialize)]
pub(super) struct PreviewView {
    /// The last command the engine copy it read had applied; `None` before any.
    seq: Option<u64>,
    account: AccountId,
    market: MarketId,
    side: Side,
    qty: Fixed,
    reduce_only: bool,
    #[serde(flatten)]
    preview: OrderPreview,
}

/// What a market order of `qty` on `side` in `market` would do for the account now, were it sequenced next
/// with nothing moved. Access screening and an order's levels are not part of it.
/// Error cases:
/// - the address or a parameter is malformed
/// - the account has never been credited
/// - the sequencer has stopped
pub(super) async fn preview(
    State(s): State<ApiState>,
    Path(address): Path<String>,
    Query(params): Query<PreviewParams>,
) -> ApiResult<PreviewView> {
    let account: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(account));
    let PreviewParams { market, side, qty, reduce_only } = params;
    let read = s
        .read(move |e| e.preview_order(account, market, side, qty, reduce_only).map(|preview| (preview, e.last_seq())))
        .await?;
    let (preview, seq) = read.ok_or(ApiError::NotFound)?;
    Ok(Json(PreviewView { seq, account, market, side, qty, reduce_only, preview }))
}
