//! The elected operator key, from `V37`.
//!
//! The operator registry on chain elects the operator by delegated MONKEY stake. The engine mirrors
//! its finalised events on the staking mirror, each credited once under its chain reference and
//! advancing the staking mirror's authenticated frontier: rotation turned on or off, and each
//! election. An election that names a key other than the one in effect records a pending rotation
//! (`operator_bounds::PendingRotation`), which hands over at the election's effective time, or
//! seven days after the election is logged when that is later, so the log always gives seven days'
//! notice, and never before the staking mirror's finalised frontier has reached the registry's
//! effective time: the registry cancels only before then, so a cancellation is always mirrored
//! before the key it cancels could take over. Turning rotation on drops a signed rotation still
//! pending, and while rotation is on `RotateOperator` is refused, so only an election moves the
//! key: an incumbent key change can never race an election. Turning rotation off with an election
//! not yet in effect cancels it, and its pending rotation; a signed rotation is also refused while
//! an elected handover waits to take effect, even with rotation off. A successor election a
//! catch-up mirrors before that handover applied supersedes it, and cancelling the successor
//! restores it, as the registry then names its winner again.
//!
//! From `V47` the mirror also records the candidate of the last election to take effect, with the
//! key it was elected under, which earns the operator's share of each distribution (`staking`). It
//! moves when an election takes effect, under the same conditions as its key, including an election
//! naming the key already in effect, which moves no key, and which a successor supersedes and its
//! cancellation restores as it does any other. It earns only while that key is in effect: an operator
//! no election installed earns in its key's address, which is the candidate identity the registry
//! gives its initial operator.
//!
//! #359 §8's registry path for the operator to change its own key is not built yet; the decision
//! recorded for it: the change takes the same seven-day notice, each change waits the full notice,
//! and it neither resets nor holds an open election — the mirrored election stands, and where the
//! two would name different keys the election wins.

use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::event::Event;
use crate::operator_bounds::{NOTICE_MS, PendingRotation};
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MAX_STAMP_MS, Millis};

/// The registry as the engine mirrors it: whether rotation is on, the last election, the election a
/// successor superseded before it took effect, kept to restore if the successor is cancelled, and
/// from `V47` the candidate seated by the last election to take effect. Left out of the encoding while
/// empty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorElection {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rotation_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<Election>,
    /// The election the last one superseded before it took effect: its handover still waited, or from
    /// `V47` it named the key in effect and its time had not come. Cancelling the last restores it,
    /// since the registry then names it again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prior: Option<Election>,
    /// From `V47`, the candidate of the last election to take effect. `None` on a log that activated
    /// `V47` with its last election not yet in effect, or cancelled, until one takes effect, since no
    /// earlier rule records who the one in effect elected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seated: Option<SeatedCandidate>,
}

/// From `V47`, a candidate an election seated, and the key it was elected under, which it earns the
/// operator's share by while that key is the key in effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeatedCandidate {
    pub candidate: AccountId,
    pub admin_key: AccountId,
}

/// An election the registry made, as the engine applies it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Election {
    /// The registry's election epoch whose close elected it.
    pub epoch: u64,
    /// The elected candidate's registry address.
    pub candidate: AccountId,
    /// The key that signs admin commands from `effective_at`.
    pub admin_key: AccountId,
    /// When the key takes effect in sequenced time: the registry's effective time, or seven days
    /// after the election was logged when that is later.
    pub effective_at: Millis,
    /// The registry's effective time, which the staking mirror's finalised frontier must reach
    /// before the key takes over.
    pub registry_effective_at: Millis,
    /// Whether turning rotation off cancelled it before it took effect.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
}

/// The registry's finalised `Elected` event, as the node mirrors it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryElection {
    pub epoch: u64,
    pub candidate: AccountId,
    pub admin_key: AccountId,
    /// The registry's effective time, in milliseconds.
    pub effective_at: Millis,
    /// The finalised Ethereum block the event was mirrored from.
    pub block: u64,
    /// `<tx hash>:<log index>`, credited once.
    pub chain_ref: String,
}

impl OperatorElection {
    /// Whether nothing is recorded, so earlier state encodings stand.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Engine {
    /// The mirrored registry: whether rotation is on, the last election, and the candidate seated.
    pub fn operator_election(&self) -> &OperatorElection {
        &self.operator_election
    }

