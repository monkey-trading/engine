//! Recording: what each applied command adds to the series, the activity and the snapshot.

use std::collections::BTreeMap;

use engine_core::event::{Event, Fill};
use engine_core::fixed::Rounding;
use engine_core::types::Millis;
use engine_core::{Engine, Fixed};

use crate::stats::activity::{Entry, Kind, id};
use crate::stats::series::Metric;
use crate::stats::snapshot::{self, MarketBook};
use crate::stats::{Begin, Distribution, Inner, Settings, Stats};

/// One MONKEY base unit count as a series value.
fn units(amount: u128) -> i128 {
    i128::try_from(amount).unwrap_or(i128::MAX)
}

/// The USD notional of a fill, or `None` for a market whose currency has no FX index.
fn fill_usd(engine: &Engine, fill: &Fill) -> Option<Fixed> {
    let market = engine.markets().get(&fill.market)?;
    let fx = fill.fx_index.or(market.fx_mark);
    (market.spec.quote_currency.is_usd() || fx.is_some()).then(|| {
        market.spec.quote_currency.to_usd(fill.qty.abs().mul_rounded(fill.price, Rounding::Down), fx, Rounding::Down)
    })
}

impl Stats {
    /// Records command `ts` just applied to `engine`, which produced `events`.
    pub(crate) fn observe(&self, engine: &Engine, ts: Millis, events: &[Event]) {
        self.lock().observe(&self.settings, engine, ts, events);
    }
}

impl Inner {
    fn observe(&mut self, settings: &Settings, engine: &Engine, ts: Millis, events: &[Event]) {
        let seq = engine.last_seq().unwrap_or(0);
        let first = self.begin.is_none();
        if first {
            self.begin = Some(Begin { ts, seq });
            self.next_scan = Some(ts.saturating_add(settings.interval_ms));
        }
        let mut stake_moved = false;
        let queued = engine.withdrawal_cash().is_some();
        // Allocations of the period the command closes: MONKEY base units, and the weight they share.
        let mut periods: BTreeMap<u64, (i128, i128)> = BTreeMap::new();
        for (index, event) in events.iter().enumerate() {
            match event {
                Event::Fill(fill) => {
                    if let Some(usd) = fill_usd(engine, fill) {
                        self.series.add(Metric::Volume, Some(fill.market), ts, usd.raw());
                        self.series.add(Metric::Volume, None, ts, usd.raw());
                    }
                }
                Event::WithdrawalQueued { amount, .. } => self.queue_total += *amount,
                // From V22 every approval takes its amount from the queue.
                Event::WithdrawalApproved { amount, .. } if queued => self.queue_total -= *amount,
                Event::Staked { account, amount } | Event::Unstaked { account, amount } => {
                    let staked = matches!(event, Event::Staked { .. });
                    stake_moved = true;
                    self.staked = if staked {
                        self.staked.saturating_add(amount.0)
                    } else {
                        self.staked.saturating_sub(amount.0)
                    };
                    let kind = if staked { Kind::Stake } else { Kind::Unstake };
                    self.activity.push(Entry {
                        id: id(seq, index),
                        seq,
                        ts,
                        kind,
                        account: *account,
                        amount: *amount,
                        period: None,
                    });
                }
                Event::EmissionAllocated { account, period, weight, amount } => {
                    let sums = periods.entry(*period).or_default();
                    sums.0 = sums.0.saturating_add(units(amount.0));
                    sums.1 = sums.1.saturating_add(weight.raw());
                    self.activity.push(Entry {
                        id: id(seq, index),
                        seq,
                        ts,
                        kind: Kind::LossClose,
                        account: *account,
                        amount: *amount,
                        period: Some(*period),
                    });
                }
                Event::EmissionPeriodClosed { period, .. } => {
                    if let Some((amount, weight)) = periods.remove(period) {
                        let ended = engine.emissions().map_or(ts, |e| e.schedule.period_end(*period));
                        self.series.ratio(Metric::MonkeyPerLoss, ended, amount, weight);
                    }
                }
                Event::DistributionEpochClosed { epoch, nav, floor, excess, allocated } => {
                    let closed_ms = engine.staking().map_or(ts, |s| s.config.epoch_end(*epoch));
                    self.series.add(Metric::DistributedPerEpoch, None, closed_ms, units(allocated.0));
                    self.distribution = Some(Distribution {
                        epoch: *epoch,
                        closed_ms,
                        nav: *nav,
                        floor: *floor,
                        excess: excess.0,
                        allocated: allocated.0,
                    });
                }
                _ => {}
            }
        }
        // The first command seeds the totals from the state it left, whatever its events moved.
        if first {
            self.refresh_queue(engine);
        }
        if first {
            self.refresh_stakers(engine);
        } else if stake_moved {
            // Accounts leave the map when they unstake everything, so its size is the stakers.
            self.stakers = engine.staked_balances().len() as u64;
        }
        self.sample_levels(engine, ts);
        if self.next_scan.is_some_and(|due| ts >= due) {
            self.next_scan = Some(ts.saturating_add(settings.interval_ms));
            let snapshot = snapshot::scan(engine, seq, ts, settings.layout, !self.recovering);
            for (market, book) in &snapshot.markets {
                self.sample_book(*market, book, ts);
            }
            self.snapshot = Some(snapshot);
        }
    }

    /// Re-reads the withdrawal queue's total from the engine.
    pub(super) fn refresh_queue(&mut self, engine: &Engine) {
        self.queue_total = engine.withdrawal_queue().map(|request| request.remaining).sum();
    }

    /// Re-reads the total staked and the stakers from the engine.
    pub(super) fn refresh_stakers(&mut self, engine: &Engine) {
        let balances = engine.staked_balances();
        self.staked = balances.values().map(|staked| staked.0).fold(0u128, u128::saturating_add);
        self.stakers = balances.len() as u64;
    }

    /// Samples the levels every command leaves.
    fn sample_levels(&mut self, engine: &Engine, ts: Millis) {
        self.series.level(Metric::Nav, None, ts, engine.house_nav().raw());
        self.series.level(Metric::Cash, None, ts, engine.house_cash().raw());
        self.series.level(Metric::WithdrawalQueue, None, ts, self.queue_total.raw());
        if let Some(emissions) = engine.emissions() {
            self.series.level(Metric::TotalAllocated, None, ts, units(emissions.entitled.0));
        }
        if let Some(staking) = engine.staking() {
            self.series.level(Metric::TotalStaked, None, ts, units(self.staked));
            self.series.level(Metric::CumulativeDistributed, None, ts, units(staking.distributed.0));
        }
    }

    /// Samples a market's long and short from a scan.
    fn sample_book(&mut self, market: u32, book: &MarketBook, ts: Millis) {
        self.series.level(Metric::Long, Some(market), ts, book.long.usd.raw());
        self.series.level(Metric::Short, Some(market), ts, book.short.usd.raw());
    }
}
