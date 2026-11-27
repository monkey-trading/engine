//! MONKEY staking and USDC distributions, from protocol `V25`: holders stake MONKEY in the staking
//! contract on Arbitrum, and each epoch the house NAV above the configured liquidity floor is
//! distributed to stakers pro-rata, paid in USDC from a snapshot at the epoch end.
//!
//! - **Mirroring.** The staking contract custodies MONKEY and enforces the unstake cooldown on
//!   chain. The engine mirrors its finalised `Staked` and `UnstakeCompleted` events as sequenced
//!   commands under their own chain references, credited once, and records the highest finalised
//!   block a credited mirror sequenced: the authenticated frontier a restart resumes from, which a
//!   host-controlled cursor cannot skip past. An unstake applies only once its cooldown has
//!   passed, so stake a snapshot sees persists through the cooldown.
//! - **Epochs** are the half-open windows `[start + n·epoch_ms, start + (n+1)·epoch_ms)` of
//!   sequenced time. The first command at or after an epoch's end closes it, before anything else
//!   the command does, exactly as emission periods finalise; epochs no command witnessed are
//!   skipped and distribute nothing. From `V75` the latest epoch whose end the mirror's completed
//!   frontier has also passed closes, as an election waits for its effective time, while it ended
//!   less than a day before; an older one is skipped too. A stalled mirror delays a close, and the
//!   excess stays with the house for the next close it lets through, but no close pays a stake set
//!   the mirror has not brought to the epoch's end. From `V83` every epoch both have passed closes in
//!   turn, oldest first, however late, and none is skipped, so an epoch the mirror was held past pays
//!   the stake set mirrored when the frontier passed its end.
//! - **Distribution.** At an epoch's close, the excess is `min(NAV, house cash less every
//!   unfinalised deposit) − floor`: the floor stays as house liquidity and principal still waiting
//!   for Ethereum finality — the house's and each client's — stays locked, exactly as
//!   `house_capital` and a reversal take it back, and unrealised NAV above that cash is not paid.
//!   Below one USDC base unit, when no account is staked, or at or below the floor, nothing
//!   distributes. Each staker receives `floor(excess · stake / total stake)` in USDC base units, so
//!   every allocation is a whole USDC unit the vault pays exactly; the remainder is the house's.
//!   From V28, unresolved negative client cash is reserved too: a partial liquidation cannot pay
//!   stakers with an unfunded realised loss while another position cannot close. From `V75` a
//!   client's unfinalised deposit is its own cash, which a reversal takes back first, so for each
//!   account only what its cash, and its marked equity, would fall below zero by were its deposits to
//!   reverse is reserved (`excess`).
//!   House cash falls by the allocated total. Each account's cumulative distribution is a leaf of
//!   its own in the state tree, which the vault checks a claim against.
//! - **The operator's share**, from V47. While rotation is on, `floor(excess / 5)`, from `V89` of what
//!   the awards leave, credits the
//!   operator in effect when the close runs, after any handover due then, and stakers share the
//!   rest as above, so the fraction that rounding leaves goes to them and their dust to the house.
//!   The allocated total never passes the excess. The share is the same whatever the excess holds:
//!   a close that sweeps what built up while nothing was staked pays the operator a fifth of it.
//!   It credits the leaf of the candidate seated under the key in effect (`election`), which is also
//!   that candidate's leaf as a staker, so the vault's claim and exit pay the operator as they pay
//!   anyone. From `V50` the share is split among the operators by their time in office during the
//!   open epoch (`office`), from `V70` a proven fault's haircut withholds part of a member's
//!   (`operator_faults`), which stays with the stakers, and from `V83` the time counts only to the
//!   epoch's end, however late its close runs.
//! - **Awards**, from `V89`. A close that distributes pays the queue of bug bounty awards first, up to the
//!   cap in force, into each recipient's cumulative distribution, and the operator's share and the
//!   stakers' come from what is left (`awards`). The allocated total counts the awards.
//! - **Reconciliation.** Allocations reduce house cash before any USDC leaves the vault; the
//!   vault's cumulative `distributionsPaid` explains the difference at the next checkpoint.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Fixed;
use crate::emissions::TokenUnits;
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MAX_STAMP_MS, Millis};
mod close;
mod excess;
mod office;
pub use office::{Stint, TimeInOffice};

/// One USDC base unit in `Fixed`'s raw units (9 decimals against USDC's 6).
pub(crate) const USDC_SCALE: i128 = 1_000;

