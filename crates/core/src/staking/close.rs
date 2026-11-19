//! Closing distribution epochs: when an epoch closes, and how its excess is distributed.

use alloy_primitives::U256;

use crate::Fixed;
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;
use crate::staking::{StakingConfig, USDC_SCALE, UsdcUnits};
use crate::types::{AccountId, Millis};

/// From `V47`, what the excess is divided by for the operator's share while rotation is on: a
/// fifth, 20%. A constant, not a parameter, so the operator it pays cannot change it.
const OPERATOR_SHARE_DIVISOR: u128 = 5;
/// From `V75`, how long after its end an epoch can still close: a day. A mirror that has not passed an
/// epoch's end a day after it has stalled rather than lagged, and with daily turns a day is one member's
/// turn, after which the next member's node mirrors.
const CLOSE_HORIZON_MS: Millis = 24 * 60 * 60 * 1000;

impl Engine {
    /// Closes every epoch the sequenced time has ended, before anything else the command does: the
    /// latest one distributes `NAV − floor` pro-rata to the staked snapshot, less from `V47` the
    /// operator's share while rotation is on, and from `V50` the share its time in office earned;
    /// the ones before it, whose end no command witnessed, distribute nothing, like emission periods.
    /// From `V75` the latest epoch closes that the staking mirror's completed frontier has passed too,
    /// within `CLOSE_HORIZON_MS` of its end (`mirrored_close`). From `V83` every epoch whose end both
    /// have passed closes in turn, oldest first, and none is skipped.
    pub(crate) fn finalise_staking(&mut self, events: &mut Vec<Event>) {
        let Some(staking) = &self.staking else { return };
        let (config, open) = (staking.config, staking.epoch);
        let elapsed = config.epochs_elapsed(self.now);
        if elapsed <= open {
            return;
        }
        if self.protocol_version >= ProtocolVersion::V83 {
            // At most the epochs between the last close and the earlier of the two clocks. A close
            // changes no account, so the accounts are walked once for them all, and not at all while the
            // mirror holds every close back.
            let mut reserves = None;
            while let Some(closed) =
                self.staking.as_ref().and_then(|staking| self.mirrored_close(&config, staking.epoch))
            {
                let reserves = *reserves.get_or_insert_with(|| self.account_reserves());
                self.close_epoch(&config, closed, reserves, events);
            }
            return;
        }
        let closed = if self.protocol_version >= ProtocolVersion::V75 {
            let Some(closed) = self.mirrored_close(&config, open) else { return };
            closed
        } else {
            elapsed - 1
        };
        if closed > open {
            events.push(Event::DistributionEpochsSkipped { first: open, last: closed - 1 });
        }
        self.close_epoch(&config, closed, self.account_reserves(), events);
    }

    /// Closes epoch `closed`, distributing the excess as `finalise_staking` describes, with the accounts'
    /// part of it `reserves` (`account_reserves`), and opens the next.
    fn close_epoch(
        &mut self,
        config: &StakingConfig,
        closed: u64,
        reserves: (i128, i128, i128),
        events: &mut Vec<Event>,
    ) {
        let nav = self.house_nav();
        let excess = self.excess_with(config.liquidity_floor, reserves);
        let usdc_excess = if excess.is_positive() { (excess.raw() / USDC_SCALE) as u128 } else { 0 };
        let mut allocated = 0u128;
        if usdc_excess > 0 && !self.staked.is_empty() {
            // At most the share, a fifth of the excess.
            allocated = self.credit_operator_shares(usdc_excess / OPERATOR_SHARE_DIVISOR, closed, events);
            let to_stakers = usdc_excess - allocated;
            let stakes: Vec<(AccountId, u128)> =
                self.staked.iter().map(|(account, staked)| (*account, staked.0)).collect();
            let total: U256 = stakes.iter().map(|(_, staked)| U256::from(*staked)).sum();
            for (account, staked) in stakes {
                // At most `to_stakers`, as each stake is at most the total.
                let amount: u128 = (U256::from(to_stakers) * U256::from(staked) / total).to();
                if amount == 0 {
                    continue;
                }
                self.credit_distribution(account, amount);
                allocated += amount;
                events.push(Event::DistributionAllocated { account, epoch: closed, amount: UsdcUnits(amount) });
            }
            // `allocated` is at most `usdc_excess`, so the raw amount fits an `i128`.
            let amount = Fixed::from_raw((allocated * USDC_SCALE as u128) as i128);
            self.house_cash -= amount;
            self.note_house_capital(-amount);
            if self.protocol_version >= ProtocolVersion::V31 {
                *self.withdrawal_cash.as_mut().expect("V31 requires V22 capacity") -= amount;
            }
        }
        events.push(Event::DistributionEpochClosed {
            epoch: closed,
            nav,
            floor: config.liquidity_floor,
            excess: UsdcUnits(usdc_excess),
            allocated: UsdcUnits(allocated),
        });
        let staking = self.staking.as_mut().expect("read above");
        staking.epoch = closed + 1;
        staking.distributed.0 += allocated;
        if self.protocol_version >= ProtocolVersion::V83 {
            self.carry_time_in_office(config.epoch_end(closed));
        } else if self.protocol_version >= ProtocolVersion::V50 {
            self.start_time_in_office();
        }
    }
}

