//! `GET /v1/awards`: the bug bounty awards votes approved, as the queue the epoch closes pay from its front
//! and the awards paid in full, with the cap in force, read from the API's engine copy, which applies only
//! durable batches.

use axum::Json;
use axum::extract::State;
use engine_core::Engine;
use engine_core::awards::{Award, AwardCap};
use engine_core::staking::UsdcUnits;
use serde::Serialize;

use crate::api::{ApiResult, ApiState};

/// What `GET /v1/awards` returns.
#[derive(Debug, PartialEq, Serialize)]
pub(super) struct AwardsView {
    /// The most one close pays awards: the lesser of `excess_bps` of its distributable excess and `max`.
    cap: AwardCap,
    /// The awards not yet paid in full, oldest first; each close pays from the front.
    queue: Vec<AwardView>,
    /// The awards paid in full, oldest first.
    paid: Vec<AwardView>,
    /// What the closes have paid awards in all, in USDC base units.
    paid_total: UsdcUnits,
}

/// One award, with what is still owed of it.
#[derive(Debug, PartialEq, Serialize)]
struct AwardView {
    id: u64,
    #[serde(flatten)]
    award: Award,
    owed: UsdcUnits,
}

impl AwardsView {
    /// The awards and the cap `engine` holds.
    fn of(engine: &Engine) -> Self {
        let (queue, paid): (Vec<AwardView>, Vec<AwardView>) = engine
            .awards()
            .iter()
            .map(|(id, award)| AwardView { id: *id, award: award.clone(), owed: UsdcUnits(award.owed()) })
            .partition(|view| view.owed.0 > 0);
        let paid_total = UsdcUnits(engine.awards().values().map(|award| award.paid.0).sum());
        AwardsView { cap: engine.award_cap(), queue, paid, paid_total }
    }
}

/// The awards and the cap in force.
/// Error cases:
/// - the sequencer has stopped
pub(super) async fn awards(State(s): State<ApiState>) -> ApiResult<AwardsView> {
    Ok(Json(s.read(AwardsView::of).await?))
}

#[cfg(test)]
mod tests;
