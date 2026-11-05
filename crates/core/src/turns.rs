//! Daily operator turns, from `V51`.
//!
//! An operator registry that names a schedule (`contracts/src/OperatorSchedule.sol`) adopts a
//! rotation set at each election close: every eligible candidate, weighted by its delegated stake,
//! taking effect seven days plus the finality margin later. The schedule draws each turn from the
//! set in effect for its boundary, 22:30 UTC on the turn's day of the Unix epoch, from a seed relayed
//! from Ethereum. The engine mirrors both from finalised events on the staking mirror, each credited
//! once under its chain reference and advancing the mirror's frontier.
//!
//! - A turn's admin key signs from its boundary when it is mirrored before it, and otherwise from its
//!   mirror: a late draw, a reassignment after its boundary, or a promotion, which the mirror reads
//!   only once it is finalised. The key moves before the command stamped at that time is
//!   authorised, so the previous turn's key signs nothing from then.
//! - A turn whose key no set mirrored at least seven days before the turn applies names is refused:
//!   credited, with the key in effect kept. A set's adoption is the log's notice of every key in it.
//! - `RotateOperator` stays refused while rotation is on (`operator_bounds`).
//! - The member's enclave logs `TurnTaken` when it first sequences its turn. While turns run, the
//!   operator's share counts the member's time in office from then to the end of its turn; time
//!   before a member takes its turn, or after its turn has ended, goes to stakers.
//!
//! A mirror that contradicts the log (before `V51` or staking, a reference already credited, a set
//! out of order or malformed) is refused as `ElectionRejected`, as an election's is, and holds the
//! watcher.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::eip712::B256;
use crate::emissions::TokenUnits;
use crate::engine::Engine;
use crate::event::Event;
use crate::operator_bounds::NOTICE_MS;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MAX_STAMP_MS, Millis};

/// A turn's length: one day.
pub const DAY_MS: Millis = 24 * 60 * 60 * 1000;
/// A turn's boundary within its day: 22:30 UTC.
pub const BOUNDARY_OFFSET_MS: Millis = (22 * 60 + 30) * 60 * 1000;
/// The most members a set may name: the registry's `CANDIDATE_LIMIT`.
pub const MAX_SET_MEMBERS: usize = 100;
/// The sets kept: the one turns draw from, the one waiting to take effect, and the one before,
/// whose drawn turns may still be due.
pub const KEPT_SETS: usize = 3;

/// The mirrored schedule: the sets the engine keeps, the turns mirrored and not yet applied, the
/// assignment whose key is in effect, and whether its member has taken it. Left out of the
/// encoding while empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DailyTurns {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sets: BTreeMap<u32, RotationSet>,
    /// Accepted assignments whose key has yet to apply, by turn; a later assignment of a turn
    /// replaces an earlier one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub assigned: BTreeMap<u64, TurnAssignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_effect: Option<TurnAssignment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken: Option<TakenTurn>,
}

/// A rotation set the schedule adopted, as mirrored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotationSet {
    /// The election epoch whose close adopted it.
    pub epoch: u64,
    /// When it takes effect on chain, in milliseconds.
    pub effective_at: Millis,
    /// The sequenced time it was mirrored at: the log's notice of its keys starts here.
    pub logged_at: Millis,
    pub members: Vec<SetMember>,
}

/// One member of a rotation set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetMember {
    pub candidate: AccountId,
    pub admin_key: AccountId,
    /// Its delegated stake when the set was adopted, the weight it is drawn by.
    pub weight: TokenUnits,
}

/// Why the schedule named a turn's member: the day's draw, or a later reassignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnReason {
    Draw,
    /// The position before it did not take the turn in time.
    Skip,
    /// The liveness rule moved the turn from a failed member.
    Promotion,
}

/// A turn assignment the engine accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnAssignment {
    pub turn: u64,
    /// The member's position in the turn's order.
    pub position: u16,
    pub reason: TurnReason,
    /// The set it was drawn from.
    pub set: u32,
    pub candidate: AccountId,
    pub admin_key: AccountId,
    /// When its key signs from: its boundary, or its mirror when that is later.
    pub applies_at: Millis,
}

/// The assignment in effect once its member's enclave logged `TurnTaken`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TakenTurn {
    pub turn: u64,
    pub position: u16,
    /// The taking enclave's release: `keccak256` of its launch measurement and config hash.
    pub release: B256,
}

/// The schedule's finalised `RotationSetAdopted` event, as the node mirrors it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryRotationSet {
    pub set: u32,
    pub epoch: u64,
    /// When it takes effect on chain, in milliseconds.
    pub effective_at: Millis,
    /// Whether the close reached its quorum, so the snapshot replaced the set.
    pub quorum: bool,
    pub members: Vec<SetMember>,
    /// The finalised Ethereum block the event was mirrored from.
    pub block: u64,
    /// `<tx hash>:<log index>`, credited once.
    pub chain_ref: String,
}