/// A cumulative USDC distribution in USDC base units (6 decimals), as the vault pays it. Serializes
/// as a decimal string, so no JSON client loses precision.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct UsdcUnits(pub u128);

/// What the operator's initialisation records once: the liquidity floor, the epoch length, when
/// epoch 0 starts, and the unstake cooldown the staking contract enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StakingConfig {
    /// House NAV at or below this distributes nothing; the floor stays as house liquidity.
    pub liquidity_floor: Fixed,
    /// One epoch's length in sequenced milliseconds.
    pub epoch_ms: Millis,
    /// When epoch 0 starts, in sequenced epoch milliseconds. Never earlier than the
    /// initialisation's own stamp.
    pub start_ms: Millis,
    /// The unstake cooldown the staking contract enforces, in milliseconds, recorded so the log
    /// names the parameter the contract was deployed with.
    pub cooldown_ms: Millis,
}

/// Staking once initialised: the config, the open epoch, what closed epochs allocated, and from `V50`
/// the open epoch's time in office.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staking {
    pub config: StakingConfig,
    /// The epoch stake and NAV accrue to: the one the last command fell in, or 0 before the start.
    /// From `V75` the first epoch no close has passed, which is earlier while a close waits for the
    /// mirror.
    pub epoch: u64,
    /// USDC base units allocated across every closed epoch: to stakers, and from `V47` to the
    /// operator.
    pub distributed: UsdcUnits,
    /// The highest finalised Ethereum block a credited mirror sequenced: the authenticated
    /// frontier the watcher resumes from, so a host cursor cannot skip an unmirrored event.
    #[serde(default)]
    pub mirrored_through: Option<u64>,
    /// The last fully verified finalized staking block, recorded only after its complete event range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_through: Option<StakingCheckpoint>,
    /// From `V50`, the time in office the open epoch's close splits the operator's share by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub office: Option<TimeInOffice>,
    /// From `V90`, the MONKEY staked across every account, in base units: set from the staked balances when
    /// `V90` activates or staking is initialised under it, and kept by every mirrored stake and unstake, so a
    /// carried genesis can check the stakes it carries are all of them (`carry`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staked_total: Option<TokenUnits>,
    /// From `V92`, the block of the staking frontier this log's genesis carried, or for a log that carried state
    /// before `V92`, of its completed frontier at `V92`'s activation: no event mirrored from a block at or below it
    /// applies, nor a completed scan there (`carry`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carried_through: Option<u64>,
}

/// The authenticated anchor of a completed finalized staking scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StakingCheckpoint {
    pub block: u64,
    pub block_hash: alloy_primitives::B256,
    /// From `V37`, the latest finalised block time the scan has reached, in milliseconds: every
    /// registry event stamped before it has been mirrored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<Millis>,
}

impl UsdcUnits {
    /// Whether the amount is zero, which an event leaves out of its encoding.
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for UsdcUnits {
    /// The amount in USDC base units, in decimal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for UsdcUnits {
    type Err = String;

    /// Parses decimal digits only: no sign, no leading zero, nothing past `u128::MAX`.
    /// Error cases:
    /// - empty, a character other than a digit, a leading zero, or a value past `u128::MAX`
    fn from_str(s: &str) -> Result<Self, String> {
        let canonical = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
        canonical
            .then(|| s.parse().ok())
            .flatten()
            .map(UsdcUnits)
            .ok_or_else(|| format!("invalid USDC amount {s:?}: expected base units in decimal digits"))
    }
}

impl Serialize for UsdcUnits {
    /// As a decimal string.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for UsdcUnits {
    /// From a decimal string; see `FromStr`.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
    }
}

impl StakingConfig {
    /// The number of epochs whose end `ts` has reached, 0 before the start.
    pub fn epochs_elapsed(&self, ts: Millis) -> u64 {
        let Some(elapsed) = ts.checked_sub(self.start_ms).filter(|elapsed| *elapsed >= 0) else { return 0 };
        (elapsed / self.epoch_ms) as u64
    }

    /// When `epoch` ends, and the next begins.
    pub fn epoch_end(&self, epoch: u64) -> Millis {
        self.start_ms + (epoch as Millis + 1) * self.epoch_ms
    }