impl Engine {
    /// From `V75`, the epoch a command closes, if any: the latest whose end both the sequenced time and
    /// the staking mirror's completed frontier have passed, as an election waits for its effective time,
    /// if `open` has not closed it and it ended less than `CLOSE_HORIZON_MS` ago. A stake set the mirror
    /// has not brought to an epoch's end is never paid; an epoch older than the horizon is skipped by
    /// the close that comes, which pays the excess it would have. A frontier that advances steadily
    /// passes each end in turn, so a mirror lagging by more than an epoch still closes every epoch. A
    /// close pays the stake mirrored when it runs: where the frontier lags, as it usually does, the first
    /// completed scan past the end, so a stake or unstake after the end within that scan counts; a
    /// frontier recorded ahead of the sequenced time lets later scans in before the close too.
    fn mirrored_close(&self, config: &StakingConfig, open: u64) -> Option<u64> {
        self.mirrored_close_at(config, open, self.now)
    }

    /// `mirrored_close` for a command stamped `now`. From `V83` it is `open` itself once both have passed
    /// its end, however late, so each held epoch closes in turn, at the stake set mirrored when the
    /// frontier passed its end, and none is skipped.
    fn mirrored_close_at(&self, config: &StakingConfig, open: u64, now: Millis) -> Option<u64> {
        let reached = self.finalised_chain_time()?;
        let passed = config.epochs_elapsed(reached.min(now));
        if self.protocol_version >= ProtocolVersion::V83 {
            return (passed > open).then_some(open);
        }
        let closed = passed.checked_sub(1).filter(|closed| *closed >= open)?;
        (config.epoch_end(closed).saturating_add(CLOSE_HORIZON_MS) > now).then_some(closed)
    }

    /// Whether a command stamped `ts` would close a distribution epoch (`finalise_staking`).
    pub fn staking_close_due(&self, ts: Millis) -> bool {
        let Some(staking) = &self.staking else { return false };
        let (config, open) = (staking.config, staking.epoch);
        if config.epochs_elapsed(ts) <= open {
            return false;
        }
        self.protocol_version < ProtocolVersion::V75 || self.mirrored_close_at(&config, open, ts).is_some()
    }

    /// The operator's share of a close, `share` USDC base units, by recipient, with what a proven fault's
    /// haircut withheld from each part: from `V50` split by the open epoch's time in office, and from
    /// `V47` all to the account in office while rotation is on.
    pub(super) fn operator_shares(&self, share: u128) -> Vec<(AccountId, u128, u128)> {
        if self.protocol_version >= ProtocolVersion::V50 {
            return self.split_by_time_in_office(share);
        }
        self.operator_share_account().filter(|_| share > 0).map(|account| (account, share, 0)).into_iter().collect()
    }

    /// Adds `amount` to `account`'s cumulative distribution.
    pub(super) fn credit_distribution(&mut self, account: AccountId, amount: u128) {
        let distribution = self.distributions.entry(account).or_default();
        distribution.0 = distribution.0.checked_add(amount).expect("a claimable distribution fits a u128");
    }
}