    /// Mirrors the registry's `RotationChanged` event: rotation turned on or off, and when turning
    /// it off cancelled the last election, that election and its pending rotation.
    pub(crate) fn switch_rotation(
        &mut self,
        enabled: bool,
        election_cancelled: bool,
        block: u64,
        chain_ref: &str,
        events: &mut Vec<Event>,
    ) {
        let reason = self.registry_mirror_refusal(chain_ref).or_else(|| {
            let last = self.operator_election.last.filter(|last| !last.cancelled);
            let reached = self.finalised_chain_time();
            if enabled == self.operator_election.rotation_enabled {
                Some(format!("rotation is already {}", if enabled { "on" } else { "off" }))
            } else if election_cancelled && enabled {
                Some("turning rotation on cancels no election".into())
            } else if election_cancelled && last.is_none_or(|last| reached >= Some(last.registry_effective_at)) {
                // The registry cancels only an election not yet in effect on chain, and its key
                // waits for the mirror to pass that time; a cancel after it, or of none, means the
                // mirror and the chain disagree.
                Some("no election waits to take effect".into())
            } else {
                None
            }
        });
        if let Some(reason) = reason {
            events.push(Event::ElectionRejected { chain_ref: chain_ref.to_owned(), reason });
            return;
        }
        self.credit_registry_mirror(chain_ref, block);
        self.operator_election.rotation_enabled = enabled;
        // A rotation the old key signed while rotation was off gives way to the elections.
        if enabled && self.operator_bounds.rotation.is_some_and(|rotation| !self.is_elected(rotation)) {
            self.operator_bounds.rotation = None;
        }
        if election_cancelled {
            let last = self.operator_election.last.as_mut().expect("checked above");
            last.cancelled = true;
            let elected = PendingRotation { address: last.admin_key, effective_at: last.effective_at };
            if self.operator_bounds.rotation == Some(elected) {
                self.operator_bounds.rotation = None;
            }
            // The registry cancels only an election not yet in effect on chain, so the one it
            // superseded is its operator again: restore the handover it interrupted.
            if let Some(prior) = self.operator_election.prior.take() {
                self.operator_election.last = Some(prior);
                let operator = self.operator.expect("checked above").address;
                self.operator_bounds.rotation = (prior.admin_key != operator)
                    .then_some(PendingRotation { address: prior.admin_key, effective_at: prior.effective_at });
            }
        }
        events.push(Event::RotationSwitched { enabled, election_cancelled });
    }

