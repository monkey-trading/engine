//! Parameter changes by stake-weighted operator vote, from `V56`.
//!
//! While rotation is on, the governed parameters change only by a vote of the rotation set's
//! members on the vote contract (`contracts/src/ParameterVote.sol`), weighted by the stake the set
//! snapshotted. The node records the contract and the restriction lifetime once
//! (`AdminCommand::SetGovernance`), and mirrors each passed proposal from finalised events on the
//! staking mirror, credited once under its chain reference, with the admin command its bytes decode
//! to canonically (`decode_command`).
//!
//! - A normal proposal names a governed command (`is_governed`). It is recorded when it is mirrored
//!   and applies no sooner than seven days later, when the node sequences `ApplyProposal` naming it:
//!   the vote's seven days are the notice, so `AnnounceParams`, `AnnounceMarkup` and `AnnounceListing`
//!   give way to it, and every other check the command meets applies: the ±50% range of the launch
//!   profit fee and close-out level, lower-only close-spread ceilings at or above their market's launch
//!   close charge, the 1x floor on each stress level's size multiplier, a listing notice's bounds,
//!   each changed spread band table from its launch rates to twice them and each changed margin band
//!   table at or above launch's, the toxicity score's, each tier within ±50% of its launch score and
//!   add-on and a market's markout floor fixed at its listing, the holding-time fee's, the base fee
//!   never below its launch value and the curve at or above its launch curve at every hold, and from
//!   `V59` each launch house loss threshold kept at its level or above, at most its launch value
//!   (`house_loss`). A `SetParams` or `UpsertMarket`
//!   applies only over the baseline it was mirrored against, so a change applied while it waits is
//!   never undone by it, and a ceiling adoption only while none waits. It carries every field, so
//!   members check it against the values in force when they vote.
//! - A fast-path proposal applies at once as a restriction (`restriction`): only a higher margin or
//!   close-out level, a stress floor, a lower house or market cap, or reduce-only, refused if any
//!   field moves the other way. A restriction never edits the baseline: the values in force are the
//!   stricter of the baseline and every restriction in force, and only normal votes, or the operator
//!   with rotation off, move the baseline. It lapses `restriction_ttl_ms` after it is logged unless
//!   another fast-path vote on the same command, or on the same bounds, renews it, and no vote lifts
//!   one early: parameters that drop a stress level a restriction holds a floor at are refused. The
//!   operator's own stress floor and reduce-only sit beside the restrictions and cannot clear one.
//! - While rotation is on, the operator's signed commands for the governed fields are refused, and so
//!   are the node's unsigned ones (`refused_outside_vote`): a protocol activation then waits for a
//!   vote. Rotation is on as the registry's `rotationEnabled()` has it: its switch, or a drawn turn
//!   naming the operator until an installation ends the turns, the engine's own or one it refused.
//!
//! A mirror that contradicts the log (before `V56`, staking or the recorded contract, a reference
//! already credited, or a command that does not hash to the proposal's) is refused as
//! `ElectionRejected`, as an election's is, and holds the watcher. A credited proposal the rules
//! refuse is `ProposalRefused` and changes nothing else.

mod restriction;

use std::collections::BTreeMap;

use alloy_primitives::keccak256;
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::command::AdminCommand;
use crate::eip712::B256;
use crate::engine::Engine;
use crate::event::Event;
use crate::market::Tier;
use crate::operator_bounds::NOTICE_MS;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId, Millis};

/// The most normal changes that may wait out their notice at once.
pub const MAX_PENDING_CHANGES: usize = 64;
/// The most fast-path restrictions that may be in force at once.
pub const MAX_RESTRICTIONS: usize = 64;

/// The vote contract the log follows and how long a fast-path restriction lasts, recorded once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GovernanceConfig {
    /// The vote contract's address.
    pub vote: AccountId,
    /// How long a restriction lasts after it is logged, at most the seven days a normal change waits,
    /// so a restriction never outlasts the notice of the vote that could make it the baseline.
    pub restriction_ttl_ms: Millis,
}