    /// Checks the config against the time it is initialised at.
    /// Error cases:
    /// - a start before `now`, or past the latest stamp the engine accepts
    /// - a non-positive epoch length or cooldown, either past the latest stamp
    /// - a floor that is not a positive valid input
    fn validate(&self, now: Millis) -> Result<(), String> {
        if self.start_ms < now {
            return Err(format!("staking start {} is before the sequenced time {now}", self.start_ms));
        }
        if self.start_ms > MAX_STAMP_MS {
            return Err(format!("staking start {} is past the latest stamp", self.start_ms));
        }
        if self.epoch_ms <= 0 || self.epoch_ms > MAX_STAMP_MS {
            return Err(format!("epoch length {} is outside 1 to {MAX_STAMP_MS}", self.epoch_ms));
        }
        if self.cooldown_ms <= 0 || self.cooldown_ms > MAX_STAMP_MS {
            return Err(format!("unstake cooldown {} is outside 1 to {MAX_STAMP_MS}", self.cooldown_ms));
        }
        if !self.liquidity_floor.is_valid_input() {
            return Err(format!("liquidity floor {} is not a positive valid input", self.liquidity_floor));
        }
        Ok(())
    }
}

impl Engine {
    /// Staking, once initialised.
    pub fn staking(&self) -> Option<&Staking> {
        self.staking.as_ref()
    }

    /// `account`'s staked MONKEY in base units.
    pub fn staked(&self, account: AccountId) -> TokenUnits {
        self.staked.get(&account).copied().unwrap_or_default()
    }

    /// Every account's staked MONKEY in base units.
    pub fn staked_balances(&self) -> &BTreeMap<AccountId, TokenUnits> {
        &self.staked
    }

    /// The sum of every account's staked MONKEY, or `None` past `u128::MAX`.
    pub(crate) fn staked_sum(&self) -> Option<TokenUnits> {
        self.staked.values().try_fold(0u128, |total, staked| total.checked_add(staked.0)).map(TokenUnits)
    }

    /// `account`'s cumulative USDC distribution in base units.
    pub fn distribution(&self, account: AccountId) -> UsdcUnits {
        self.distributions.get(&account).copied().unwrap_or_default()
    }

    /// Records a fully verified finalized staking range after all its events have applied, from
    /// `V37` the block's time, which never moves back, and from `V65` the registry's turn state at that
    /// block: the boundary of the drawn turn naming the operator, which a turn refused before `V65` left
    /// no other record of, or none, which resolves what `V65`'s activation could not see; from `V67` a
    /// drawn turn not yet started, up to a day after the block's time, may be named. From `V92` a block at or
    /// below the carried frontier is refused.
    pub(crate) fn staking_progress(
        &mut self,
        block: u64,
        block_hash: alloy_primitives::B256,
        block_time: Option<Millis>,
        named_turn: Option<Option<Millis>>,
        events: &mut Vec<Event>,
    ) {
        let reached = self.staking.as_ref().and_then(|staking| staking.completed_through).and_then(|c| c.time);
        let valid = self.protocol_version >= ProtocolVersion::V27
            && block != u64::MAX
            && block_hash != alloy_primitives::B256::ZERO
            && block_time.is_none_or(|time| self.protocol_version >= ProtocolVersion::V37 && Some(time) >= reached)
            && named_turn.is_none_or(|turn| {
                self.protocol_version >= ProtocolVersion::V65
                    && turn.is_none_or(|since| self.turn_names_at(since, block_time))
            })
            && self.carried_frontier_covering(block).is_none()
            && self.staking.as_ref().is_some_and(|staking| {
                staking.mirrored_through.is_none_or(|through| block >= through)
                    && staking.completed_through.is_none_or(|through| {
                        block > through.block || (block == through.block && block_hash == through.block_hash)
                    })
            });
        if !valid {
            events.push(Event::AdminRejected { reason: "invalid staking finalized frontier".into() });
            return;
        }
        let time = block_time.or(reached);
        self.staking.as_mut().expect("checked above").completed_through =
            Some(StakingCheckpoint { block, block_hash, time });
        if let Some(turn) = named_turn {
            self.governance.turns_drawn = false;
            if let Some(since) = turn {
                self.governance.turns_from = Some(self.governance.turns_from.map_or(since, |from| from.min(since)));
            }
        }
    }

    /// Whether a staking mirror's chain reference already credited.
    pub fn is_staking_credited(&self, reference: &str) -> bool {
        self.staking_refs.contains(reference)
    }

