//! The operator key in effect, its pending rotation, and the operator registry as the log mirrors
//! it: whether rotation is on, the last election, and from `V51` the rotation sets and daily turns,
//! with the turn history paged from the log.

use axum::Json;
use axum::extract::{Query, State};
use engine_core::Engine;
use engine_core::election::Election;
use engine_core::operator_bounds::PendingRotation;
use engine_core::turns::{SetMember, TurnAssignment};
use engine_core::types::{AccountId, Millis};
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult, ApiState};
use crate::stats::TurnsPage;

/// Turn history records per page by default.
const DEFAULT_TURNS_LIMIT: usize = 20;

/// What `GET /v1/operator` returns.
#[derive(Serialize)]
pub(super) struct OperatorView {
    /// The key whose signed admin commands apply now: the one the log records, or before it records
    /// one, the configured key.
    operator: AccountId,
    /// The key that takes over at its `effective_at`, by an election or, while rotation is off, a
    /// signed rotation.
    pending_rotation: Option<PendingRotation>,
    rotation_enabled: bool,
    last_election: Option<Election>,
    /// The newest mirrored rotation set in effect on chain by the log's sequenced time, which turns
    /// are drawn from.
    rotation_set: Option<SetView>,
    /// The mirrored set that takes effect next, and from when (`effective_at`).
    next_rotation_set: Option<SetView>,
    /// Today's turn: the assignment whose key is in effect, and whether its member has taken it.
    turn: Option<TurnView>,
    /// Tomorrow's turn, once it is drawn and mirrored.
    next_turn: Option<TurnView>,
    /// The turn assignments the log mirrored, newest first.
    turns: TurnsPage,
}

/// A mirrored rotation set.
#[derive(Serialize)]
struct SetView {
    set: u32,
    epoch: u64,
    effective_at: Millis,
    logged_at: Millis,
    members: Vec<SetMember>,
}

/// A turn assignment the engine accepted.
#[derive(Serialize)]
struct TurnView {
    #[serde(flatten)]
    assignment: TurnAssignment,
    /// Whether its member's enclave has logged `TurnTaken` for it.
    taken: bool,
}

/// `GET /v1/operator`'s paging of the turn history.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OperatorParams {
    /// Records mirrored before this seq: a previous page's `next`.
    before: Option<u64>,
    limit: Option<usize>,
}

/// The operator in effect, the pending rotation and its time, whether rotation is on, the last
/// election, and the daily turns, from the engine and the record at one durable seq.
/// Error cases:
/// - the turn history's limit is outside range
/// - the sequencer has stopped
pub(super) async fn operator(
    State(s): State<ApiState>,
    Query(params): Query<OperatorParams>,
) -> ApiResult<OperatorView> {
    let configured = s.operator.address;
    let stats = s.stats.clone();
    let limit = params.limit.unwrap_or(DEFAULT_TURNS_LIMIT);
    let view = s
        .query_durable(move |e| {
            stats.turns(params.before, limit).map(|turns| {
                let (rotation_set, next_rotation_set) = sets(e);
                let daily = e.daily_turns();
                let taken = |assignment: &TurnAssignment| {
                    daily
                        .taken
                        .is_some_and(|taken| (taken.turn, taken.position) == (assignment.turn, assignment.position))
                };
                let view = |assignment: TurnAssignment| TurnView { taken: taken(&assignment), assignment };
                OperatorView {
                    operator: e.operator().map_or(configured, |operator| operator.address),
                    pending_rotation: e.operator_bounds().rotation,
                    rotation_enabled: e.operator_election().rotation_enabled,
                    last_election: e.operator_election().last,
                    rotation_set,
                    next_rotation_set,
                    turn: daily.in_effect.map(view),
                    next_turn: daily.assigned.values().next().copied().map(view),
                    turns,
                }
            })
        })
        .await?;
    view.map(Json).map_err(ApiError::BadRequest)
}

/// The newest mirrored set in effect by `engine`'s sequenced time, and the newest one after it.
fn sets(engine: &Engine) -> (Option<SetView>, Option<SetView>) {
    let view = |(set, recorded): (&u32, &engine_core::turns::RotationSet)| SetView {
        set: *set,
        epoch: recorded.epoch,
        effective_at: recorded.effective_at,
        logged_at: recorded.logged_at,
        members: recorded.members.clone(),
    };
    let sets = &engine.daily_turns().sets;
    let now = engine.now();
    let in_effect = sets.iter().rev().find(|(_, recorded)| recorded.effective_at <= now).map(view);
    let next = sets.iter().rev().find(|(_, recorded)| recorded.effective_at > now).map(view);
    (in_effect, next)
}
