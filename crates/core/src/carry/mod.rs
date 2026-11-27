//! A carried genesis, from `V90`: a new log's genesis takes, from another log's checkpoint root, each account's
//! MONKEY entitlement and staked MONKEY, the staking mirror's frontier, the emission schedule and every bug bounty
//! award, each proven against that root, so MONKEY, its stakes and the awards outlive a move to a new vault.
//!
//! - **Parts.** The node alone sequences `AdminCommand::CarryState`, in genesis, as parts in a fixed order, each
//!   the command after the last: `Begin`; the entitlements, then the stakes, in batches of at most
//!   `MAX_CARRY_ENTRIES` in strictly increasing account order; the awards from id 0 in order, the last batch with
//!   the proof that the source holds no award at the next id; then `End`. A refused part changes nothing, and a
//!   node refuses to serve a log whose genesis holds one.
//! - **The source.** `Begin` names the source log's chain, vault, seq and root, with the source's remaining-state
//!   leaf as the JSON its value hashes and that leaf's proof. The leaf must record that chain and vault as its
//!   client auth and that seq as its last, be at `V90` or later with emissions and staking initialised, hold a
//!   staking mirror whose completed frontier reaches every event it mirrored, and have rotation's switch off.
//!   `Begin` applies only once staking is initialised under `V90` and before anything is credited, staked,
//!   entitled or awarded.
//! - **Values.** Each entitlement and stake is the leaf the source root holds for its account, and each award
//!   the record it holds under its id. A carried award owes its whole amount: the source's payments are not
//!   carried, and neither are distributions, balances, positions, orders, approvals, deposits or the award cap.
//! - **Completeness.** `End` checks that the entitlements sum to the source's total entitled, the stakes to its
//!   total staked, and that the awards run from id 0 to the first the source holds none at. It then takes the
//!   source's emission schedule, open period and totals, and its staking mirror's frontier on top of the staking
//!   this log initialised, and records the source in the remaining-state leaf.
//! - **Emissions** continue from the source's start and halvings. The first command stamped past the carried
//!   period's end closes it with no weight, as the source's results there are not carried, and cancels every
//!   later period up to its own, so no period before the new log's first command after `End` allocates anything.
//! - **The frontier**, from `V92`. `End` records the carried staking frontier's block, and an activation of `V92`
//!   records a log's completed frontier if its genesis carried state before. The source mirrored every event of the
//!   contracts it followed at or below it, under references this log does not hold, so a staking, registry,
//!   schedule, vote or fault proof event mirrored from a block at or below it is refused, as is a completed scan
//!   there (`frontier`).

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

use crate::awards::Award;
use crate::emissions::{Emissions, TokenUnits};
use crate::engine::Engine;
use crate::event::Event;
use crate::merkle::Witness;
use crate::protocol::ProtocolVersion;
use crate::staking::StakingCheckpoint;
use crate::types::AccountId;

mod apply;
mod build;
mod frontier;

/// The most entries one entitlement, stake or award part carries, so each command's proofs stay bounded.
pub const MAX_CARRY_ENTRIES: usize = 128;

/// The checkpoint a carried genesis carries from: the source log's custody chain and vault, which its client
/// auth records, and the seq and state root of its checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarrySource {
    pub chain_id: u64,
    pub vault: AccountId,
    pub seq: u64,
    pub root: B256,
}

/// One part of a carried genesis, as `AdminCommand::CarryState` carries it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "part", rename_all = "snake_case", deny_unknown_fields)]
pub enum CarryPart {
    /// First: the source, and its remaining-state leaf as the JSON its value hashes, with the leaf's siblings.
    Begin { source: CarrySource, globals: String, siblings: Vec<B256> },
    /// Entitlements, in strictly increasing account order across every such part.
    Entitlements { entries: Vec<CarriedBalance> },
    /// Stakes, after every entitlement, in strictly increasing account order across every such part.
    Stakes { entries: Vec<CarriedBalance> },
    /// Awards by id from 0 in order, after every stake. The last such part proves the source holds no award
    /// at the next id.
    Awards {
        awards: Vec<CarriedAward>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<AbsenceProof>,
    },
    /// Last: checks every total, then takes the source's emissions and staking frontier.
    End,
}

/// An account's entitlement or stake in base units, with its leaf's siblings in the source root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarriedBalance {
    pub account: AccountId,
    pub amount: TokenUnits,
    pub siblings: Vec<B256>,
}

/// An award as the source holds it under `id`, with its leaf's siblings in the source root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarriedAward {
    pub id: u64,
    pub award: Award,
    pub siblings: Vec<B256>,
}

/// The proof that a key has no leaf: the place it would take is empty, or holds `other`, a leaf of another key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbsenceProof {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other: Option<Witness>,
    pub siblings: Vec<B256>,
}

/// `StateCarried`: a carried genesis applied in full, with what it carried: the accounts holding an entitlement
/// or a stake, the totals entitled and staked, and the number of awards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateCarried {
    pub source: CarrySource,
    pub accounts: u64,
    pub entitled: TokenUnits,
    pub staked: TokenUnits,
    pub awards: u64,
}

/// Where a carried genesis has reached between `Begin` and `End`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CarryStage {
    Begun,
    Entitlements,
    Stakes,
    Awards,
    /// The awards' last part proved the next id empty; only `End` may follow.
    AwardsClosed,
}

/// A carried genesis between `Begin` and `End`: the source, what of it `End` takes and checks against, and what
/// the parts have carried so far. Held in the remaining-state leaf until `End`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarryProgress {
    pub source: CarrySource,
    /// The seq of the last part applied; the next part is the command after it.
    pub seq: u64,
    /// The source's emissions, whose total entitled the entitlements must sum to.
    pub emissions: Emissions,
    /// The source's staking mirror frontier.
    pub mirrored_through: Option<u64>,
    pub completed_through: StakingCheckpoint,
    /// The source's total staked, which the stakes must sum to.
    pub staked_total: TokenUnits,
    pub stage: CarryStage,
    /// The last account the current stage's parts carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_account: Option<AccountId>,
    /// What the entitlement and stake parts have carried so far.
    pub entitled: TokenUnits,
    pub staked: TokenUnits,
    /// The id the next award part starts at.
    pub next_award: u64,
}

impl Engine {
    /// The checkpoint this log's genesis carried from, once its carried genesis has applied in full.
    pub fn carried_from(&self) -> Option<CarrySource> {
        self.carried_from
    }

    /// A carried genesis between its `Begin` and `End`.
    pub fn carry_progress(&self) -> Option<&CarryProgress> {
        self.carry.as_ref()
    }

    /// Applies one part of a carried genesis (see the module docs), which only the node sequences.
    /// Error cases:
    /// - before `V90`, or under a passed vote
    /// - any error case of the part's own check (`Engine::carry_begin` and the others in `carry::apply`)
    pub(crate) fn carry_state(&mut self, part: &CarryPart, voted: bool, events: &mut Vec<Event>) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V90 {
            return Err(format!("a carried genesis needs protocol {:?}", ProtocolVersion::V90));
        }
        if voted {
            return Err("only the node carries state into a genesis".into());
        }
        match part {
            CarryPart::Begin { source, globals, siblings } => self.carry_begin(source, globals, siblings),
            CarryPart::Entitlements { entries } => self.carry_entitlements(entries),
            CarryPart::Stakes { entries } => self.carry_stakes(entries),
            CarryPart::Awards { awards, next } => self.carry_awards(awards, next.as_ref()),
            CarryPart::End => self.carry_end(events),
        }
    }
}

#[cfg(test)]
mod tests;
