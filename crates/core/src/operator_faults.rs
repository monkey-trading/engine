//! Penalties for provable operator faults, from `V69`.
//!
//! The fault proof contract (`contracts/src/FaultProof.sol`) proves that the enclave holding a daily
//! turn's keys signed two histories: two `engine.log-head.v2` heads of one signer epoch, binding the
//! same turn, position and release, that name different links at one seq. It emits `FaultProven`
//! naming the member the schedule assigned, once per assignment. The node records the contract and
//! the penalty once (`AdminCommand::SetFaultProof`), and mirrors each `FaultProven` from finalised
//! events on the staking mirror, credited once under its chain reference.
//!
//! - **The haircut.** A mirrored fault gives the member a haircut `h` that starts at the penalty's
//!   `initial_bps` at the mirror's stamp and falls linearly to nothing over its `decay_ms`. A fault
//!   while one decays restarts the decay at the larger of the two, which under the one recorded
//!   penalty is `initial_bps`.
//! - **The cut.** The member's time in office is credited as before, and beside it the integral of
//!   `h` over that time, in bps·ms, each command's part rounded up (`OperatorFaults::withheld`). A
//!   close pays the member `floor(share · (served − withheld / 10,000) / counted)`, so its operator
//!   share over any stretch of office is multiplied by `1 − h`, rounding against it. The withheld part
//!   stays in the stakers' remainder, shared pro rata, and `OperatorShareWithheld` reports it.
//!   Distributions already allocated are never touched, and time served before the mirror is not cut.
//!
//! A mirror that contradicts the log (before `V69` or staking, a reference already credited, or no
//! recorded contract) is refused as `ElectionRejected`, as an election's is, and holds the watcher.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, Millis};

/// The longest decay a penalty may record: ten years.
pub const MAX_DECAY_MS: Millis = 3_650 * 24 * 60 * 60 * 1000;
/// A haircut of the whole share, in bps.
pub(crate) const WHOLE_BPS: i64 = 10_000;

/// The fault proof contract the log follows and the penalty a proven fault carries, recorded once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultProofConfig {
    /// The fault proof contract's address.
    pub contract: AccountId,
    pub penalty: Penalty,
}

/// The haircut a proven fault puts on its member's operator share.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Penalty {
    /// The haircut when the fault is mirrored, in bps of the share: 1 to 10,000.
    pub initial_bps: i64,
    /// How long it takes to fall linearly to nothing, in milliseconds: 1 to `MAX_DECAY_MS`.
    pub decay_ms: Millis,
}

/// The recorded contract and penalty, each penalised member's haircut, and what the haircuts withheld
/// in the open distribution epoch. Left out of the encoding while empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorFaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<FaultProofConfig>,
    /// When each penalised member's haircut started decaying, by candidate; dropped at the first
    /// close after it reaches nothing.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub haircuts: BTreeMap<AccountId, Millis>,
    /// The haircuts' integral over each member's time in office since the open epoch began counting,
    /// in bps·ms; cleared at each close.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub withheld: BTreeMap<AccountId, i64>,
}

/// The fault proof contract's finalised `FaultProven` event, as the node mirrors it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvenFault {
    /// The candidate the schedule assigned at `turn` and `position`.
    pub member: AccountId,
    pub turn: u64,
    pub position: u16,
    /// The signer epoch both heads were signed under, and the seq they disagree at.
    pub signer_epoch: u64,
    pub seq: u64,
    /// The finalised Ethereum block the event was mirrored from.
    pub block: u64,
    /// `<tx hash>:<log index>`, credited once.
    pub chain_ref: String,
}

impl OperatorFaults {
    /// Whether nothing is recorded, so earlier state encodings stand.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Penalty {
    /// Checks the penalty's bounds.
    /// Error cases:
    /// - a haircut outside 1 to 10,000 bps, or a decay outside 1 ms to `MAX_DECAY_MS`
    fn validate(&self) -> Result<(), String> {
        if !(1..=WHOLE_BPS).contains(&self.initial_bps) {
            return Err(format!("penalty initial_bps must be in 1..={WHOLE_BPS}"));
        }
        if !(1..=MAX_DECAY_MS).contains(&self.decay_ms) {
            return Err(format!("penalty decay_ms must be in 1..={MAX_DECAY_MS}"));
        }
        Ok(())
    }
}

