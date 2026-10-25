//! Cash in and out of the engine: deposits, withdrawals, house capital, and overnight financing.
//!
//! Cash is conserved: client balances plus house cash always equal deposits plus house capital,
//! less approved withdrawals. Every movement below debits one side and credits the other.

use std::collections::BTreeMap;

use crate::Fixed;
use crate::account::Position;
use crate::auth::RequestAuth;
use crate::deposits::legacy_chain_reference;
use crate::eip712::withdrawal_hash;
use crate::engine::Engine;
use crate::event::{Event, Reject};
use crate::fixed::Rounding;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId};

impl Engine {
    /// Credits legacy or mock cash once; receipt-finality rules refuse the legacy EVM route.
    pub(crate) fn deposit(&mut self, account: AccountId, amount: Fixed, chain_ref: &str, events: &mut Vec<Event>) {
        let reason = if !amount.is_valid_input() {
            Some(Reject::InvalidAmount)
        } else if self.deposit_finality_enabled() && legacy_chain_reference(chain_ref).is_some() {
            Some(Reject::CustodyReceiptRequired)
        } else if self.credited_refs.contains(chain_ref) {
            Some(Reject::DuplicateReference)
        } else {
            None
        };
        if let Some(reason) = reason {
            events.push(Event::DepositRejected { account, amount, chain_ref: chain_ref.to_owned(), reason });
            return;
        }
        self.credited_refs.insert(chain_ref.to_owned());
        self.accounts.entry(account).or_default().balance += amount;
        events.push(Event::Deposited { account, amount });
    }

    /// Authorises the request, then approves a withdrawal of free cash: no more than the balance,
    /// and no more than equity above the margin requirement. Approval bumps the account's
    /// withdrawal nonce, which the enclave signs with the payout.
    pub(crate) fn withdraw(
        &mut self,
        account: AccountId,
        amount: Fixed,
        auth: Option<&RequestAuth>,
        events: &mut Vec<Event>,
    ) {
        let authorised = self.authorise(account, auth, |domain, auth| {
            withdrawal_hash(domain, account, amount, auth.nonce, auth.expires_at)
        });
        let available = self.accounts.get(&account).map_or(Fixed::ZERO, |acct| {
            let (equity, margin) = self.equity_and_margin(acct, None);
            let cash = if self.deposit_finality_enabled() {
                if acct.balance > acct.unfinalised_deposits {
                    acct.balance - acct.unfinalised_deposits
                } else {
                    Fixed::ZERO
                }
            } else {
                acct.balance
            };
            cash.min(equity - margin).max(Fixed::ZERO)
        });
        let reason = if let Err(reason) = authorised {
            Some(reason)
        } else if !amount.is_valid_input() {
            Some(Reject::InvalidAmount)
        } else if amount > available {
            Some(Reject::InsufficientFunds { available })
        } else {
            None
        };
        if let Some(reason) = reason {
            events.push(Event::WithdrawalRejected { account, amount, reason });
            return;
        }
        let acct = self.accounts.get_mut(&account).expect("an account with funds available exists");
        acct.balance -= amount;
        acct.withdrawal_nonce += 1;
        events.push(Event::WithdrawalApproved { account, amount, nonce: acct.withdrawal_nonce });
    }

    /// Moves house capital in (positive) or out (negative), at most once per chain reference, and
    /// never leaving house NAV at or below zero. From `V4`, a zero amount records its reference and
    /// moves nothing, so a genesis that starts the house with no capital still records its credit.
    pub(crate) fn house_capital(&mut self, amount: Fixed, chain_ref: &str, events: &mut Vec<Event>) {
        let recorded_zero = amount == Fixed::ZERO && self.protocol_version >= ProtocolVersion::V4;
        let reason = if !recorded_zero && !amount.abs().is_valid_input() {
            Some("house capital amount out of range")
        } else if self.deposit_finality_enabled() && legacy_chain_reference(chain_ref).is_some() {
            Some("vault capital credits require exact custody receipts")
        } else if self.credited_refs.contains(chain_ref) {
            Some("house capital reference already applied")
        } else if self.deposit_finality_enabled()
            && amount.is_negative()
            && self.unfinalised_house_deposits.is_positive()
            && self
                .house_cash
                .raw()
                .checked_add(amount.raw())
                .is_none_or(|cash| cash < self.unfinalised_house_deposits.raw())
        {
            Some("house capital move would withdraw unfinalised principal")
        } else if amount.is_negative() && !(self.house_nav() + amount).is_positive() {
            Some("house capital move would leave NAV <= 0")
        } else {
            None
        };
        if let Some(reason) = reason {
            events.push(Event::AdminRejected { reason: reason.into() });
            return;
        }
        self.credited_refs.insert(chain_ref.to_owned());
        self.house_cash += amount;
        events.push(Event::HouseCapitalMoved { amount, nav: self.house_nav() });
    }

