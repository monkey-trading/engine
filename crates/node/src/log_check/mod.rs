//! The boundary's log check (#594). While rotation is on and a daily turn is in effect whose rotation
//! set names two or more members, the enclave signs a withdrawal only for an approval at or below the
//! head of the log the last check verified, #14's posted root still applying; a one-member set,
//! rotation off, or no turn in effect, leaves #14's gate alone. A check is another member's
//! verification of the log through a checkpoint, `(seq, link, root)`:
//! - at a handover, the successor's own: its replica, or its start's replay, verified every entry
//!   through the handed-over checkpoint, recorded as its start re-seals the keys
//!   (`node::keys::check_predecessor_head`);
//! - at a boundary the operator holds on through, an attested verification from a following member:
//!   the operator's node asks the members of the turn's order, first to last (`ask`), and a member
//!   answers from the checkpoints its replica verified (`answer`).
//!
//! A check counts only from a release that sequenced none of the covered range, the commands after
//! the last verified head, as the log's `TurnTaken` commands record (`Takings`). The verified head is
//! sealed with the keys, so a restart restores it once the recovered log holds that checkpoint, and an
//! approved upgrade carries it to the next release; a host that restores an older copy only delays
//! payouts. The check bounds how long a withheld branch can pay nothing: a day plus the switch. It does
//! not bound delay or censorship.

pub(crate) mod answer;
pub(crate) mod ask;

use std::collections::{BTreeMap, BTreeSet};

use engine_core::Engine;
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::types::{AccountId, Millis};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::handover::Evidence;
use crate::log::CheckpointDomain;

/// A checkpoint of the log a check verified: its seq, the cumulative command prefix after its
/// command, and its state root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifiedHead {
    pub seq: u64,
    pub link: B256,
    pub root: B256,
}

/// A check the operator's node waits for: the turn in effect it holds on through, the checkpoint to
/// verify, at or after its `TurnTaken`, and the members and releases that sequenced the covered range,
/// which no check may come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Due {
    pub turn: u64,
    pub position: u16,
    pub set: u32,
    /// The turn's member, whose node asks and is never asked.
    pub candidate: AccountId,
    pub head: VerifiedHead,
    pub excluded: Sequencers,
}

/// The candidates that sequenced a range of the log, by the turns they took, and the releases they
/// took them under. A check counts only from a member in neither: a candidate's other release, as
/// after an upgrade, is the same member.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Sequencers {
    pub candidates: BTreeSet<AccountId>,
    pub releases: BTreeSet<B256>,
}

impl Sequencers {
    /// Whether `candidate`, or `release`, sequenced part of the range.
    pub(crate) fn include(&self, candidate: AccountId, release: B256) -> bool {
        self.candidates.contains(&candidate) || self.releases.contains(&release)
    }
}

/// A `TurnTaken` the engine accepted: the candidate whose turn it took, and the release it took it
/// under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Taking {
    candidate: AccountId,
    release: B256,
}

/// What a member's verification states, inside its quote: the boundary, the turn assignment and the
/// vault's signer epoch it was asked under, and the checkpoint its replica verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Statement {
    pub signer_epoch: u64,
    pub boundary: Millis,
    pub turn: u64,
    pub position: u16,
    pub set: u32,
    pub head: VerifiedHead,
}

/// The operator's question to a member: a fresh challenge, hex, the statement to verify, and the
/// operator enclave's signature over both (`ask_digest`), which the member checks against the signer
/// of the checkpoint it verified before it quotes anything.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ask {
    pub nonce: String,
    pub statement: Statement,
    pub signature: String,
}

/// A member's answer.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Answer {
    /// Its quote over the statement, the challenge and its config hash (`report_data`).
    Verified { config_sha256: String, evidence: Evidence },
    /// Why its replica does not verify the statement.
    Refused { reason: String },
}

/// The report data a member's quote binds: the statement, the operator's challenge, and the config it
/// runs with, for this deployment.
pub(crate) fn report_data(
    domain: CheckpointDomain,
    nonce: &[u8; 32],
    statement: &Statement,
    config_sha256: &[u8; 32],
) -> [u8; 64] {
    let mut hash = statement_hash(b"engine.log-check.v1", domain, nonce, statement);
    hash.update(config_sha256);
    crate::handover::padded(hash.finalize().into())
}

/// The digest the operator's enclave key signs over its question: the challenge and the statement,
/// for this deployment.
pub(crate) fn ask_digest(domain: CheckpointDomain, nonce: &[u8; 32], statement: &Statement) -> B256 {
    B256::from(<[u8; 32]>::from(statement_hash(b"engine.log-check-ask.v1", domain, nonce, statement).finalize()))
}

/// A hash under `tag` of the deployment, the challenge and every field of the statement.
fn statement_hash(tag: &[u8], domain: CheckpointDomain, nonce: &[u8; 32], statement: &Statement) -> Sha256 {
    let mut hash = Sha256::new();
    hash.update(tag);
    hash.update(domain.chain_id.to_be_bytes());
    hash.update(domain.vault.0);
    hash.update(nonce);
    hash.update(statement.signer_epoch.to_be_bytes());
    hash.update(statement.boundary.to_be_bytes());
    hash.update(statement.turn.to_be_bytes());
    hash.update(statement.position.to_be_bytes());
    hash.update(statement.set.to_be_bytes());
    hash.update(statement.head.seq.to_be_bytes());
    hash.update(statement.head.link);
    hash.update(statement.head.root);
    hash
}

