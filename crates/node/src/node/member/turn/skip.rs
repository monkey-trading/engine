//! A member's turn under a skip (#596). The finalised schedule shows a skip only a finality period
//! after it lands, so with a reader of the latest agreed block (`handover::assigned`) a member:
//! - takes a turn the latest block's skip names it for, from the operator holding the keys
//!   (`handover::discovery`), binding the skip's assignment and sealing the keys in its generation;
//! - does not take a turn it was skipped from, unless it already holds its keys, when it starts as
//!   their holder and hands them to the member now named, as it does with keys it took under an
//!   earlier assignment of the turn that a later skip moved past;
//! - takes back a drawn holding it handed to a skip's member once a reorg removes that skip;
//! - follows the operator whose release signed the newest head when the one the registry names fails
//!   a stint, as while a skipped member never comes up the incumbent still serves.
//!
//! A read that fails leaves the finalised schedule to decide, as without a reader: the holder admits
//! only the member its own read names.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, ensure};
use tracing::{debug, info};

use crate::checkpoints::{CheckpointChain, HandoverSource, ScheduledTurn, View};
use crate::config::NodeConfig;
use crate::handover::assigned::{Assigned, Assignments, Bound, Reason};
use crate::handover::tail::Prepared;
use crate::node::member::turn::Holding;
use crate::node::member::{Member, PostedHead, Sampling};
use crate::standby::Followed;
use crate::tee::sealing::file_for;

/// What a member reads a turn's reassignments through, and whom it follows past them.
#[derive(Default)]
pub(crate) struct Fallback {
    /// The turn's assignment at the latest agreed block, with `[log_heads]`.
    pub assignments: Option<Arc<dyn Assignments>>,
    /// Whether it follows the operator whose release signed the newest head, rather than the one the
    /// registry names: each failed stint moves it to the other.
    pub follow_holder: bool,
    /// Set by a confirmed handover until the next read records the assignment then current.
    pub handing: bool,
    /// The latest assignment when its last confirmed handover gave the keys away: the successor's.
    pub handed_to: Option<Assigned>,
}

impl Fallback {
    /// `cfg`'s reader of the latest agreed block (`assigned::for_config`), following the operator the
    /// registry names first.
    /// Error cases:
    /// - the custody section or the vault address is malformed
    pub(crate) fn for_config(cfg: &NodeConfig) -> anyhow::Result<Self> {
        let assignments = crate::handover::assigned::for_config(cfg, "member_assignments")?;
        Ok(Self { assignments, ..Self::default() })
    }
}

