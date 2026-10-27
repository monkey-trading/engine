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
//!   skipped and distribute nothing.
//! - **Distribution.** At an epoch's close, the excess is `min(NAV, house cash less every
//!   unfinalised deposit) − floor`: the floor stays as house liquidity and principal still waiting
//!   for Ethereum finality — the house's and each client's — stays locked, exactly as
//!   `house_capital` and a reversal take it back, and unrealised NAV above that cash is not paid.
//!   Below one USDC base unit, when no account is staked, or at or below the floor, nothing
//!   distributes. Each staker receives `floor(excess · stake / total stake)` in USDC base units, so
//!   every allocation is a whole USDC unit the vault pays exactly; the remainder is the house's.
//!   House cash falls by the allocated total. Each account's cumulative distribution is a leaf of
//!   its own in the state tree, which the vault checks a claim against.
//! - **Reconciliation.** Allocations reduce house cash before any USDC leaves the vault; the
//!   vault's cumulative `distributionsPaid` explains the difference at the next checkpoint.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use alloy_primitives::U256;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::Fixed;
use crate::emissions::TokenUnits;
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MAX_STAMP_MS, Millis};

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

/// Staking once initialised: the config, the open epoch, and what closed epochs allocated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staking {
    pub config: StakingConfig,
    /// The epoch stake and NAV accrue to: the one the last command fell in, or 0 before the start.
    pub epoch: u64,
    /// USDC base units allocated to stakers across every closed epoch.
    pub distributed: UsdcUnits,
    /// The highest finalised Ethereum block a credited mirror sequenced: the authenticated
    /// frontier the watcher resumes from, so a host cursor cannot skip an unmirrored event.
    #[serde(default)]
    pub mirrored_through: Option<u64>,
    /// The last fully verified finalized staking block, recorded only after its complete event range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_through: Option<StakingCheckpoint>,
}

