//! The elected operator key, from `V37`.
//!
//! The operator registry on chain elects the operator by delegated MONKEY stake. The engine mirrors
//! its finalised events on the staking mirror, each credited once under its chain reference and
//! advancing the staking mirror's authenticated frontier: rotation turned on or off, and each
//! election. An election that names a key other than the one in effect records a pending rotation
//! (`operator_bounds::PendingRotation`), which hands over at the election's effective time, or
//! seven days after the election is logged when that is later, so the log always gives seven days'
//! notice. While rotation is on, `RotateOperator` is refused, so only an election moves the key.
//! Turning rotation off with an election not yet in effect cancels it, and its pending rotation.

use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::event::Event;
use crate::operator_bounds::{NOTICE_MS, PendingRotation};
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MAX_STAMP_MS, Millis};

/// The registry as the engine mirrors it: whether rotation is on, and the last election. Left out
/// of the encoding while empty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorElection {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rotation_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<Election>,
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
    /// The mirrored registry: whether rotation is on, and the last election.
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
            if enabled == self.operator_election.rotation_enabled {
                Some(format!("rotation is already {}", if enabled { "on" } else { "off" }))
            } else if election_cancelled && enabled {
                Some("turning rotation on cancels no election".into())
            } else if election_cancelled && last.is_none_or(|last| self.now >= last.effective_at) {
                // The registry cancels only an election not yet in effect on chain; one the engine
                // has already put in effect, or none, means the mirror and the chain disagree.
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
        if election_cancelled {
            let last = self.operator_election.last.as_mut().expect("checked above");
            last.cancelled = true;
            let elected = PendingRotation { address: last.admin_key, effective_at: last.effective_at };
            if self.operator_bounds.rotation == Some(elected) {
                self.operator_bounds.rotation = None;
            }
        }
        events.push(Event::RotationSwitched { enabled, election_cancelled });
    }

    /// Mirrors the registry's `Elected` event: records the election, and a pending rotation to its
    /// key when that is not the key in effect, replacing any pending rotation.
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
        let effective_at = effective_at.max(self.now + NOTICE_MS);
        let operator = self.operator.expect("checked above").address;
        self.operator_bounds.rotation =
            (admin_key != operator).then_some(PendingRotation { address: admin_key, effective_at });
        self.operator_election.last = Some(Election { epoch, candidate, admin_key, effective_at, cancelled: false });
        events.push(Event::OperatorElected { epoch, candidate, admin_key, effective_at });
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
