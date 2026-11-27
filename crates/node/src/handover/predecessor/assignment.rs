//! The holder's assignment checks (#596). With a reader of the latest agreed block, a holder admits a
//! successor only while the turn in effect's current assignment there is the one its request binds and
//! names its release in that generation: a skip names the next member at sequencer confirmation, a
//! finality period before the finalised schedule does. A drawn assignment still passes the finalised
//! checks it always has, and so does a promotion's, whose member holds its own standby key and hands
//! it only to its own approved release, and a skip a release move revised, its member's own upgrade,
//! which so waits for the log's mirror of the skip, and the log check counts the member's commands as
//! its own. A skip's member is admitted once this release's log shows the skip's set naming its
//! candidate with the adopted admin key under V51's notice rule (`DailyTurns::set_refusal`), without
//! waiting for the log's own mirror of the skip, which comes only at finality; once sequencing has
//! stopped, the turns retained at the stop answer, for turns up to `turns::SOURCE_LOOKBACK` past the
//! last one they know. The holder reads the assignment again just before its stop and, once stopped,
//! before the keys go: one that moved since the admission refuses that successor, and the stopped
//! holder goes on serving the member now named. With no turn naming the operator at the latest agreed
//! block, or no reader, the finalised schedule's turn or the vault's own release, which the checkpoint
//! endpoint reads, decide only once the custody quorum reads the same at the same block
//! (`quorum::confirmed_generation`).
//!
//! Every check above reads public values, which an enclave that runs the member's release on another
//! host satisfies as well, so a turn's member is also held to its own enclave: the request must carry
//! the member's signature, with the standby key the schedule lists as its candidate's promotable key at
//! the latest agreed block, over the request's report data (`ensure_member`). A member with no
//! promotable key, as until its registration lands, or after its release move or a promotion voids the
//! key, is admitted by its release and assignment alone.

use anyhow::{Context, bail, ensure};
use engine_core::eip712::{self, B256, Signature};
use engine_core::turns::{DailyTurns, TURN_MS};
use engine_core::types::{AccountId, Millis};
use tracing::{info, warn};

use crate::checkpoints::{CheckpointChain, Observation};
use crate::handover::assigned::{Assigned, Binding, Reason};
use crate::handover::quorum::confirmed_generation;
use crate::handover::turns::{SOURCE_LOOKBACK, named_generation};
use crate::handover::{Predecessor, Request, member_digest};
use crate::log::CheckpointDomain;
use crate::tee::sealing::Release;

impl<C: CheckpointChain> Predecessor<C> {
    /// Whether this release may hand its keys to a successor running `release` whose `request`, with
    /// `report_data`, binds its assignment, by the latest agreed block's assignment when this listener
    /// reads one, and by the finalised schedule at `at` and trusted time as before otherwise, which the
    /// custody quorum must read as the checkpoint endpoint does (`confirmed_generation`), with the turn's
    /// member's standby signature when this listener reads the latest block (see the module).
    /// Error cases:
    /// - the latest assignment cannot be read, is not the one the request binds, or names another release
    /// - the member's promotable standby key cannot be read, or did not sign the request (`ensure_member`)
    /// - a draw, a promotion or a revised skip fails the finalised checks (`named_generation`,
    ///   `ensure_logged`), or another skip the log's notice rule (`ensure_noticed`)
    /// - with no latest assignment, the finalised checks fail, or the custody quorum reads otherwise
    pub(super) async fn admit_turn(
        &self,
        (request, report_data): (&Request, &[u8; 64]),
        release: &Release,
        at: &Observation,
    ) -> anyhow::Result<()> {
        let reached = self.trusted.reached_ms();
        let latest = match &self.assignments {
            Some(assignments) => assignments.assigned().await.context("reading the turn's latest assignment")?,
            None => None,
        };
        let Some(assigned) = latest else {
            let (_, turn) = confirmed_generation(&self.vault, self.quorum.as_ref(), at, release, reached).await?;
            if let Some(turn) = turn {
                if let Some(assignments) = &self.assignments {
                    let read = assignments.standby(turn.candidate).await;
                    let standby = read.context("reading the turn's member's standby key")?;
                    ensure_member(self.domain, (turn.turn, turn.candidate, standby), request, report_data)?;
                }
                self.ensure_logged(&turn).await?;
            }
            return Ok(());
        };
        ensure_bound(&assigned, request.binding.as_ref(), release)?;
        let member = (assigned.binding.turn, assigned.candidate, assigned.standby);
        ensure_member(self.domain, member, request, report_data)?;
        match assigned.reason {
            Reason::Skip if !assigned.revised => self.ensure_noticed(&assigned, reached).await,
            // A promotion's member holds its standby key, the vault's signer, so the only keys it takes by a
            // handover are its own, moving to an approved release, as a skip's member's upgrade does: these
            // pass the finalised checks a draw does, the log having mirrored the assignment, so the log
            // check counts the member's own commands as its own, not as another's to check.
            Reason::Skip | Reason::Draw | Reason::Promotion => {
                let (generation, turn) = named_generation(&self.vault, at, release, reached).await?;
                let turn = turn.context("trusted time has not reached a turn the finalised schedule names")?;
                ensure!(
                    turn.turn == assigned.binding.turn && generation == assigned.binding.generation,
                    "the finalised schedule names turn {} in generation {}, but the latest agreed block's assignment \
                     is turn {}'s in generation {}",
                    turn.turn,
                    generation,
                    assigned.binding.turn,
                    assigned.binding.generation
                );
                self.ensure_logged(&turn).await
            }
        }
    }