    /// Charges financing for every rollover the sequenced time has passed. After downtime each
    /// missed day is charged in turn, at the marks the engine holds. From `V6`, each day's charges
    /// are followed by a close-out check of every account.
    pub(crate) fn run_rollovers(&mut self, events: &mut Vec<Event>) {
        let Some(params) = &self.params else { return };
        let rollover = params.rollover.clone();
        while let Some(at) = self.next_rollover.filter(|at| *at <= self.now) {
            self.charge_financing(events);
            if self.protocol_version >= ProtocolVersion::V18 {
                self.prune_benchmark_history(self.benchmark_date(at));
            }
            self.next_rollover = Some(rollover.next_after(at));
            if self.protocol_version >= ProtocolVersion::V6 {
                self.check_all_closeouts(events);
            }
        }
    }

    /// One day of overnight financing on every open position. Longs pay benchmark + markup; shorts
    /// receive benchmark - markup. From `V14` each benchmark with no rate is recorded with the markets
    /// that use it, which accrue nothing.
    fn charge_financing(&mut self, events: &mut Vec<Event>) {
        if self.protocol_version >= ProtocolVersion::V18 {
            self.benchmark_status(
                self.benchmark_date(self.next_rollover.expect("charging a scheduled rollover")),
                events,
            );
        } else if self.protocol_version >= ProtocolVersion::V14 {
            let unrated = self
                .markets
                .iter()
                .filter(|(_, market)| !self.benchmarks.contains_key(&market.spec.financing.benchmark))
                .fold(BTreeMap::<&str, Vec<MarketId>>::new(), |mut unrated, (id, market)| {
                    unrated.entry(&market.spec.financing.benchmark).or_default().push(*id);
                    unrated
                });
            events.extend(
                unrated.into_iter().map(|(benchmark, markets)| Event::BenchmarkRateMissing {
                    benchmark: benchmark.to_owned(),
                    markets,
                }),
            );
        }
        let charges: Vec<(AccountId, MarketId, Fixed)> = self
            .accounts
            .iter()
            .flat_map(|(account, acct)| acct.positions.iter().map(move |(market, pos)| (*account, *market, *pos)))
            .filter_map(|(account, market, pos)| {
                self.daily_financing(market, pos).map(|amount| (account, market, amount))
            })
            .filter(|(_, _, amount)| !amount.is_zero())
            .collect();
        for (account, market, amount) in charges {
            self.accounts.get_mut(&account).expect("account holds a position").balance -= amount;
            self.house_cash += amount;
            events.push(Event::Financing { account, market, amount });
        }
    }