    /// Mirrors the registry's `Elected` event: records the election, and a pending rotation to its
    /// key when that is not the key in effect, replacing any pending rotation. An election the
    /// successor supersedes before it took effect is kept as `prior`, restored if the successor is
    /// cancelled before it takes effect.
    pub(crate) fn elect_operator(&mut self, election: &RegistryElection, events: &mut Vec<Event>) {
        let RegistryElection { epoch, candidate, admin_key, effective_at, block, ref chain_ref } = *election;
        let reason = self.registry_mirror_refusal(chain_ref).or_else(|| {
            let zero = AccountId([0; 20]);
            if !self.operator_election.rotation_enabled {
                Some("an election while rotation is off".into())
            } else if self.operator.is_none() {
                Some("no operator is recorded".into())
            } else if candidate == zero || admin_key == zero {
                Some("an election names a nonzero candidate and key".into())
            } else if self.operator_election.last.is_some_and(|last| epoch <= last.epoch) {
                Some(format!("election epoch {epoch} is not after the last one"))
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
        let registry_effective_at = effective_at;
        let effective_at = effective_at.max(self.now + NOTICE_MS);
        let operator = self.operator.expect("checked above").address;
        // A successor a catch-up mirrors before this one took effect supersedes it; keep it so
        // cancelling the successor restores it, as the registry then names it again. Before `V47`
        // only a waiting handover counts; from it, so does an election naming the key in effect, which
        // moves the candidate.
        let waiting = |last: &Election| {
            self.operator_bounds.rotation.is_some_and(|rotation| self.is_elected(rotation))
                || (self.protocol_version >= ProtocolVersion::V47 && !last.cancelled && !self.has_taken_effect(last))
        };
        self.operator_election.prior = self.operator_election.last.filter(waiting);
        self.operator_bounds.rotation =
            (admin_key != operator).then_some(PendingRotation { address: admin_key, effective_at });
        self.operator_election.last =
            Some(Election { epoch, candidate, admin_key, effective_at, registry_effective_at, cancelled: false });
        events.push(Event::OperatorElected { epoch, candidate, admin_key, effective_at });
    }

    /// The pending rotation that hands over before a command stamped `ts`: its time has come and,
    /// for an elected key, the staking mirror's finalised frontier has reached the registry's
    /// effective time.
    pub fn due_rotation(&self, ts: Millis) -> Option<PendingRotation> {
        let reached = self.finalised_chain_time();
        self.operator_bounds.rotation.filter(|rotation| {
            rotation.effective_at <= ts
                && (!self.is_elected(*rotation)
                    || self.operator_election.last.is_some_and(|last| reached >= Some(last.registry_effective_at)))
        })
    }

    /// From `V47`, seats the last election's candidate, with its key, once that election's time has
    /// come (`has_taken_effect`). These are `due_rotation`'s conditions, so a handover and its candidate
    /// move together, and an election naming the key already in effect moves the candidate at the time
    /// it would have moved a key.
    pub(crate) fn settle_operator_candidate(&mut self) {
        if self.protocol_version < ProtocolVersion::V47 {
            return;
        }
        let Some(last) = self.operator_election.last.filter(|last| !last.cancelled) else { return };
        if self.has_taken_effect(&last) {
            let seated = SeatedCandidate { candidate: last.candidate, admin_key: last.admin_key };
            self.operator_election.seated = Some(seated);
        }
    }

    /// Whether `election`'s time has come by the sequenced time and the staking mirror's finalised
    /// frontier has reached the registry's effective time, as `due_rotation` requires of its key.
    fn has_taken_effect(&self, election: &Election) -> bool {
        election.effective_at <= self.now && self.finalised_chain_time() >= Some(election.registry_effective_at)
    }

    /// The candidate seated by the last election to take effect, while the key it was elected under
    /// is the key in effect. `None` while the key in effect is one no election installed.
    pub fn operator_candidate(&self) -> Option<AccountId> {
        let operator = self.operator?.address;
        self.operator_election.seated.filter(|seated| seated.admin_key == operator).map(|seated| seated.candidate)
    }

    /// From `V47`, the account that earns the operator's share of a distribution closing now: while
    /// rotation is on, the candidate seated under the key in effect, or for an operator no election
    /// installed the address of its key. `None` before `V47` or while rotation is off; `V8` on always
    /// records an operator.
    pub(crate) fn operator_share_account(&self) -> Option<AccountId> {
        if self.protocol_version < ProtocolVersion::V47 || !self.operator_election.rotation_enabled {
            return None;
        }
        self.operator_candidate().or_else(|| self.operator.map(|operator| operator.address))
    }

    /// The finalised block time the staking mirror has completed through, from `V37`.
    fn finalised_chain_time(&self) -> Option<Millis> {
        self.staking.as_ref().and_then(|staking| staking.completed_through).and_then(|checkpoint| checkpoint.time)
    }

    /// Whether `rotation` is the one the last election, not cancelled, recorded.
    pub(crate) fn is_elected(&self, rotation: PendingRotation) -> bool {
        self.operator_election.last.is_some_and(|last| {
            !last.cancelled && last.admin_key == rotation.address && last.effective_at == rotation.effective_at
        })
    }

    /// Whether elections move the operator key, which refuses `RotateOperator`.
    pub(crate) fn rotation_by_election(&self) -> bool {
        self.operator_election.rotation_enabled
    }

    /// Why a registry mirror under `chain_ref` cannot apply, if it cannot.
    fn registry_mirror_refusal(&self, chain_ref: &str) -> Option<String> {
        if self.protocol_version < ProtocolVersion::V37 {
            Some(format!("operator elections need protocol {:?}", ProtocolVersion::V37))
        } else if self.staking.is_none() {
            Some("staking is not initialised".into())
        } else if self.staking_refs.contains(chain_ref) {
            Some(format!("chain reference {chain_ref} is already mirrored"))
        } else {
            None
        }
    }

    /// Credits a registry mirror once under `chain_ref`, advancing the staking mirror's frontier to
    /// `block`.
    fn credit_registry_mirror(&mut self, chain_ref: &str, block: u64) {
        self.staking_refs.insert(chain_ref.to_owned());
        // Checked by `registry_mirror_refusal`.
        let staking = self.staking.as_mut().expect("staking is initialised");
        staking.mirrored_through = Some(staking.mirrored_through.map_or(block, |frontier| frontier.max(block)));
    }
}