impl<C: CheckpointChain, H: PostedHead, T: Sampling> Member<'_, C, H, T> {
    /// The latest agreed block's assignment of the turn in effect, `None` without a reader, while it
    /// names no turn, or when the read fails.
    async fn latest(&self) -> Option<Assigned> {
        let read = self.fallback.assignments.as_ref()?;
        match read.assigned().await {
            Ok(assigned) => assigned,
            Err(e) => {
                debug!("reading the turn's latest assignment failed: {e:#}");
                None
            }
        }
    }

    /// What this member does when the latest agreed block's skip or promotion moved `finalised`, the
    /// turn in effect at trusted time, past what the finalised schedule shows: it takes the turn a skip
    /// names it for, and follows rather than takes one a reassignment moved past it; keys it already
    /// holds for either, or for an earlier assignment of the turn a later skip moved past, it holds. A
    /// promotion naming it is its standby side's to start. A drawn holding it handed to a skip's member
    /// it takes back once a reorg removes the skip. `None` when no reassignment of that turn says
    /// otherwise.
    pub(super) async fn skipped(&mut self, finalised: &ScheduledTurn) -> Option<Holding> {
        let cfg = self.cfg;
        let dir = &cfg.tee.sealing.as_ref()?.dir;
        let latest = self.latest().await;
        if std::mem::take(&mut self.fallback.handing) {
            self.fallback.handed_to = latest;
        }
        let assigned = latest.filter(|assigned| assigned.binding.turn == finalised.turn)?;
        let own = self.release.id();
        // The successor this member handed the keys to was a skip's member that this assignment no longer is.
        let restored =
            self.fallback.handed_to.is_some_and(|to| to.reason == Reason::Skip && to.binding != assigned.binding);
        let given = |generation| self.handed.is_some_and(|handed| handed.generation == Some(generation));
        if assigned.reason == Reason::Draw {
            let generation = assigned.binding.generation;
            let back = restored
                && assigned.release == own
                && given(generation)
                && !file_for(dir, &self.release, generation).exists();
            return back.then(|| {
                info!(turn = finalised.turn, "the skip this member handed its turn to is gone: taking the turn back");
                Holding::Take(finalised.clone())
            });
        }
        if assigned.release == own && assigned.reason == Reason::Skip {
            let turn = assigned.as_scheduled(finalised);
            return Some(match given(turn.generation) && !restored {
                true => Holding::Follow(Some(turn)),
                false if file_for(dir, &self.release, turn.generation).exists() => Holding::Hold,
                false => {
                    info!(
                        turn = turn.turn,
                        position = assigned.binding.position,
                        "a skip names this member: taking the turn"
                    );
                    Holding::Take(turn)
                }
            });
        }
        // Keys it took under an earlier assignment of the turn that a later skip moved past: it holds,
        // starts and hands them to the member now named. Past a promotion its key is fenced out.
        if assigned.release != own && assigned.reason == Reason::Skip && seals_any(dir, &self.release) {
            let read = self.fallback.assignments.as_ref()?;
            let superseded = read.superseded(&assigned, own).await.ok().flatten();
            if superseded.is_some_and(|held| file_for(dir, &self.release, held.binding.generation).exists()) {
                info!(turn = finalised.turn, "a later skip moved past the keys this member holds: handing them on");
                return Some(Holding::Hold);
            }
        }
        // Moved past in a turn it holds no keys for: it follows. Keys sealed for the turn make it the
        // holder, which starts and hands them to the member now named.
        let skipped_from = assigned.release != own
            && finalised.release == own
            && !file_for(dir, &self.release, finalised.generation).exists();
        skipped_from.then(|| {
            info!(turn = finalised.turn, "a skip passed this member before it took the turn: following");
            Holding::Follow(Some(finalised.clone()))
        })
    }

    /// The operators `turn` takes its keys from, and what the tail handover reads before it connects:
    /// with a reader, the operator holding them first (`discovery::sources`) and a binding to the
    /// latest assignment, a skip's generation included; the finalised schedule's sources and
    /// generation otherwise.
    /// Error cases:
    /// - the vault, the schedule or the latest block cannot be read, or the latest assignment names
    ///   another holding than `turn`'s
    pub(super) async fn prepare_take(&self, turn: &ScheduledTurn) -> anyhow::Result<(Vec<HandoverSource>, Prepared)> {
        let at = self.vault.observe(View::Finalized, &[]).await?;
        let latest = match &self.fallback.assignments {
            Some(read) => read.assigned().await.context("reading the turn's latest assignment")?.map(|a| (read, a)),
            None => None,
        };
        let Some((read, assigned)) = latest else {
            let sources = crate::handover::turns::sources(&self.vault, &at, turn).await?;
            return Ok((sources, Prepared::read(&self.vault, &self.release, Some(turn.boundary), None).await?));
        };
        if assigned.binding.turn < turn.turn {
            return Err(anyhow::Error::new(AssignmentBehind(turn.turn)));
        }
        ensure!(
            assigned.binding.turn == turn.turn && assigned.names(turn.release, turn.generation),
            "the latest agreed block's assignment {:?} of release 0x{} is not turn {}'s holding of this release",
            assigned.binding,
            hex::encode(assigned.release),
            turn.turn
        );
        let prepared = match assigned.reason {
            Reason::Skip => Prepared::assigned(&self.vault, &assigned, read.clone()).await?,
            Reason::Draw => {
                let mut prepared = Prepared::read(&self.vault, &self.release, Some(turn.boundary), None).await?;
                ensure!(prepared.generation == turn.generation, "the finalised schedule's generation moved");
                prepared.bound = Bound::of(&assigned, read.clone());
                prepared
            }
            Reason::Promotion => anyhow::bail!("a promotion's member takes no keys by handover"),
        };
        let members = read.members(&assigned).await?;
        let sources = crate::handover::discovery::sources(&self.vault, &at, turn, &assigned, &members).await?;
        Ok((sources, prepared))
    }

    /// The operator to follow under `turn`, the turn in effect: the one the registry names, or, every
    /// other time a stint fails, the one whose release signed the newest head, when the latest agreed
    /// block shows one among the turn's set and earlier holders.
    /// Error cases:
    /// - the operator the registry names cannot be resolved (see `standby::resolve`)
    pub(super) async fn resolve_following(&self, turn: Option<&ScheduledTurn>) -> anyhow::Result<Followed> {
        let named = crate::standby::resolve(self.cfg, &self.vault).await?;
        if !self.fallback.follow_holder {
            return Ok(named);
        }
        let Some(holder) = self.holder(turn).await else { return Ok(named) };
        info!(holder = %holder.candidate, operator = %holder.api_hostname, "following the operator whose head is newest");
        Ok(Followed { origin: format!("https://{}", holder.api_hostname), ..named })
    }

    /// The operator whose release signed the newest head, among the latest set of the turn in effect
    /// and `turn`'s source and earlier holders; `None` without a reader, a head, or a match.
    async fn holder(&self, turn: Option<&ScheduledTurn>) -> Option<HandoverSource> {
        let (read, assigned) = (self.fallback.assignments.as_ref()?, self.latest().await?);
        let members = read.members(&assigned).await.ok()?;
        let earlier = match turn {
            Some(turn) => {
                let at = self.vault.observe(View::Finalized, &[]).await.ok()?;
                crate::handover::turns::sources(&self.vault, &at, turn).await.ok()?
            }
            None => Vec::new(),
        };
        let holder = crate::handover::discovery::holder(assigned.head_release, &members, &earlier).cloned();
        holder.filter(|holder| !holder.api_hostname.is_empty())
    }
}

/// The latest agreed block has not reached the boundary of the turn a take is for, as through a chain
/// outage across it: the member keeps its copy and waits, as for an unfinal signer epoch.
#[derive(Debug)]
pub(in crate::node::member) struct AssignmentBehind(pub u64);

impl std::fmt::Display for AssignmentBehind {
    /// What happened.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "the latest agreed block has not reached turn {}'s boundary", self.0)
    }
}

impl std::error::Error for AssignmentBehind {}

/// Whether a failed take waits on the latest agreed block reaching its turn (`AssignmentBehind`).
pub(super) fn behind(e: &anyhow::Error) -> bool {
    e.downcast_ref::<AssignmentBehind>().is_some()
}

/// Whether `dir` holds keys sealed for `release` in any generation.
fn seals_any(dir: &Path, release: &crate::tee::sealing::Release) -> bool {
    let prefix = format!("{}-", hex::encode(release.id()));
    let names = std::fs::read_dir(dir).into_iter().flatten().flatten();
    names.into_iter().any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
}
