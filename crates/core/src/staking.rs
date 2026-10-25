//! MONKEY staking and USDC distributions, from protocol `V25`: holders stake MONKEY in the staking
//! contract on Arbitrum, and each epoch the house NAV above the configured liquidity floor is
//! distributed to stakers pro-rata, paid in USDC from a snapshot at the epoch end.
//!
//! - **Mirroring.** The staking contract custodies MONKEY and enforces the unstake cooldown on
//!   chain. The engine mirrors its finalised `Staked` and `UnstakeCompleted` events as sequenced
//!   commands under their own chain references, credited once. An unstake applies only once its
//!   cooldown has passed, so stake a snapshot sees persists through the cooldown.
//! - **Epochs** are the half-open windows `[start + n·epoch_ms, start + (n+1)·epoch_ms)` of
//!   sequenced time. The first command at or after an epoch's end closes it, before anything else
//!   the command does, exactly as emission periods finalise; epochs no command witnessed are
//!   skipped and distribute nothing.
//! - **Distribution.** At an epoch's close, `NAV − liquidity_floor` is the excess, or nothing when
//!   NAV is at or below the floor, when no account is staked, or when the excess is below one USDC
//!   base unit; the excess is also capped at the house cash the vault pays from, since NAV can
//!   exceed cash while clients' open positions sit underwater. Each staker receives
//!   `floor(excess · stake / total stake)` in USDC base units, so every allocation is a whole USDC
//!   unit the vault pays exactly; the remainder is the house's. House cash falls by the allocated
//!   total; the dust stays as house equity for the next epoch. Each account's cumulative
//!   distribution is a leaf of its own in the state tree, which the vault checks a claim against.
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
        self.staking = Some(Staking { config: *config, epoch: 0, distributed: UsdcUnits(0) });
        Ok(())
    }

    /// Mirrors a staking contract's `Staked` event: credits `amount` to `account`'s stake under
    /// `chain_ref`, credited once.
    pub(crate) fn stake(&mut self, account: AccountId, amount: TokenUnits, chain_ref: &str, events: &mut Vec<Event>) {
        let total = self.staked.get(&account).copied().unwrap_or_default().0.checked_add(amount.0);
        let reason = if self.staking.is_none() {
            Some(crate::event::Reject::StakingNotInitialised)
        } else if amount.0 == 0 || total.is_none() {
            Some(crate::event::Reject::InvalidAmount)
        } else if self.credited_refs.contains(chain_ref) {
            Some(crate::event::Reject::DuplicateReference)
        } else {
            None
        };
        if let Some(reason) = reason {
            events.push(Event::StakingRejected { account, amount, chain_ref: chain_ref.to_owned(), reason });
            return;
        }
        self.credited_refs.insert(chain_ref.to_owned());
        self.staked.insert(account, TokenUnits(total.expect("checked above")));
        events.push(Event::Staked { account, amount });
    }

    /// Mirrors a staking contract's `UnstakeCompleted` event, which fires only once the unstake
    /// cooldown has passed: removes `amount` from `account`'s stake under `chain_ref`, credited
    /// once, refusing an amount above what the account has staked.
    pub(crate) fn unstake(&mut self, account: AccountId, amount: TokenUnits, chain_ref: &str, events: &mut Vec<Event>) {
        let reason = if self.staking.is_none() {
            Some(crate::event::Reject::StakingNotInitialised)
        } else if amount.0 == 0 {
            Some(crate::event::Reject::InvalidAmount)
        } else if self.credited_refs.contains(chain_ref) {
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
        self.credited_refs.insert(chain_ref.to_owned());
        let staked = self.staked.get_mut(&account).expect("checked above");
        staked.0 -= amount.0;
        if staked.0 == 0 {
            self.staked.remove(&account);
        }
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
        let excess = (nav - config.liquidity_floor).max(Fixed::ZERO);
        // Whole USDC base units above the floor, and never more than the house actually holds: NAV
        // can exceed house cash while clients' open positions sit underwater, and house cash is
        // what the vault pays from.
        let usdc_excess = if excess.is_positive() {
            (excess.raw() / USDC_SCALE).min(self.house_cash.raw() / USDC_SCALE).max(0) as u128
        } else {
            0
        };
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
mod tests {
    use super::*;
    use crate::event::Reject;

    fn engine(now: Millis) -> Engine {
        Engine { protocol_version: ProtocolVersion::V25, now, ..Engine::default() }
    }

    fn config(now: Millis) -> StakingConfig {
        StakingConfig {
            liquidity_floor: Fixed::from_int(100),
            epoch_ms: 600_000,
            start_ms: now,
            cooldown_ms: 86_400_000,
        }
    }

    #[test]
    fn initialisation_records_the_config_once_under_v25() {
        let mut engine = engine(1_000);
        engine.init_staking(&config(1_000)).expect("initialises");
        assert_eq!(engine.staking().unwrap().epoch, 0);
        assert_eq!(engine.init_staking(&config(1_000)), Err("staking is already initialised".into()));
    }

    #[test]
    fn initialisation_needs_v25_and_a_config_valid_at_the_sequenced_time() {
        let mut before = Engine { protocol_version: ProtocolVersion::V21, now: 1_000, ..Engine::default() };
        assert_eq!(before.init_staking(&config(1_000)), Err("staking needs protocol V25".into()));

        let mut engine = engine(1_000);
        let late = StakingConfig { start_ms: 999, ..config(1_000) };
        assert!(engine.init_staking(&late).unwrap_err().contains("before the sequenced time"));
        let zero_epoch = StakingConfig { epoch_ms: 0, ..config(1_000) };
        assert!(engine.init_staking(&zero_epoch).unwrap_err().contains("epoch length"));
        let zero_floor = StakingConfig { liquidity_floor: Fixed::ZERO, ..config(1_000) };
        assert!(engine.init_staking(&zero_floor).unwrap_err().contains("liquidity floor"));
    }

    #[test]
    fn stake_and_unstake_mirror_events_once_with_balance_checks() {
        let mut engine = engine(1_000);
        engine.init_staking(&config(1_000)).unwrap();
        let mut events = Vec::new();
        engine.stake(AccountId([1; 20]), TokenUnits(500), "0xstake:0", &mut events);
        engine.stake(AccountId([1; 20]), TokenUnits(300), "0xstake:1", &mut events);
        assert_eq!(engine.staked(AccountId([1; 20])), TokenUnits(800));
        engine.stake(AccountId([1; 20]), TokenUnits(1), "0xstake:0", &mut events);
        assert!(matches!(events.last(), Some(Event::StakingRejected { reason: Reject::DuplicateReference, .. })));
        engine.unstake(AccountId([1; 20]), TokenUnits(900), "0xunstake:0", &mut events);
        assert!(matches!(events.last(), Some(Event::StakingRejected { reason: Reject::InsufficientStake, .. })));
        engine.unstake(AccountId([1; 20]), TokenUnits(800), "0xunstake:0", &mut events);
        assert_eq!(engine.staked(AccountId([1; 20])), TokenUnits(0));
        assert!(!engine.staked_balances().contains_key(&AccountId([1; 20])));
    }

    #[test]
    fn stake_and_unstake_refuse_until_initialised() {
        let mut engine = engine(1_000);
        let mut events = Vec::new();
        engine.stake(AccountId([1; 20]), TokenUnits(500), "0xstake:0", &mut events);
        assert!(matches!(events.last(), Some(Event::StakingRejected { reason: Reject::StakingNotInitialised, .. })));
        engine.unstake(AccountId([1; 20]), TokenUnits(500), "0xunstake:0", &mut events);
        assert!(matches!(events.last(), Some(Event::StakingRejected { reason: Reject::StakingNotInitialised, .. })));
    }

    #[test]
    fn an_epoch_above_the_floor_pays_stakers_pro_rata_and_reduces_house_cash() {
        let mut engine = engine(1_000);
        engine.init_staking(&config(1_000)).unwrap();
        engine.house_cash = Fixed::from_int(1_000);
        let mut events = Vec::new();
        engine.stake(AccountId([1; 20]), TokenUnits(1), "0xa:0", &mut events);
        engine.stake(AccountId([2; 20]), TokenUnits(3), "0xa:1", &mut events);
        engine.now = config(1_000).epoch_end(0);
        engine.finalise_staking(&mut events);
        // Excess 900 USDC splits 25/75; house cash falls by exactly the allocated total.
        assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(225_000_000));
        assert_eq!(engine.distribution(AccountId([2; 20])), UsdcUnits(675_000_000));
        assert_eq!(engine.house_cash, Fixed::from_int(1_000) - Fixed::from_int(900));
        assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { epoch: 0, allocated, excess, .. }
            if *allocated == UsdcUnits(900_000_000) && *excess == UsdcUnits(900_000_000))));
        assert_eq!(engine.staking().unwrap().epoch, 1);
    }

    #[test]
    fn an_epoch_at_or_below_the_floor_pays_nothing_and_keeps_house_cash() {
        let mut engine = engine(1_000);
        engine.init_staking(&config(1_000)).unwrap();
        engine.house_cash = Fixed::from_int(100);
        let mut events = Vec::new();
        engine.stake(AccountId([1; 20]), TokenUnits(1), "0xa:0", &mut events);
        engine.now = config(1_000).epoch_end(0);
        engine.finalise_staking(&mut events);
        assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(0));
        assert_eq!(engine.house_cash, Fixed::from_int(100));
        assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { epoch: 0, excess, allocated, .. }
            if *excess == UsdcUnits(0) && *allocated == UsdcUnits(0))));
    }

    #[test]
    fn an_epoch_with_no_stakers_pays_nothing_and_keeps_house_cash() {
        let mut engine = engine(1_000);
        engine.init_staking(&config(1_000)).unwrap();
        engine.house_cash = Fixed::from_int(1_000);
        engine.now = config(1_000).epoch_end(0);
        let mut events = Vec::new();
        engine.finalise_staking(&mut events);
        assert_eq!(engine.house_cash, Fixed::from_int(1_000));
        assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { excess, allocated, .. }
            if *excess == UsdcUnits(900_000_000) && *allocated == UsdcUnits(0))));
    }

    #[test]
    fn unwitnessed_epochs_are_skipped_and_only_the_latest_closes() {
        let mut engine = engine(1_000);
        engine.init_staking(&config(1_000)).unwrap();
        engine.house_cash = Fixed::from_int(1_000);
        let mut events = Vec::new();
        engine.stake(AccountId([1; 20]), TokenUnits(1), "0xa:0", &mut events);
        // Jump straight past the ends of epochs 0, 1, and 2.
        engine.now = config(1_000).epoch_end(2);
        engine.finalise_staking(&mut events);
        assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochsSkipped { first: 0, last: 1 })));
        assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { epoch: 2, .. })));
        // One distribution, for the latest epoch only, not one per elapsed epoch.
        assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(900_000_000));
        assert_eq!(engine.house_cash, Fixed::from_int(100));
        assert_eq!(engine.staking().unwrap().epoch, 3);
    }

    #[test]
    fn sub_usdc_excess_allocates_nothing() {
        let mut engine = engine(1_000);
        engine.init_staking(&config(1_000)).unwrap();
        engine.house_cash = Fixed::from_int(100) + Fixed::from_raw(999);
        let mut events = Vec::new();
        engine.stake(AccountId([1; 20]), TokenUnits(1), "0xa:0", &mut events);
        engine.now = config(1_000).epoch_end(0);
        engine.finalise_staking(&mut events);
        // The excess is 999 raw: less than one USDC base unit, so nothing is allocated.
        assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(0));
        assert_eq!(engine.house_cash, Fixed::from_int(100) + Fixed::from_raw(999));
    }

    #[test]
    fn per_staker_flooring_leaves_the_dust_with_the_house() {
        let mut engine = engine(1_000);
        engine.init_staking(&config(1_000)).unwrap();
        // 1,000.002999 in the house: a 900.002999 excess, 900_000_002 whole USDC units.
        engine.house_cash = Fixed::from_int(1_000) + Fixed::from_raw(2_999);
        let mut events = Vec::new();
        engine.stake(AccountId([1; 20]), TokenUnits(1), "0:a", &mut events);
        engine.stake(AccountId([2; 20]), TokenUnits(2), "0:b", &mut events);
        engine.now = config(1_000).epoch_end(0);
        engine.finalise_staking(&mut events);
        // A third and two thirds of 900_000_002 floor to 300_000_000 and 600_000_001; the last unit
        // stays with the house.
        assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(300_000_000));
        assert_eq!(engine.distribution(AccountId([2; 20])), UsdcUnits(600_000_001));
        assert_eq!(engine.house_cash(), Fixed::from_int(100) + Fixed::from_raw(1_999));
    }

    #[test]
    fn distribution_never_exceeds_house_cash() {
        let mut engine = engine(1_000);
        engine.init_staking(&config(1_000)).unwrap();
        // A client deep underwater on an open position: NAV counts the unrealised loss as house
        // gain, far above the house cash the vault pays from.
        let spec: crate::market::MarketSpec = serde_json::from_value(serde_json::json!({
            "id": 1, "symbol": "BIG", "asset_class": "crypto", "schedule": { "kind": "always_open" },
            "margin_tiers": [{ "bps": 1_000 }], "spread_tiers": [{ "bps": 0 }],
            "price_rules": { "max_age_ms": 1_000, "max_divergence_bps": 50, "min_sources": 2 },
            "max_slippage_bps": 20, "house_cap_bps": 5_000,
            "financing": { "benchmark": "SOFR", "markup_bps": 0, "day_basis": 365 },
        }))
        .expect("a valid listing");
        let mut market = crate::engine::MarketState::new(spec);
        market.mark = Some(Fixed::from_int(1));
        market.client_qty = Fixed::ONE;
        market.client_cost = Fixed::from_int(10_000);
        engine.markets.insert(1, market);
        engine.house_cash = Fixed::from_int(1_000);
        engine.staked.insert(AccountId([1; 20]), TokenUnits(1));
        assert!(engine.house_nav() > engine.house_cash + Fixed::from_int(9_000));

        let mut events = Vec::new();
        engine.now = config(1_000).epoch_end(0);
        engine.finalise_staking(&mut events);
        // Only the whole USDC units house cash actually holds are allocated.
        assert_eq!(engine.distribution(AccountId([1; 20])), UsdcUnits(1_000_000_000));
        assert_eq!(engine.house_cash, Fixed::ZERO);
        assert!(events.iter().any(|e| matches!(e, Event::DistributionEpochClosed { excess, allocated, .. }
            if *excess == UsdcUnits(1_000_000_000) && *allocated == UsdcUnits(1_000_000_000))));
    }

    #[test]
    fn a_stake_sum_past_u128_is_refused_without_mutation() {
        let mut engine = engine(1_000);
        engine.init_staking(&config(1_000)).unwrap();
        let mut events = Vec::new();
        let account = AccountId([1; 20]);
        engine.stake(account, TokenUnits(u128::MAX - 1), "0xmax:0", &mut events);
        engine.stake(account, TokenUnits(2), "0xmax:1", &mut events);
        assert!(matches!(events.last(), Some(Event::StakingRejected { reason: Reject::InvalidAmount, .. })));
        assert_eq!(engine.staked(account), TokenUnits(u128::MAX - 1));
        assert!(!engine.is_credited("0xmax:1"));
    }
}
