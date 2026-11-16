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
//! the last verified head, as the log's `TurnTaken` commands and mirrored promotions record (`Takings`). The verified head is
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
use engine_core::turns::TurnReason;
use engine_core::types::{AccountId, Millis};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::checkpoints::Assigned;
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Sequencers {
    pub candidates: BTreeSet<AccountId>,
    pub releases: BTreeSet<B256>,
    /// The turn and candidate of the `TurnTaken` in office at the range's start: a member the schedule
    /// promoted or skipped to after that assignment may have sequenced part of the range before the log
    /// mirrored it, so a check counts from none of them (`reassigned`).
    pub in_office: (u64, AccountId),
}

impl Sequencers {
    /// Whether `candidate`, or `release`, sequenced part of the range.
    pub(crate) fn include(&self, candidate: AccountId, release: B256) -> bool {
        self.candidates.contains(&candidate) || self.releases.contains(&release)
    }

    /// The members of `assigned`, the schedule's assignments of the in-office turn and the turns after
    /// it, that a promotion or a skip made after the in-office holder's assignment: in a later turn, or
    /// in the in-office turn at a later position, since a turn's positions only advance. With no
    /// assignment of the holder's in its turn, every reassignment of that turn counts.
    pub(crate) fn reassigned(&self, assigned: &[Assigned]) -> BTreeSet<AccountId> {
        let (turn, holder) = self.in_office;
        let since =
            assigned.iter().find(|held| (held.turn, held.candidate) == (turn, holder)).map(|held| held.position);
        let after =
            |at: &&Assigned| at.turn > turn || (at.turn == turn && since.is_none_or(|since| at.position > since));
        assigned.iter().filter(|at| at.reassigned).filter(after).map(|at| at.candidate).collect()
    }
}

/// The schedule's assignments a start read before it connected, for its own check: those a promotion or
/// a skip made, in the turns from `from` on, other than the one that made the start. A check whose range
/// begins in an earlier turn cannot tell, and counts for nothing (`own_check_read`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reassigned {
    pub from: u64,
    pub assigned: Vec<Assigned>,
}

/// As `own_check`, and counted only when the schedule `read` also shows no promotion or skip to one of
/// `candidates`, this node's member, after the in-office assignment: its commands may precede the mirror
/// the log has yet to hold, as a promotion within the finality window leaves it. Without a `read`, as
/// for a start that read no schedule, the log's records alone decide.
pub(crate) fn own_check_read(
    previous: Option<VerifiedHead>,
    takings: &Takings,
    head: VerifiedHead,
    (release, candidates): (B256, &BTreeSet<AccountId>),
    read: Option<&Reassigned>,
) -> Option<VerifiedHead> {
    let checked = own_check(previous, takings, head, (release, candidates));
    let Some(read) = read.filter(|_| checked != previous) else { return checked };
    let sequenced = takings.sequenced(previous.map(|previous| previous.seq), head.seq);
    let unread = |sequencers: &Sequencers| {
        sequencers.in_office.0 < read.from || !sequencers.reassigned(&read.assigned).is_disjoint(candidates)
    };
    match sequenced {
        Some(sequencers) if !unread(&sequencers) => checked,
        _ => previous,
    }
}

/// A `TurnTaken` the engine accepted: the turn, the candidate whose turn it took, and the release it
/// took it under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Taking {
    turn: u64,
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
/// at, from the first command the replay that collected them applied; and the turn and candidate of
/// each promotion (#595) or skip (#596) the log mirrored, accepted or refused, by the seq of the mirror.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Takings {
    by_seq: BTreeMap<u64, Taking>,
    /// A promoted or skipped-to member sequences from its promotion or its take, before the finalised
    /// mirror of it and its own `TurnTaken`, which a failure before then never logs: each one's turn and
    /// candidate.
    promoted: BTreeMap<u64, BTreeSet<(u64, AccountId)>>,
    /// The latest turn that had begun by a command's stamp, at each seq where it moved: which turns had
    /// begun by the command at a range's end (`turn_begun_by`).
    began: BTreeMap<u64, u64>,
    /// The first command seen: zero for a replay from genesis, past a snapshot's base for one from it.
    from: Option<u64>,
}

impl Takings {
    /// Notes the command at `seq` with the `events` it gave: a `TurnTaken` the engine accepted
    /// records its turn, candidate and release, and a promotion's or a skip's assignment, accepted or
    /// refused, its turn and candidate.
    pub(crate) fn command(&mut self, seq: u64, events: &[Event]) {
        self.from.get_or_insert(seq);
        for event in events {
            if let Event::TurnAssigned { turn, reason: TurnReason::Promotion | TurnReason::Skip, candidate, .. }
            | Event::TurnRefused { turn, cause: TurnReason::Promotion | TurnReason::Skip, candidate, .. } = event
            {
                self.promoted.entry(seq).or_default().insert((*turn, *candidate));
            }
        }
        let taken = events.iter().find_map(|event| match event {
            Event::TurnTaken { turn, candidate, release } => {
                Some(Taking { turn: *turn, candidate: *candidate, release: *release })
            }
            _ => None,
        });
        if let Some(taking) = taken {
            self.by_seq.insert(seq, taking);
        }
    }