impl Engine {
    /// The recorded fault proof contract and the haircuts in force.
    pub fn operator_faults(&self) -> &OperatorFaults {
        &self.operator_faults
    }

    /// `member`'s haircut at `at`, in bps of its operator share, rounded down: 0 without a fault or
    /// once it has decayed.
    pub fn haircut_bps(&self, member: AccountId, at: Millis) -> i64 {
        let faults = &self.operator_faults;
        let (Some(config), Some(since)) = (faults.config, faults.haircuts.get(&member)) else { return 0 };
        let left = (since + config.penalty.decay_ms - at).clamp(0, config.penalty.decay_ms);
        // At most 10,000 · `MAX_DECAY_MS`, which an `i64` holds.
        config.penalty.initial_bps * left / config.penalty.decay_ms
    }

    /// Records the fault proof contract and the penalty, once.
    /// Error cases:
    /// - before `V69`, or once recorded
    /// - a zero address, or a penalty outside its bounds (see `Penalty::validate`)
    pub(crate) fn set_fault_proof(&mut self, config: &FaultProofConfig) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V69 {
            return Err(format!("fault proofs need protocol {:?}", ProtocolVersion::V69));
        }
        if self.operator_faults.config.is_some() {
            return Err("the fault proof contract is already recorded".into());
        }
        if config.contract == AccountId([0; 20]) {
            return Err("the fault proof contract is a nonzero address".into());
        }
        config.penalty.validate()?;
        self.operator_faults.config = Some(*config);
        Ok(())
    }

    /// Mirrors the fault proof contract's `FaultProven` event: the member's haircut restarts at the
    /// penalty from now.
    pub(crate) fn prove_fault(&mut self, fault: &ProvenFault, events: &mut Vec<Event>) {
        let refusal = if self.protocol_version < ProtocolVersion::V69 {
            Some(format!("fault proofs need protocol {:?}", ProtocolVersion::V69))
        } else {
            self.registry_mirror_refusal(&fault.chain_ref).or_else(|| {
                if self.operator_faults.config.is_none() {
                    Some("no fault proof contract is recorded".into())
                } else if fault.member == AccountId([0; 20]) {
                    Some("a fault names no member".into())
                } else {
                    None
                }
            })
        };
        if let Some(reason) = refusal {
            events.push(Event::ElectionRejected { chain_ref: fault.chain_ref.clone(), reason });
            return;
        }
        self.credit_registry_mirror(&fault.chain_ref, fault.block);
        let decay_ms = self.operator_faults.config.expect("checked above").penalty.decay_ms;
        self.operator_faults.haircuts.insert(fault.member, self.now);
        events.push(Event::OperatorFaultProven {
            member: fault.member,
            turn: fault.turn,
            position: fault.position,
            signer_epoch: fault.signer_epoch,
            seq: fault.seq,
            decays_until: self.now.saturating_add(decay_ms),
        });
    }

    /// The integral of `holder`'s haircut over `[from, to]` of its office, in bps·ms, rounded up: over
    /// the part `[a, b]` the haircut covers, `initial_bps · ((end − a)² − (end − b)²) / (2 · decay_ms)`,
    /// with `end` when it reaches nothing. 0 without a haircut.
    pub(crate) fn withheld_in_office(&self, holder: AccountId, from: Millis, to: Millis) -> i64 {
        let faults = &self.operator_faults;
        let (Some(config), Some(&since)) = (faults.config, faults.haircuts.get(&holder)) else { return 0 };
        let end = since.saturating_add(config.penalty.decay_ms);
        let (a, b) = (from.max(since), to.min(end));
        if b <= a {
            return 0;
        }
        let square = |left: Millis| (left as u128) * (left as u128);
        // Each square is at most `MAX_DECAY_MS²`, so the product stays far inside a `u128`.
        let doubled = config.penalty.initial_bps as u128 * (square(end - a) - square(end - b));
        // At most `initial_bps · (b − a)`, which an `i64` holds.
        doubled.div_ceil(2 * config.penalty.decay_ms as u128) as i64
    }

    /// Drops each haircut that has decayed to nothing by now.
    pub(crate) fn drop_spent_haircuts(&mut self) {
        let Some(config) = self.operator_faults.config else { return };
        let now = self.now;
        self.operator_faults.haircuts.retain(|_, since| since.saturating_add(config.penalty.decay_ms) > now);
    }
}