    /// Reads the latest assignment again just before the stop and, once stopped, before the keys go: a
    /// skip or a promotion that landed since the admission refuses this successor, so a release still
    /// sequencing goes on, and a stopped one admits the member now named instead. Returns the assignment
    /// read, the one `binding` names; `None` without a reader, or while no turn names the operator and
    /// the request binds none.
    /// Error cases:
    /// - the read fails, or the assignment is no longer the one `binding` names for `release`
    pub(super) async fn revalidate(
        &self,
        binding: Option<&Binding>,
        release: &Release,
    ) -> anyhow::Result<Option<Assigned>> {
        let Some(assignments) = &self.assignments else { return Ok(None) };
        let assigned =
            assignments.assigned().await.context("reading the turn's latest assignment before the keys go")?;
        let moved = || "the turn's assignment moved since the admission: this release serves the member now named";
        match assigned {
            Some(assigned) => ensure_bound(&assigned, binding, release).context(moved()).map(|()| Some(assigned)),
            None => {
                ensure!(binding.is_none(), "{}: the latest agreed block names no turn", moved());
                Ok(None)
            }
        }
    }

    /// Refuses a skip's member unless this release's log shows its set naming its candidate with the
    /// adopted admin key its notice before it applies (`SET_NOTICE_MS`), at the later of its boundary
    /// and trusted time `reached`, and its turn has not ended: from the sequencer while it runs, and
    /// once it has stopped from the turns retained at the stop, for a turn at most `SOURCE_LOOKBACK`
    /// past the last they know.
    /// Error cases:
    /// - the turn has ended, or the log refuses the set, candidate or key, or the listener reads no
    ///   log, or has stopped with no turns retained or the turn past the lookback
    pub(super) async fn ensure_noticed(&self, assigned: &Assigned, reached: Option<Millis>) -> anyhow::Result<()> {
        let Assigned { set, candidate, admin_key, boundary, .. } = *assigned;
        let turn = assigned.binding.turn;
        let reached = reached.unwrap_or(boundary);
        ensure!(reached < boundary.saturating_add(TURN_MS), "turn {turn} has ended: its skip names no holding now");
        let applies = boundary.max(reached);
        let refusal = match self
            .query(move |engine| engine.daily_turns().set_refusal(set, candidate, admin_key, applies))
            .await
        {
            Ok(refusal) => refusal,
            Err(e) => {
                let retained = self.retained.lock().expect("the retained turns lock");
                let retained = retained.as_ref().ok_or(e)?;
                ensure!(
                    within_lookback(retained, turn),
                    "turn {turn} is more than {SOURCE_LOOKBACK} turns past the last this release's stopped log knows"
                );
                retained.set_refusal(set, candidate, admin_key, applies)
            }
        };
        if let Some(reason) = refusal {
            bail!(
                "this release's log refuses turn {turn}'s skip to candidate {candidate} with key {admin_key}: {reason}"
            );
        }
        Ok(())
    }
}

/// Refuses a request for turn `turn`'s keys unless `candidate`'s promotable standby key `standby` signed
/// its `report_data` (`member_digest`): the key is sealed in the member's own enclave and registered by
/// its candidate, so an enclave that runs the member's release on another host cannot sign. A member
/// with no promotable key is admitted by its release and assignment alone, which is logged.
/// Error cases:
/// - the member has a promotable standby key, and the request carries no signature, one that does not
///   parse, or another key's
fn ensure_member(
    domain: CheckpointDomain,
    (turn, candidate, standby): (u64, AccountId, Option<AccountId>),
    request: &Request,
    report_data: &[u8; 64],
) -> anyhow::Result<()> {
    let Some(standby) = standby else {
        warn!(
            turn,
            %candidate,
            "the turn's member has no promotable standby key: admitting it by its release and assignment alone"
        );
        return Ok(());
    };
    let signature = request.standby_signature.as_deref().with_context(|| {
        format!("turn {turn}'s member {candidate} holds the standby key {standby}, which did not sign the request")
    })?;
    let signature: Signature = signature.parse().context("the request's standby signature is not a signature")?;
    let signer = eip712::recover(&B256::from(member_digest(domain, report_data)), &signature);
    ensure!(
        signer == Some(standby),
        "the request is signed by {signer:?}, not by turn {turn}'s member {candidate}'s standby key {standby}"
    );
    info!(turn, %candidate, %standby, "the turn's member signed the request with its standby key");
    Ok(())
}

/// Refuses `assigned` unless it is the assignment `binding` names and names `release`.
/// Error cases:
/// - the request binds no assignment or another one, or the assignment names another release
fn ensure_bound(assigned: &Assigned, binding: Option<&Binding>, release: &Release) -> anyhow::Result<()> {
    ensure!(
        binding == Some(&assigned.binding),
        "the successor binds {binding:?}, but the latest agreed block holds {:?}",
        assigned.binding
    );
    ensure!(
        assigned.release == release.id(),
        "turn {}'s current assignment names release 0x{}, not the successor's 0x{}",
        assigned.binding.turn,
        hex::encode(assigned.release),
        hex::encode(release.id())
    );
    Ok(())
}

/// Whether `turn` is at most `SOURCE_LOOKBACK` turns past the last turn `retained` knows, in effect or
/// waiting: a member looks back that far for the holder of its keys, and no further.
pub(super) fn within_lookback(retained: &DailyTurns, turn: u64) -> bool {
    let last = retained.assigned.keys().copied().chain(retained.in_effect.map(|in_effect| in_effect.turn)).max();
    last.is_some_and(|last| turn <= last.saturating_add(SOURCE_LOOKBACK as u64))
}