    /// Records the staking config, once, under `V25` or later.
    /// Error cases:
    /// - before `V25`, or once staking is initialised
    /// - a config that fails its checks at the sequenced time (see `StakingConfig::validate`)
    pub(crate) fn init_staking(&mut self, config: &StakingConfig) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V25 {
            return Err(format!("staking needs protocol {:?}", ProtocolVersion::V25));
        }
        if self.staking.is_some() {
            return Err("staking is already initialised".into());
        }
        config.validate(self.now)?;
        // Nothing is staked before staking is initialised: a mirrored stake is refused until then.
        let staked_total = (self.protocol_version >= ProtocolVersion::V90).then_some(TokenUnits(0));
        self.staking = Some(Staking {
            config: *config,
            epoch: 0,
            distributed: UsdcUnits(0),
            mirrored_through: None,
            completed_through: None,
            office: None,
            staked_total,
            carried_through: None,
        });
        if self.protocol_version >= ProtocolVersion::V50 {
            self.start_time_in_office();
        }
        Ok(())
    }

    /// Mirrors a staking contract's `Staked` event: credits `amount` to `account`'s stake under
    /// `chain_ref`, credited once, advancing the mirrored-through frontier to `block`; from `V92`
    /// refusing a `block` at or below the carried frontier.
    pub(crate) fn stake(
        &mut self,
        account: AccountId,
        amount: TokenUnits,
        block: u64,
        chain_ref: &str,
        events: &mut Vec<Event>,
    ) {
        let total = self.staked.get(&account).copied().unwrap_or_default().0.checked_add(amount.0);
        let staked_total = self.staking.as_ref().and_then(|staking| staking.staked_total);
        let overflows = staked_total.is_some_and(|staked| staked.0.checked_add(amount.0).is_none());
        let reason = if self.staking.is_none() {
            Some(crate::event::Reject::StakingNotInitialised)
        } else if amount.0 == 0 || total.is_none() || overflows {
            Some(crate::event::Reject::InvalidAmount)
        } else if self.staking_refs.contains(chain_ref) {
            Some(crate::event::Reject::DuplicateReference)
        } else {
            self.carried_frontier_covering(block).map(|through| crate::event::Reject::CarriedFrontier { through })
        };
        if let Some(reason) = reason {
            events.push(Event::StakingRejected { account, amount, chain_ref: chain_ref.to_owned(), reason });
            return;
        }
        self.staking_refs.insert(chain_ref.to_owned());
        self.staked.insert(account, TokenUnits(total.expect("checked above")));
        let staking = self.staking.as_mut().expect("initialised above");
        if let Some(staked) = &mut staking.staked_total {
            staked.0 += amount.0;
        }
        staking.mirrored_through = Some(staking.mirrored_through.map_or(block, |frontier| frontier.max(block)));
        events.push(Event::Staked { account, amount });
    }

    /// Mirrors a staking contract's `UnstakeCompleted` event, which fires only once the unstake
    /// cooldown has passed: removes `amount` from `account`'s stake under `chain_ref`, credited
    /// once, refusing an amount above what the account has staked, and from `V92` a `block` at or
    /// below the carried frontier, and advancing the mirrored-through frontier to `block`.
    pub(crate) fn unstake(
        &mut self,
        account: AccountId,
        amount: TokenUnits,
        block: u64,
        chain_ref: &str,
        events: &mut Vec<Event>,
    ) {
        let reason = if self.staking.is_none() {
            Some(crate::event::Reject::StakingNotInitialised)
        } else if amount.0 == 0 {
            Some(crate::event::Reject::InvalidAmount)
        } else if self.staking_refs.contains(chain_ref) {
            Some(crate::event::Reject::DuplicateReference)
        } else if let Some(through) = self.carried_frontier_covering(block) {
            Some(crate::event::Reject::CarriedFrontier { through })
        } else if self.staked.get(&account).is_none_or(|staked| staked.0 < amount.0) {
            Some(crate::event::Reject::InsufficientStake)
        } else {
            None
        };
        if let Some(reason) = reason {
            events.push(Event::StakingRejected { account, amount, chain_ref: chain_ref.to_owned(), reason });
            return;
        }
        self.staking_refs.insert(chain_ref.to_owned());
        let staked = self.staked.get_mut(&account).expect("checked above");
        staked.0 -= amount.0;
        if staked.0 == 0 {
            self.staked.remove(&account);
        }
        let staking = self.staking.as_mut().expect("initialised above");
        if let Some(staked) = &mut staking.staked_total {
            // The total is the sum of the balances, one of which held `amount`.
            staked.0 -= amount.0;
        }
        staking.mirrored_through = Some(staking.mirrored_through.map_or(block, |frontier| frontier.max(block)));
        events.push(Event::Unstaked { account, amount });
    }
}

#[cfg(test)]
mod tests;