/// The mirrored votes: the recorded contract, the normal changes waiting out their notice, the
/// restrictions in force, and the baseline of each field a restriction holds. Left out of the
/// encoding while empty.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Governance {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<GovernanceConfig>,
    /// In the order mirrored, which is the order they apply from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<PendingChange>,
    /// By proposal.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub restrictions: BTreeMap<u64, Restriction>,
    /// Whether the schedule has drawn a turn no installation has ended since `V56`, accepted or refused:
    /// the registry's `rotationEnabled()` holds while one names the operator, whatever its switch.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub turns_drawn: bool,
    /// The parameters' restricted fields as the baseline sets them, while a restriction holds one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params_baseline: Option<ParamsBaseline>,
    /// Each market's restricted fields as the baseline sets them, while a restriction holds one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub market_baselines: BTreeMap<MarketId, MarketBaseline>,
}

/// A passed normal change waiting out its notice.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingChange {
    pub proposal: u64,
    pub command: AdminCommand,
    /// `keccak256` of what a `SetParams` or `UpsertMarket` replaces, the baseline parameters or the
    /// market's baseline spec (`null` while unlisted), as it was mirrored: it applies only over them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<B256>,
    /// Seven days after it was logged.
    pub applies_at: Millis,
}

/// A fast-path restriction in force.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Restriction {
    pub limit: Limit,
    /// `keccak256` of the command that set it, which a later fast-path vote carrying it renews.
    pub command_hash: B256,
    /// `restriction_ttl_ms` after it was logged.
    pub expires_at: Millis,
}

/// What a restriction holds: each named value is a bound the value in force may not pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Limit {
    /// The least close-out level, and the most the total house cap and its floor may be.
    Params {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        closeout_level_bps: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        house_total_cap_bps: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        house_total_cap_floor: Option<Fixed>,
    },
    /// A listed market's least margin bands, and the most its house cap and floor may be.
    Market {
        market: MarketId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        margin_tiers: Option<Vec<Tier>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        house_cap_bps: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        house_cap_floor: Option<Fixed>,
    },
    /// The least stress level in one market, or in every market.
    StressFloor { market: Option<MarketId>, level: usize },
    /// No new risk in one market, or in every market.
    ReduceOnly { market: Option<MarketId> },
}

/// The parameters' restricted fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParamsBaseline {
    pub closeout_level_bps: i64,
    pub house_total_cap_bps: i64,
    pub house_total_cap_floor: Fixed,
}

/// A market's restricted fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketBaseline {
    pub margin_tiers: Vec<Tier>,
    pub house_cap_bps: i64,
    pub house_cap_floor: Fixed,
}

/// The vote contract's finalised `ProposalPassed` event, as the node mirrors it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PassedProposal {
    pub proposal: u64,
    /// The fast path: a restriction, in force at once.
    pub fast: bool,
    /// `keccak256` of the proposal's command bytes, as the contract recorded it.
    pub command_hash: B256,
    /// The admin command the bytes decode to canonically, or `None` when they do not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<AdminCommand>,
    /// The finalised Ethereum block the event was mirrored from.
    pub block: u64,
    /// `<tx hash>:<log index>`, credited once.
    pub chain_ref: String,
}

/// A passed normal change, as the node sequences it once its notice has run: the proposal and the
/// command it waits with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VotedChange {
    pub proposal: u64,
    pub command: AdminCommand,
}