/// The authenticated anchor of a completed finalized staking scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StakingCheckpoint {
    pub block: u64,
    pub block_hash: alloy_primitives::B256,
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

    /// `account`'s cumulative USDC distribution in base units.
    pub fn distribution(&self, account: AccountId) -> UsdcUnits {
        self.distributions.get(&account).copied().unwrap_or_default()
    }

    /// Records a fully verified finalized staking range after all its events have applied.
    pub(crate) fn staking_progress(&mut self, block: u64, block_hash: alloy_primitives::B256, events: &mut Vec<Event>) {
        let valid = self.protocol_version >= ProtocolVersion::V27
            && block != u64::MAX
            && block_hash != alloy_primitives::B256::ZERO
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
        self.staking.as_mut().expect("checked above").completed_through = Some(StakingCheckpoint { block, block_hash });
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
        self.staking = Some(Staking {
            config: *config,
            epoch: 0,
            distributed: UsdcUnits(0),
            mirrored_through: None,
            completed_through: None,
        });
        Ok(())
    }

    /// Mirrors a staking contract's `Staked` event: credits `amount` to `account`'s stake under
    /// `chain_ref`, credited once, advancing the mirrored-through frontier to `block`.
    pub(crate) fn stake(
        &mut self,
        account: AccountId,
        amount: TokenUnits,
        block: u64,
        chain_ref: &str,
        events: &mut Vec<Event>,
    ) {
        let total = self.staked.get(&account).copied().unwrap_or_default().0.checked_add(amount.0);
        let reason = if self.staking.is_none() {
            Some(crate::event::Reject::StakingNotInitialised)
        } else if amount.0 == 0 || total.is_none() {
            Some(crate::event::Reject::InvalidAmount)
        } else if self.staking_refs.contains(chain_ref) {
            Some(crate::event::Reject::DuplicateReference)
        } else {
            None
        };
        if let Some(reason) = reason {
            events.push(Event::StakingRejected { account, amount, chain_ref: chain_ref.to_owned(), reason });
            return;
        }
        self.staking_refs.insert(chain_ref.to_owned());
        self.staked.insert(account, TokenUnits(total.expect("checked above")));
        let staking = self.staking.as_mut().expect("initialised above");
        staking.mirrored_through = Some(staking.mirrored_through.map_or(block, |frontier| frontier.max(block)));
        events.push(Event::Staked { account, amount });
    }

    /// Mirrors a staking contract's `UnstakeCompleted` event, which fires only once the unstake
    /// cooldown has passed: removes `amount` from `account`'s stake under `chain_ref`, credited
    /// once, refusing an amount above what the account has staked, and advancing the
    /// mirrored-through frontier to `block`.
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
        staking.mirrored_through = Some(staking.mirrored_through.map_or(block, |frontier| frontier.max(block)));
        events.push(Event::Unstaked { account, amount });
    }

    /// Closes every epoch the sequenced time has ended, before anything else the command does: the
    /// latest one distributes `NAV − floor` pro-rata to the staked snapshot; the ones before it,
    /// whose end no command witnessed, distribute nothing, like emission periods.
    pub(crate) fn finalise_staking(&mut self, events: &mut Vec<Event>) {
        let Some(staking) = &self.staking else { return };
        let (config, open) = (staking.config, staking.epoch);
        let elapsed = config.epochs_elapsed(self.now);
        if elapsed <= open {
            return;
        }
        let closed = elapsed - 1;
        if closed > open {
            events.push(Event::DistributionEpochsSkipped { first: open, last: closed - 1 });
        }

        let nav = self.house_nav();
        // The floor stays as house liquidity, and every principal still able to reverse stays
        // locked, exactly as `house_capital` and a reversal take it back: the house's unfinalised
        // deposits and each client's, whose realised gains are therefore not distributed until the
        // deposit finalises. NAV can exceed that cash while clients' open positions sit
        // underwater; the unrealised excess is not paid either.
        let unfinalised: Fixed = self.accounts.values().map(|acct| acct.unfinalised_deposits).sum();
        let available = Fixed::from_raw(
            self.house_cash
                .raw()
                .saturating_sub(self.unfinalised_house_deposits.raw())
                .saturating_sub(unfinalised.raw()),
        );
        let excess = (nav.min(available) - config.liquidity_floor).max(Fixed::ZERO);
        let usdc_excess = if excess.is_positive() { (excess.raw() / USDC_SCALE) as u128 } else { 0 };
        let mut allocated = 0u128;
        if usdc_excess > 0 && !self.staked.is_empty() {
            let stakes: Vec<(AccountId, u128)> =
                self.staked.iter().map(|(account, staked)| (*account, staked.0)).collect();
            let total: U256 = stakes.iter().map(|(_, staked)| U256::from(*staked)).sum();
            for (account, staked) in stakes {
                // At most `usdc_excess`, as each stake is at most the total.
                let amount: u128 = (U256::from(usdc_excess) * U256::from(staked) / total).to();
                if amount == 0 {
                    continue;
                }
                let distribution = self.distributions.entry(account).or_default();
                distribution.0 = distribution.0.checked_add(amount).expect("a claimable distribution fits a u128");
                allocated += amount;
                events.push(Event::DistributionAllocated { account, epoch: closed, amount: UsdcUnits(amount) });
            }
            // `allocated` is at most `usdc_excess`, so the raw amount fits an `i128`.
            self.house_cash -= Fixed::from_raw((allocated * USDC_SCALE as u128) as i128);
        }
        events.push(Event::DistributionEpochClosed {
            epoch: closed,
            nav,
            floor: config.liquidity_floor,
            excess: UsdcUnits(usdc_excess),
            allocated: UsdcUnits(allocated),
        });
        let staking = self.staking.as_mut().expect("read above");
        staking.epoch = elapsed;
        staking.distributed.0 += allocated;
    }
}

#[cfg(test)]
mod tests;