    /// Notes that the command at `seq`, which `command` notes the events of, was stamped `ts`.
    pub(crate) fn stamped(&mut self, seq: u64, ts: Millis) {
        if let Some(begun) = turn_begun_by(ts)
            && self.began.last_key_value().is_none_or(|(_, last)| *last != begun)
        {
            self.began.insert(seq, begun);
        }
    }

    /// The latest turn that had begun by the stamp of the command at `through`, as far as it is known.
    pub(crate) fn began_by(&self, through: u64) -> Option<u64> {
        self.began.range(..=through).next_back().map(|(_, turn)| *turn)
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

    /// Who sequenced the commands after `after`, the last verified head, through `through`, as
    /// `TurnTaken` records them: the latest one at or before the first of them, whose member sequenced
    /// on from there, and each one after it; with the candidate of each promotion or skip mirrored after
    /// that latest one, at any seq, in its turn or a later one that had begun by the command at `through`
    /// (when its stamp is known): the member's commands may precede its mirror, which finality can hold
    /// past `through`, and its own `TurnTaken`, if it ever logged one, while one in an earlier turn ended
    /// before that `TurnTaken`, and one in a turn that began after `through` sequenced none of the range. With no verified head, as before the
    /// first check or once a host kept the sealed one from a start, the range is the holding of the
    /// latest `TurnTaken` at or before `through`: from genesis it would name every member that ever
    /// took a turn, and no check could count again. `None` when no `TurnTaken` these records hold names
    /// who sequenced the first of them: the operator before the log's first `TurnTaken`, which no record
    /// names, or the holder before a snapshot's base.
    pub(crate) fn sequenced(&self, after: Option<u64>, through: u64) -> Option<Sequencers> {
        let first = match after {
            Some(after) => after.saturating_add(1),
            None => self.by_seq.range(..=through).next_back().map_or(0, |(seq, _)| *seq),
        };
        let from = self.from?;
        let (since, in_office) = self.by_seq.range(..=first).next_back().map(|(seq, taking)| (*seq, *taking))?;
        if from > first {
            return None;
        }
        let takings = std::iter::once(in_office).chain(self.by_seq.range(first..=through).map(|(_, taking)| *taking));
        let empty = Sequencers {
            candidates: BTreeSet::new(),
            releases: BTreeSet::new(),
            in_office: (in_office.turn, in_office.candidate),
        };
        let mut sequencers = takings.fold(empty, |mut sequencers, taking| {
            sequencers.candidates.insert(taking.candidate);
            sequencers.releases.insert(taking.release);
            sequencers
        });
        let after_in_office = (std::ops::Bound::Excluded(since), std::ops::Bound::Unbounded);
        let promoted = self.promoted.range(after_in_office).flat_map(|(_, promoted)| promoted);
        let begun = self.began_by(through);
        let named = |turn: u64| turn >= in_office.turn && begun.is_none_or(|begun| turn <= begun);
        let from_in_office = promoted.filter(|(turn, _)| named(*turn)).map(|(_, candidate)| *candidate);
        sequencers.candidates.extend(from_in_office);
        Some(sequencers)
    }

    /// Drops the records no range after `verified` needs: those before the latest at or before the
    /// command after it, from which the records then attribute ranges.
    pub(crate) fn prune(&mut self, verified: u64) {
        let first = verified.saturating_add(1);
        if let Some((&keep, _)) = self.by_seq.range(..=first).next_back() {
            self.by_seq = self.by_seq.split_off(&keep);
            self.promoted = self.promoted.split_off(&keep.saturating_add(1));
            if let Some((&base, _)) = self.began.range(..=keep).next_back() {
                self.began = self.began.split_off(&base);
            }
            self.from = self.from.max(Some(keep));
        }
    }
}

/// The latest turn that had begun by `ts`, `None` before the first one's boundary.
fn turn_begun_by(ts: Millis) -> Option<u64> {
    let turn =
        u64::try_from(ts.checked_sub(engine_core::turns::BOUNDARY_OFFSET_MS)?.div_euclid(engine_core::turns::DAY_MS))
            .ok()?;
    engine_core::turns::boundary(turn).filter(|boundary| *boundary <= ts).map(|_| turn)
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
