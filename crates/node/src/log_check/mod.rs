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
/// verify, at or after its `TurnTaken`, and the releases that sequenced the covered range, which no
/// check may come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Due {
    pub turn: u64,
    pub position: u16,
    pub set: u32,
    /// The turn's member, whose node asks and is never asked.
    pub candidate: AccountId,
    pub head: VerifiedHead,
    pub excluded: BTreeSet<B256>,
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

/// The operator's question to a member: a fresh challenge, hex, and the statement to verify.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ask {
    pub nonce: String,
    pub statement: Statement,
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
    let mut hash = Sha256::new();
    hash.update(b"engine.log-check.v1");
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
    hash.update(config_sha256);
    crate::handover::padded(hash.finalize().into())
}

/// Whether the gate applies to `engine`'s log: rotation is on and a turn is in effect whose set names
/// two or more members. A set the log no longer keeps counts as two.
pub(crate) fn applies(engine: &Engine) -> bool {
    let turns = engine.daily_turns();
    engine.operator_election().rotation_enabled
        && turns.in_effect.is_some_and(|turn| turns.sets.get(&turn.set).is_none_or(|set| set.members.len() >= 2))
}

/// The log's `TurnTaken` commands: the release each names, by the seq it was sequenced at, from the
/// first command the replay that collected them applied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Takings {
    by_seq: BTreeMap<u64, B256>,
    /// The first command seen: zero for a replay from genesis, past a snapshot's base for one from it.
    from: Option<u64>,
    /// The release the engine recorded as the last to take a turn before `from`, which a replay from
    /// a snapshot does not see logged.
    before: Option<B256>,
}

impl Takings {
    /// Notes the command at `seq` with the `events` it gave, after which the engine records `taken`
    /// as the last release to take a turn: a `TurnTaken` the engine accepted records its release, and
    /// the first command seen, when it is none, keeps `taken` as the release in office before it.
    pub(crate) fn command(&mut self, seq: u64, events: &[Event], taken: Option<B256>) {
        let turn_taken = events.iter().find_map(|event| match event {
            Event::TurnTaken { release, .. } => Some(*release),
            _ => None,
        });
        if self.from.is_none() {
            self.from = Some(seq);
            self.before = turn_taken.is_none().then_some(taken).flatten();
        }
        if let Some(release) = turn_taken {
            self.by_seq.insert(seq, release);
        }
    }

    /// Takes the records as complete from `next`, the next command's seq, when no command was seen:
    /// a log with none yet, or a start whose snapshot covers every command, in which the engine records
    /// `taken` as the last release to take a turn.
    pub(crate) fn complete_from(mut self, next: u64, taken: Option<B256>) -> Self {
        if self.from.is_none() {
            (self.from, self.before) = (Some(next), taken);
        }
        self
    }

    /// The latest `TurnTaken`'s seq and release.
    pub(crate) fn last(&self) -> Option<(u64, B256)> {
        self.by_seq.last_key_value().map(|(seq, release)| (*seq, *release))
    }

    /// The releases that sequenced the commands after `after` through `through`, as `TurnTaken`
    /// records them: the latest one at or before the first of them, whose release sequenced on from
    /// there, or with none recorded the one in office before the records start, and each one after
    /// it. Commands before the log's first `TurnTaken` name no release. `None` when these records do
    /// not reach back to the first of them, as after a snapshot start.
    pub(crate) fn sequenced(&self, after: Option<u64>, through: u64) -> Option<BTreeSet<B256>> {
        let first = after.map_or(0, |after| after.saturating_add(1));
        if self.from.is_none_or(|from| from > first) {
            return None;
        }
        let in_office = self.by_seq.range(..=first).next_back().map(|(_, release)| *release).or(self.before);
        let later = self.by_seq.range(first..=through).map(|(_, release)| *release);
        Some(in_office.into_iter().chain(later).collect())
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
/// a promoted member's resume: `head` when `release` sequenced none of the commands after `previous`
/// through it, as `takings` records; otherwise `previous`, and a following member's check is asked
/// for as at a boundary without a handover.
pub(crate) fn own_check(
    previous: Option<VerifiedHead>,
    takings: &Takings,
    head: VerifiedHead,
    release: B256,
) -> Option<VerifiedHead> {
    if previous.is_some_and(|previous| previous.seq >= head.seq) {
        return previous;
    }
    let sequenced = takings.sequenced(previous.map(|previous| previous.seq), head.seq);
    match sequenced {
        Some(releases) if !releases.contains(&release) => Some(head),
        _ => previous,
    }
}

#[cfg(test)]
mod tests;