/// Whether the gate applies to `engine`'s log: rotation is on and a turn is in effect whose set names
/// two or more members. A set the log no longer keeps counts as two.
pub(crate) fn applies(engine: &Engine) -> bool {
    let turns = engine.daily_turns();
    engine.operator_election().rotation_enabled
        && turns.in_effect.is_some_and(|turn| turns.sets.get(&turn.set).is_none_or(|set| set.members.len() >= 2))
}

/// The log's `TurnTaken` commands: the candidate and release each names, by the seq it was sequenced
/// at, from the first command the replay that collected them applied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Takings {
    by_seq: BTreeMap<u64, Taking>,
    /// The first command seen: zero for a replay from genesis, past a snapshot's base for one from it.
    from: Option<u64>,
}

impl Takings {
    /// Notes the command at `seq` with the `events` it gave: a `TurnTaken` the engine accepted
    /// records its candidate and release.
    pub(crate) fn command(&mut self, seq: u64, events: &[Event]) {
        self.from.get_or_insert(seq);
        let taken = events.iter().find_map(|event| match event {
            Event::TurnTaken { candidate, release, .. } => Some(Taking { candidate: *candidate, release: *release }),
            _ => None,
        });
        if let Some(taking) = taken {
            self.by_seq.insert(seq, taking);
        }
    }

    /// Takes the records as complete from `next`, the next command's seq, when no command was seen:
    /// a log with none yet, or a start whose snapshot covers every command.
    pub(crate) fn complete_from(mut self, next: u64) -> Self {
        self.from.get_or_insert(next);
        self
    }

    /// The latest `TurnTaken`'s seq and release.
    pub(crate) fn last(&self) -> Option<(u64, B256)> {
        self.by_seq.last_key_value().map(|(seq, taking)| (*seq, taking.release))
    }

    /// Who sequenced the commands after `after` through `through`, as `TurnTaken` records them: the
    /// latest one at or before the first of them, whose member sequenced on from there, and each one
    /// after it. Commands before the log's first `TurnTaken` name no one. `None` when these records do
    /// not reach back to the first of them, or, from a snapshot's base, to a `TurnTaken` at or before
    /// it, since a `TurnTaken` before the base would name who sequenced them.
    pub(crate) fn sequenced(&self, after: Option<u64>, through: u64) -> Option<Sequencers> {
        let first = after.map_or(0, |after| after.saturating_add(1));
        let from = self.from?;
        let in_office = self.by_seq.range(..=first).next_back().map(|(_, taking)| *taking);
        if from > first || (from > 0 && in_office.is_none()) {
            return None;
        }
        let takings = in_office.into_iter().chain(self.by_seq.range(first..=through).map(|(_, taking)| *taking));
        Some(takings.fold(Sequencers::default(), |mut sequencers, taking| {
            sequencers.candidates.insert(taking.candidate);
            sequencers.releases.insert(taking.release);
            sequencers
        }))
    }

    /// Drops the records no range after `verified` needs: those before the latest at or before the
    /// command after it, from which the records then attribute ranges.
    pub(crate) fn prune(&mut self, verified: u64) {
        let first = verified.saturating_add(1);
        if let Some((&keep, _)) = self.by_seq.range(..=first).next_back() {
            self.by_seq = self.by_seq.split_off(&keep);
            self.from = self.from.max(Some(keep));
        }
    }
}

/// The verified head after this node's own verification of the log through `head`, at a handover or
/// a promoted member's resume: `head` when neither `release` nor any of `candidates`, the candidates
/// this node's operator key sits for (`own_candidates`), sequenced the commands after `previous`
/// through it, as `takings` records; otherwise `previous`, and a following member's check is asked
/// for as at a boundary without a handover. So the same member's other release, as after an upgrade,
/// never counts as the check, however its start was made.
pub(crate) fn own_check(
    previous: Option<VerifiedHead>,
    takings: &Takings,
    head: VerifiedHead,
    (release, candidates): (B256, &BTreeSet<AccountId>),
) -> Option<VerifiedHead> {
    if previous.is_some_and(|previous| previous.seq >= head.seq) {
        return previous;
    }
    let sequenced = takings.sequenced(previous.map(|previous| previous.seq), head.seq);
    let independent = |sequencers: &Sequencers| {
        !sequencers.releases.contains(&release) && sequencers.candidates.is_disjoint(candidates)
    };
    match sequenced {
        Some(sequencers) if independent(&sequencers) => Some(head),
        _ => previous,
    }
}

/// The candidates whose admin key in `engine`'s rotation sets and turns is `operator`: the member a
/// node running under that operator key is.
pub(crate) fn own_candidates(engine: &Engine, operator: AccountId) -> BTreeSet<AccountId> {
    let turns = engine.daily_turns();
    let members =
        turns.sets.values().flat_map(|set| set.members.iter().map(|member| (member.admin_key, member.candidate)));
    let assigned = turns.assigned.values().chain(turns.in_effect.iter()).map(|turn| (turn.admin_key, turn.candidate));
    members.chain(assigned).filter(|(admin_key, _)| *admin_key == operator).map(|(_, candidate)| candidate).collect()
}

#[cfg(test)]
mod tests;