impl Governance {
    /// Whether nothing is recorded, so earlier state encodings stand.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The highest stress floor the restrictions in force hold in `market`, 0 for none.
    pub fn stress_floor(&self, market: MarketId) -> usize {
        self.restrictions
            .values()
            .filter_map(|restriction| match restriction.limit {
                Limit::StressFloor { market: None, level } => Some(level),
                Limit::StressFloor { market: Some(id), level } if id == market => Some(level),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    /// Whether a restriction in force makes `market` reduce-only.
    pub fn reduce_only(&self, market: MarketId) -> bool {
        self.restrictions.values().any(
            |restriction| matches!(restriction.limit, Limit::ReduceOnly { market: m } if m.is_none_or(|m| m == market)),
        )
    }
}

/// The admin command `bytes` decode to, when they are its canonical JSON, the encoding the engine
/// hashes and the operator signs: they must decode, and encode back to themselves.
pub fn decode_command(bytes: &[u8]) -> Option<AdminCommand> {
    let command: AdminCommand = serde_json::from_slice(bytes).ok()?;
    let canonical = serde_json::to_vec(&command).ok()?;
    (canonical == bytes).then_some(command)
}

/// `keccak256` of `command`'s canonical JSON, which a proposal carrying it records.
pub fn command_hash(command: &AdminCommand) -> B256 {
    // Every admin command serialises: its fields are maps, lists, strings and integers.
    keccak256(serde_json::to_vec(command).expect("admin commands serialise"))
}

/// Whether a normal vote may carry `command`: the parameters, listings, toxicity weighting,
/// close-spread ceiling adoptions, the access policy and protocol activation.
pub fn is_governed(command: &AdminCommand) -> bool {
    matches!(
        command,
        AdminCommand::SetParams(_)
            | AdminCommand::UpsertMarket(_)
            | AdminCommand::SetToxicityNotional(_)
            | AdminCommand::AdoptCloseSpreadCeiling { .. }
            | AdminCommand::SetAccessPolicy(_)
            | AdminCommand::ActivateProtocol { .. }
    )
}

impl Engine {
    /// The mirrored votes.
    pub fn governance(&self) -> &Governance {
        &self.governance
    }

    /// From `V56`, whether only a vote may apply `command`: while rotation is on, a governed command,
    /// a notice the vote's own notice replaces, or the legacy custody activation.
    pub fn refused_outside_vote(&self, command: &AdminCommand) -> bool {
        self.governed_at(self.protocol_version, command)
    }

    /// Whether only a vote may apply `command` once `version` is in force, as `refused_outside_vote`
    /// says for the version in force: how the node decides what a start's activation leaves to a vote.
    pub fn governed_at(&self, version: ProtocolVersion, command: &AdminCommand) -> bool {
        version >= ProtocolVersion::V56
            && self.votes_govern()
            && (is_governed(command)
                || matches!(
                    command,
                    AdminCommand::AnnounceParams(_)
                        | AdminCommand::AnnounceMarkup { .. }
                        | AdminCommand::AnnounceListing { .. }
                        | AdminCommand::ActivateCustodyFinality { .. }
                ))
    }

    /// Whether rotation is on as the registry's `rotationEnabled()` has it: its switch, or a drawn turn
    /// naming the operator, the engine's own or one it refused (`Governance::turns_drawn`).
    fn votes_govern(&self) -> bool {
        self.rotation_by_election() || self.governance.turns_drawn
    }

    /// From `V56`, records a mirrored turn assignment the engine credited, accepted or refused, as a drawn
    /// turn, and an installation, which ends the drawn turns, as none.
    pub(crate) fn note_turns_drawn(&mut self, events: &[Event]) {
        if self.protocol_version < ProtocolVersion::V56 {
            return;
        }
        if events.iter().any(|event| matches!(event, Event::TurnAssigned { .. } | Event::TurnRefused { .. })) {
            self.governance.turns_drawn = true;
        }
        if events.iter().any(|event| matches!(event, Event::OperatorInstalled { .. })) {
            self.governance.turns_drawn = false;
        }
    }

    /// The first passed normal change whose notice has run by `ts`, which the node sequences as
    /// `ApplyProposal`.
    pub fn due_change(&self, ts: Millis) -> Option<&PendingChange> {
        self.governance.pending.iter().find(|pending| pending.applies_at <= ts)
    }

    /// Records the vote contract and the restriction lifetime, once.
    /// Error cases:
    /// - before `V56`, or once recorded
    /// - a zero address, or a lifetime that is not positive or exceeds seven days
    pub(crate) fn set_governance(&mut self, config: &GovernanceConfig) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V56 {
            return Err(format!("parameter votes need protocol {:?}", ProtocolVersion::V56));
        }
        if self.governance.config.is_some() {
            return Err("the vote contract is already recorded".into());
        }
        if config.vote == AccountId([0; 20]) {
            return Err("the vote contract is a nonzero address".into());
        }
        if !(1..=NOTICE_MS).contains(&config.restriction_ttl_ms) {
            return Err(format!("restriction_ttl_ms must be in 1..={NOTICE_MS}"));
        }
        self.governance.config = Some(*config);
        Ok(())
    }

    /// Mirrors the vote contract's `ProposalPassed` event: a normal change waits out its notice, and a
    /// fast-path restriction applies at once, unless the rules refuse it, which credits it as refused.
    pub(crate) fn pass_proposal(&mut self, passed: &PassedProposal, events: &mut Vec<Event>) {
        let PassedProposal { proposal, fast, command_hash: hash, ref command, block, ref chain_ref } = *passed;
        let refusal = if self.protocol_version < ProtocolVersion::V56 {
            Some(format!("parameter votes need protocol {:?}", ProtocolVersion::V56))
        } else {
            self.registry_mirror_refusal(chain_ref).or_else(|| {
                if self.governance.config.is_none() {
                    Some("no vote contract is recorded".into())
                } else if command.as_ref().is_some_and(|command| command_hash(command) != hash) {
                    Some(format!("proposal {proposal}'s command does not hash to {hash}"))
                } else {
                    None
                }
            })
        };
        if let Some(reason) = refusal {
            events.push(Event::ElectionRejected { chain_ref: chain_ref.clone(), reason });
            return;
        }
        self.credit_registry_mirror(chain_ref, block);
        let outcome = match command {
            _ if !self.votes_govern() => Err("rotation is off, so the timelock governs".into()),
            None => Err("the proposal's bytes are not an admin command's canonical JSON".into()),
            Some(command) if fast => self.restrict_by_vote(proposal, command, events),
            Some(command) => self.schedule_change(proposal, command, events),
        };
        if let Err(reason) = outcome {
            events.push(Event::ProposalRefused { proposal, reason });
        }
    }

    /// Records a normal change to apply seven days from now.
    /// Error cases:
    /// - a command no normal vote may carry, or `MAX_PENDING_CHANGES` already waiting
    fn schedule_change(
        &mut self,
        proposal: u64,
        command: &AdminCommand,
        events: &mut Vec<Event>,
    ) -> Result<(), String> {
        if !is_governed(command) {
            return Err("a normal vote changes only the governed parameters, listings, toxicity weighting, close-spread ceilings, the access policy or the protocol version".into());
        }
        if self.governance.pending.len() >= MAX_PENDING_CHANGES {
            return Err(format!("{MAX_PENDING_CHANGES} changes already wait out their notice"));
        }
        let applies_at = (self.now + NOTICE_MS).min(crate::types::MAX_STAMP_MS);
        let basis = self.basis_of(command);
        self.governance.pending.push(PendingChange { proposal, command: command.clone(), basis, applies_at });
        events.push(Event::ProposalScheduled { proposal, applies_at });
        Ok(())
    }

    /// Applies the passed normal change `voted` names, once its notice has run; it is spent whether
    /// or not the command then applies.
    /// Error cases:
    /// - before `V56`
    /// - no change waits under the proposal, or it waits with another command
    /// - its notice has not run
    /// - a `SetParams` or `UpsertMarket` whose baseline has moved since it was mirrored, which it would
    ///   otherwise undo, or a ceiling adoption while another waits, which it would replace
    /// - any error case of the command (`Engine::admin`), with the notices the vote's own replaces
    pub(crate) fn apply_proposal(&mut self, voted: &VotedChange, events: &mut Vec<Event>) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V56 {
            return Err(format!("parameter votes need protocol {:?}", ProtocolVersion::V56));
        }
        let at = self
            .governance
            .pending
            .iter()
            .position(|pending| pending.proposal == voted.proposal)
            .ok_or_else(|| format!("no change waits under proposal {}", voted.proposal))?;
        let pending = &self.governance.pending[at];
        if pending.command != voted.command {
            return Err(format!("proposal {} waits with another command", voted.proposal));
        }
        if pending.applies_at > self.now {
            return Err(format!("proposal {} applies from {}", voted.proposal, pending.applies_at));
        }
        let basis = pending.basis;
        self.governance.pending.remove(at);
        if basis != self.basis_of(&voted.command) {
            return Err(format!("what proposal {} replaces changed after it passed; propose it again", voted.proposal));
        }
        if matches!(voted.command, AdminCommand::AdoptCloseSpreadCeiling { .. }) && self.pending_ceiling.is_some() {
            return Err("another close-spread ceiling adoption waits; propose it again once that applies".into());
        }
        self.admin(&voted.command, true, events)
    }

    /// `keccak256` of what `command` replaces, for a `SetParams` or `UpsertMarket`: the baseline
    /// parameters, or the market's baseline spec, `null` while it is unlisted.
    fn basis_of(&self, command: &AdminCommand) -> Option<B256> {
        // Parameters and specs serialise: their fields are maps, lists, strings and integers.
        let hash = |value: serde_json::Result<Vec<u8>>| keccak256(value.expect("the baseline serialises"));
        match command {
            AdminCommand::SetParams(_) => Some(hash(serde_json::to_vec(&self.baseline_params()))),
            AdminCommand::UpsertMarket(spec) => Some(hash(serde_json::to_vec(&self.baseline_spec(spec.id)))),
            _ => None,
        }
    }
}