/// The schedule's finalised `TurnAssigned` event, as the node mirrors it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryTurn {
    pub turn: u64,
    pub set: u32,
    pub position: u16,
    pub reason: TurnReason,
    pub candidate: AccountId,
    pub admin_key: AccountId,
    /// The finalised Ethereum block the event was mirrored from.
    pub block: u64,
    /// `<tx hash>:<log index>`, credited once.
    pub chain_ref: String,
}

impl DailyTurns {
    /// Whether nothing is recorded, so earlier state encodings stand.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// When `turn` starts: 22:30 UTC on its day of the Unix epoch, or `None` past the latest stamp.
pub fn boundary(turn: u64) -> Option<Millis> {
    let ms = Millis::try_from(turn).ok()?.checked_mul(DAY_MS)?.checked_add(BOUNDARY_OFFSET_MS)?;
    (ms <= MAX_STAMP_MS).then_some(ms)
}

/// When `turn` ends: the next turn's boundary.
fn turn_end(turn: u64) -> Millis {
    // Accepted turns have a boundary, and a day past it stays within `i64`.
    boundary(turn).expect("an accepted turn has a boundary") + DAY_MS
}

impl Engine {
    /// The mirrored schedule.
    pub fn daily_turns(&self) -> &DailyTurns {
        &self.daily_turns
    }

    /// Mirrors the schedule's `RotationSetAdopted` event: records the set, keeping the newest
    /// `KEPT_SETS`.
    pub(crate) fn adopt_rotation_set(&mut self, adoption: &RegistryRotationSet, events: &mut Vec<Event>) {
        let RegistryRotationSet { set, epoch, effective_at, ref members, block, ref chain_ref, .. } = *adoption;
        let reason = self.turns_mirror_refusal(chain_ref).or_else(|| {
            let zero = AccountId([0; 20]);
            let mut candidates = std::collections::BTreeSet::new();
            let mut keys = std::collections::BTreeSet::new();
            let distinct = members.iter().all(|m| candidates.insert(m.candidate) && keys.insert(m.admin_key));
            if self.daily_turns.sets.last_key_value().is_some_and(|(last, _)| set <= *last) {
                Some(format!("set {set} is not after the last mirrored"))
            } else if members.is_empty() || members.len() > MAX_SET_MEMBERS {
                Some(format!("a set names 1 to {MAX_SET_MEMBERS} members, not {}", members.len()))
            } else if members.iter().any(|m| m.candidate == zero || m.admin_key == zero || m.weight.0 == 0) {
                Some("a set names nonzero candidates and keys with positive weights".into())
            } else if !distinct {
                Some("a set names each candidate and key once".into())
            } else if !(0..=MAX_STAMP_MS).contains(&effective_at) {
                Some(format!("effective time {effective_at} is outside 0 to {MAX_STAMP_MS}"))
            } else {
                None
            }
        });
        if let Some(reason) = reason {
            events.push(Event::ElectionRejected { chain_ref: chain_ref.clone(), reason });
            return;
        }
        self.credit_registry_mirror(chain_ref, block);
        let recorded = RotationSet { epoch, effective_at, logged_at: self.now, members: members.clone() };
        self.daily_turns.sets.insert(set, recorded);
        while self.daily_turns.sets.len() > KEPT_SETS {
            self.daily_turns.sets.pop_first();
        }
        events.push(Event::RotationSetAdopted { set, epoch, effective_at });
    }

    /// Mirrors the schedule's `TurnAssigned` event: records the assignment, whose key applies from
    /// its boundary or from now when that is later, unless its key had no seven days' notice in a
    /// mirrored set, or its turn is over, which credits it as refused.
    pub(crate) fn assign_turn(&mut self, assignment: &RegistryTurn, events: &mut Vec<Event>) {
        let RegistryTurn { turn, set, position, reason, candidate, admin_key, block, ref chain_ref } = *assignment;
        let refusal = self.turns_mirror_refusal(chain_ref).or_else(|| {
            let zero = AccountId([0; 20]);
            if candidate == zero || admin_key == zero {
                Some("a turn names a nonzero candidate and key".into())
            } else if boundary(turn).is_none_or(|start| start > MAX_STAMP_MS - DAY_MS) {
                Some(format!("turn {turn} has no boundary within the latest stamp"))
            } else {
                None
            }
        });
        if let Some(reason) = refusal {
            events.push(Event::ElectionRejected { chain_ref: chain_ref.clone(), reason });
            return;
        }
        self.credit_registry_mirror(chain_ref, block);
        let start = boundary(turn).expect("checked above");
        let applies_at = start.max(self.now);
        if let Some(refused) = self.turn_refusal(turn, set, candidate, admin_key, applies_at) {
            events.push(Event::TurnRefused { turn, position, cause: reason, candidate, admin_key, reason: refused });
            return;
        }
        let accepted = TurnAssignment { turn, position, reason, set, candidate, admin_key, applies_at };
        self.daily_turns.assigned.insert(turn, accepted);
        events.push(Event::TurnAssigned { turn, position, reason, candidate, admin_key, applies_at });
    }

