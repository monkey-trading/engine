//! The operator's admin route: an admin command signed by the operator key the log records, or the
//! key a logged rotation hands over to, as `engine admin` posts it.

use axum::Json;
use axum::extract::State;
use engine_core::auth::Operator;
use engine_core::command::{AdminCommand, AdminRequest, Command};
use engine_core::event::Event;
use tracing::field::display;
use tracing::{Span, debug, info};

use crate::api::{ApiError, ApiResult, ApiState};
use crate::checkpoints::emission_budget::check_budget;

/// Sequences an admin command the operator signed and returns the events it produced, ending in
/// `admin_applied` or `admin_rejected`. A request stays out of the log unless the operator may send
/// its command, it carries the signature of the operator or of a pending rotation's key (which the
/// engine then judges at the sequenced time), its nonce is the operator's next, and it has
/// not expired, so a copy sent after the original was sequenced stays out of the log. Copies that
/// race the original all pass these checks and are sequenced; the engine, which checks all four
/// again at the sequenced time, applies one and refuses the rest for their nonce.
/// Error cases:
/// - a command only the node sequences: client auth, the operator, or a protocol activation
/// - a signature that is neither the recorded operator's nor a pending rotation's key over this
///   command, nonce, and expiry
/// - an emission schedule whose period budget is not the one the configured vault's MONKEY cap was
///   derived from
/// - a nonce other than the operator's next
/// - an expiry already past
/// - the sequencer has stopped
pub(super) async fn admin(State(s): State<ApiState>, Json(req): Json<AdminRequest>) -> ApiResult<Vec<Event>> {
    if !req.command.is_operator_command() {
        debug!("admin request refused: not an operator command");
        return Err(ApiError::BadRequest("the operator may not send this command".into()));
    }
    // The key the log records, and the one a pending rotation hands over to: the engine decides at
    // the sequenced time which of them applies.
    let (recorded, rotation) = s.query(|e| (e.operator().copied(), e.operator_bounds().rotation)).await?;
    let current = recorded.unwrap_or(s.operator);
    let next = rotation.map(|rotation| Operator { address: rotation.address, ..current });
    let Some(operator) =
        [Some(current), next].into_iter().flatten().find(|signer| signer.check(&req.command, &req.auth).is_ok())
    else {
        debug!("admin request refused: invalid signature");
        return Err(ApiError::Unauthorized("invalid signature"));
    };
    Span::current().record("id", display(operator.address));
    if let (AdminCommand::InitEmissions(schedule), Some(vault)) = (&req.command, s.emission_budget)
        && let Err(reason) = check_budget(vault, schedule.period_budget)
    {
        debug!("admin request refused: {reason}");
        return Err(ApiError::BadRequest(reason));
    }
    let (next, last_sequenced) = s.query(move |e| (e.next_request_nonce(operator.address), e.now())).await?;
    s.check_fresh(&req.auth, next, last_sequenced)?;
    // As for an account's request: only one the node would sequence spends the operator's rate.
    s.limits.check_account(operator.address)?;
    let nonce = req.auth.nonce;
    let events = s.submit(Command::SignedAdmin(Box::new(req))).await?;
    let rejected = events.iter().find_map(|event| match event {
        Event::AdminRejected { reason } => Some(reason),
        _ => None,
    });
    info!(nonce, ?rejected, "admin command sequenced");
    Ok(Json(events))
}
