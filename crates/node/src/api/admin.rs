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
use crate::config::is_published_operator;

/// Sequences an admin command the operator signed and returns the events it produced, ending in
/// `admin_applied` or `admin_rejected`. A request stays out of the log unless the operator may send
/// its command, it carries the signature of the operator, or of a pending rotation's key once it is
/// due by the node's clock (see `Engine::due_rotation`), its nonce is the operator's next, and it has
/// not expired, so a copy sent after the original was sequenced stays out of the log. Copies that
/// race the original all pass these checks and are sequenced; the engine, which checks all four
/// again at the sequenced time, applies one and refuses the rest for their nonce.
/// Error cases:
/// - a command only the node sequences: client auth, the operator, or a protocol activation
/// - a signature that is neither the recorded operator's nor a pending rotation's key over this
///   command, nonce, and expiry
/// - with a real enclave, a rotation to a published test key, or a request signed by a published
///   test key other than the configured one, as an election may name
/// - an emission schedule whose period budget is not the one the configured vault's MONKEY cap was
///   derived from
/// - a nonce other than the operator's next
/// - an expiry already past
/// - the sequencer has stopped
pub(super) async fn admin(State(s): State<ApiState>, Json(req): Json<AdminRequest>) -> ApiResult<Vec<Event>> {
    let mut seen = None;
    let answered = sequence_admin(&s, req, &mut seen).await;
    // A refusal states the operator and nonce the engine had taken in, which a crash could take back.
    if answered.is_err() {
        s.durable(seen).await?;
    }
    answered
}

/// `admin`, noting in `seen` the last seq of the state its refusals read.
/// Error cases:
/// - as `admin`
async fn sequence_admin(s: &ApiState, req: AdminRequest, seen: &mut Option<u64>) -> ApiResult<Vec<Event>> {
    if !req.command.is_operator_command() {
        debug!("admin request refused: not an operator command");
        return Err(ApiError::BadRequest("the operator may not send this command".into()));
    }
    // The key the log records, and the one a pending rotation or a daily turn hands over to: the
    // engine decides at the sequenced time which of them applies. The pending key passes only once it is due by the
    // node's clock, an elected one once the staking mirror has also reached the registry's time, so
    // no request the engine cannot yet authorise enters the log, where it could be copied and sent
    // again after the handover.
    let now = s.clock.now_ms();
    let (recorded, due, last_seq) = s.query(move |e| (e.operator().copied(), e.due_key(now), e.last_seq())).await?;
    *seen = last_seq;
    let current = recorded.unwrap_or(s.operator);
    let next = due.map(|rotation| Operator { address: rotation.address, ..current });
    let Some(operator) =
        [Some(current), next].into_iter().flatten().find(|signer| signer.check(&req.command, &req.auth).is_ok())
    else {
        debug!("admin request refused: invalid signature");
        return Err(ApiError::Unauthorized("invalid signature"));
    };
    Span::current().record("id", display(operator.address));
    // An election can name any key, and a real enclave takes no admin command from a published one;
    // the configured key is checked at config load.
    if operator.address != s.operator.address
        && s.attestation.provider != "mock"
        && is_published_operator(operator.address).map_err(|_| ApiError::Internal)?
    {
        debug!("admin request refused: signed by a published test key");
        return Err(ApiError::Unauthorized("signed by a published test key"));
    }
    // The vault's real attestation means a real enclave, whose operator key must not be a published one.
    if let AdminCommand::RotateOperator { address } = &req.command
        && s.attestation.provider != "mock"
        && is_published_operator(*address).map_err(|_| ApiError::Internal)?
    {
        debug!("admin request refused: rotation to a published test key");
        return Err(ApiError::BadRequest("the operator may not rotate to a published test key".into()));
    }
    if let (AdminCommand::InitEmissions(schedule), Some(vault)) = (&req.command, s.emission_budget)
        && let Err(reason) = check_budget(vault, schedule.period_budget)
    {
        debug!("admin request refused: {reason}");
        return Err(ApiError::BadRequest(reason));
    }
    let (next, last_sequenced, last_seq) =
        s.query(move |e| (e.next_request_nonce(operator.address), e.now(), e.last_seq())).await?;
    *seen = last_seq;
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