    /// Why a credited assignment cannot apply its key at `applies_at`, if it cannot.
    fn turn_refusal(
        &self,
        turn: u64,
        set: u32,
        candidate: AccountId,
        admin_key: AccountId,
        applies_at: Millis,
    ) -> Option<String> {
        let notice = |recorded: &RotationSet| recorded.logged_at + NOTICE_MS <= applies_at;
        let member = |recorded: &RotationSet| {
            recorded.members.iter().any(|m| m.candidate == candidate && m.admin_key == admin_key)
        };
        if turn_end(turn) <= self.now {
            Some(format!("turn {turn} ended before its assignment was mirrored"))
        } else if self.daily_turns.in_effect.is_some_and(|in_effect| in_effect.turn > turn) {
            Some(format!("turn {turn} is before the turn in effect"))
        } else {
            match self.daily_turns.sets.get(&set) {
                None => Some(format!("set {set} is not among the sets mirrored")),
                Some(recorded) if !member(recorded) => {
                    Some(format!("set {set} does not name candidate {candidate} with key {admin_key}"))
                }
                Some(recorded) if !notice(recorded) => Some(format!(
                    "set {set} was logged at {}, less than seven days before the turn applies at {applies_at}",
                    recorded.logged_at
                )),
                Some(_) => None,
            }
        }
    }

    /// From `V51`, the latest accepted assignment whose key applies by `ts`, if it is not the one in
    /// effect already.
    pub fn due_turn(&self, ts: Millis) -> Option<TurnAssignment> {
        if self.protocol_version < ProtocolVersion::V51 {
            return None;
        }
        self.daily_turns.assigned.values().rev().find(|assignment| assignment.applies_at <= ts).copied()
    }

    /// Hands the operator's role to the due turn's key (see `due_turn`), before the command stamped
    /// `ts` is authorised, so the previous turn's key signs nothing from then. Earlier assignments
    /// it supersedes are dropped.
    pub(crate) fn apply_due_turn(&mut self, ts: Millis) {
        let Some(due) = self.due_turn(ts) else { return };
        // `V8` on always records an operator, and `V51` is later.
        let operator = self.operator.as_mut().expect("V51 records an operator");
        if operator.address != due.admin_key {
            operator.address = due.admin_key;
            self.operator_bounds.rotations += 1;
        }
        self.daily_turns.assigned.retain(|turn, _| *turn > due.turn);
        self.daily_turns.in_effect = Some(due);
    }

    /// Records that this node's enclave, running `release`, took the turn in effect, `turn`.
    pub(crate) fn take_turn(&mut self, turn: u64, release: B256, events: &mut Vec<Event>) {
        let in_effect = self.daily_turns.in_effect;
        let reason = if self.protocol_version < ProtocolVersion::V51 {
            Some(format!("taking a turn needs protocol {:?}", ProtocolVersion::V51))
        } else if in_effect.is_none_or(|in_effect| in_effect.turn != turn) {
            Some(format!("turn {turn} is not the turn in effect"))
        } else if self.daily_turns.taken.is_some_and(|taken| {
            in_effect.is_some_and(|in_effect| (taken.turn, taken.position) == (turn, in_effect.position))
        }) {
            Some(format!("turn {turn} is already taken"))
        } else {
            None
        };
        if let Some(reason) = reason {
            events.push(Event::TurnTakenRejected { reason });
            return;
        }
        let in_effect = in_effect.expect("checked above");
        self.daily_turns.taken = Some(TakenTurn { turn, position: in_effect.position, release });
        events.push(Event::TurnTaken { turn, candidate: in_effect.candidate, release });
    }

    /// From `V51`, while rotation is on and a turn is in effect, its member while it holds the
    /// turn it took, with the end of that turn; `None` before its `TurnTaken` or past the turn's end.
    /// The outer `None` means no turn has applied, so the earlier rule names the account in office.
    pub(crate) fn turn_office(&self) -> Option<Option<(AccountId, Millis)>> {
        if self.protocol_version < ProtocolVersion::V51 {
            return None;
        }
        let in_effect = self.daily_turns.in_effect?;
        let taken = self
            .daily_turns
            .taken
            .is_some_and(|taken| (taken.turn, taken.position) == (in_effect.turn, in_effect.position));
        Some(
            (taken && self.operator_election.rotation_enabled).then(|| (in_effect.candidate, turn_end(in_effect.turn))),
        )
    }

    /// Drops the turns an installation supersedes: the registry names the installed operator from
    /// then, so no turn is in effect.
    pub(crate) fn end_turns(&mut self) {
        self.daily_turns.assigned.clear();
        self.daily_turns.in_effect = None;
        self.daily_turns.taken = None;
    }

    /// Why a schedule mirror under `chain_ref` cannot apply, if it cannot: an election's reasons,
    /// or a protocol before `V51`.
    fn turns_mirror_refusal(&self, chain_ref: &str) -> Option<String> {
        if self.protocol_version < ProtocolVersion::V51 {
            return Some(format!("daily turns need protocol {:?}", ProtocolVersion::V51));
        }
        self.registry_mirror_refusal(chain_ref)
    }
}