    /// One day's financing on `pos` in USD, positive when the account pays, or `None` when the market
    /// has no mark to value it at or, from `V14`, its benchmark has no rate. Before `V14` a benchmark
    /// with no rate counts as zero. From `V5` the amount rounds up, so a charge rounds against the
    /// account and a credit toward zero; before `V5` it truncates. A market quoted in another
    /// currency converts the amount to USD rounded up, the same way.
    fn daily_financing(&self, market_id: MarketId, pos: Position) -> Option<Fixed> {
        let market = self.markets.get(&market_id)?;
        let mark = market.mark?;
        let financing = &market.spec.financing;
        if self.protocol_version >= ProtocolVersion::V18 {
            let date = self.benchmark_date(self.next_rollover.expect("charging a scheduled rollover"));
            let rate = self
                .dated_benchmark(&financing.benchmark, date)
                .map(|(_, rate)| rate)
                .or_else(|| self.benchmarks.get(&financing.benchmark).map(|bps| Fixed::from_bps(*bps)))?;
            let markup = Fixed::from_bps(financing.markup_bps);
            let annual_rate = if pos.qty.is_positive() { rate + markup } else { markup - rate };
            let notional =
                pos.qty.abs().mul_rounded(mark, if annual_rate >= Fixed::ZERO { Rounding::Up } else { Rounding::Down });
            let amount =
                notional.mul_rounded(annual_rate, Rounding::Up).div_int_rounded(financing.day_basis, Rounding::Up);
            return Some(market.usd_at_mark(amount, Rounding::Up));
        }
        let benchmark = match self.benchmarks.get(&financing.benchmark) {
            Some(rate) => *rate,
            None if self.protocol_version >= ProtocolVersion::V14 => return None,
            None => 0,
        };
        let annual_bps =
            if pos.qty.is_positive() { benchmark + financing.markup_bps } else { financing.markup_bps - benchmark };
        let amount = if self.protocol_version < ProtocolVersion::V5 {
            pos.qty.abs().mul(mark).mul_bps(annual_bps).div_int(financing.day_basis)
        } else {
            // Each step rounds the amount up: the notional up for a charge and down for a credit, then
            // the rate and the day count, so it is the exact amount rounded up or one unit above it.
            let notional = pos.qty.abs().mul_rounded(mark, if annual_bps >= 0 { Rounding::Up } else { Rounding::Down });
            notional.mul_bps_rounded(annual_bps, Rounding::Up).div_int_rounded(financing.day_basis, Rounding::Up)
        };
        Some(market.usd_at_mark(amount, Rounding::Up))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::MarketState;
    use crate::market::MarketSpec;
    use crate::types::Side;

    #[test]
    fn a_house_principal_lock_refuses_an_overflowing_cash_debit_without_mutation() {
        let cash = Fixed::from_raw(i128::MIN + 1);
        let mut engine = Engine {
            protocol_version: ProtocolVersion::LATEST,
            house_cash: cash,
            unfinalised_house_deposits: Fixed::ONE,
            ..Engine::default()
        };
        assert!(engine.deposit_finality_enabled(), "V19 activation requires actual V18 integration");
        let mut events = Vec::new();
        engine.house_capital(-Fixed::ONE, "overflowing-house-debit", &mut events);
        assert_eq!(
            events,
            vec![Event::AdminRejected { reason: "house capital move would withdraw unfinalised principal".into() }]
        );
        assert_eq!(engine.house_cash, cash);
        assert_eq!(engine.unfinalised_house_deposits, Fixed::ONE);
        assert!(!engine.is_credited("overflowing-house-debit"));
    }

    #[test]
    fn financing_whose_notional_times_its_rate_passes_i128_is_charged_exactly() {
        // 1e16 units marked at 1e9 is 1e34 of raw notional: at 200% a year its raw product with the
        // rate, 2e38, is past `i128::MAX`, though a day's financing is near 5.5e22. Charges round up
        // and the credit toward zero.
        let cases = [
            (10_000, 10_000, Side::Buy, "54794520547945205479452.054794521"),
            (-10_000, 10_000, Side::Sell, "54794520547945205479452.054794521"),
            (-10_000, -10_000, Side::Buy, "-54794520547945205479452.05479452"),
        ];
        for (benchmark_bps, markup_bps, side, daily) in cases {
            let spec: MarketSpec = serde_json::from_value(serde_json::json!({
                "id": 1, "symbol": "BIG", "asset_class": "crypto", "schedule": { "kind": "always_open" },
                "margin_tiers": [{ "bps": 1_000 }], "spread_tiers": [{ "bps": 0 }],
                "price_rules": { "max_age_ms": 1_000, "max_divergence_bps": 50, "min_sources": 2 },
                "max_slippage_bps": 20, "house_cap_bps": 5_000,
                "financing": { "benchmark": "SOFR", "markup_bps": markup_bps, "day_basis": 365 },
            }))
            .expect("a valid listing");
            let mut market = MarketState::new(spec);
            market.mark = Some(Fixed::MAX_INPUT);
            let mut engine = Engine { protocol_version: ProtocolVersion::V5, ..Engine::default() };
            engine.markets.insert(1, market);
            engine.benchmarks.insert("SOFR".into(), benchmark_bps);
            let position = Position { qty: side.signed(Fixed::from_int(10_000_000_000_000_000)), entry: Fixed::ONE };
            assert_eq!(engine.daily_financing(1, position), Some(daily.parse().unwrap()), "{side:?} {benchmark_bps}");
        }
    }
}
