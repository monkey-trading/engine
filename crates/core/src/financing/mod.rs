//! The rollover's financing charge from `V18`: each market's terms read once, each position charged
//! at them, and each account's balance written once. From `V80` the rollover logs one
//! `Event::FinancingApplied` in place of an `Event::Financing` per position, with the root over each
//! account's charges (`commitment`); the per-position rows are the command's derived output
//! (`Engine::take_derived`), at the same amounts, in the same order.

use std::collections::BTreeMap;

mod commitment;
pub use commitment::{
    Charge, ChargesTree, FinancingProof, FinancingRow, charges_of, leaf_hash, node_hash, proof, root_of,
};

use crate::Fixed;
use crate::engine::Engine;
use crate::event::Event;
use crate::fixed::Rounding;
use crate::operator_bounds::rollover::financing_over;
use crate::params::MAX_RATE_BPS;
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId, Millis};

/// What a market charges a day at the rollover: its mark and its benchmark's annual rate.
#[derive(Clone, Copy)]
struct Terms {
    mark: Fixed,
    rate: Fixed,
}

impl Engine {
    /// The derived output of the last command applied, which the log does not carry: from `V80` a
    /// rollover's row per position charged, the `Event::Financing` the rollover logged before it, in
    /// that order, for each `Event::FinancingApplied` in turn. Taking them leaves none; the next
    /// command's apply drops any left.
    pub fn take_derived(&mut self) -> Vec<FinancingRow> {
        std::mem::take(&mut self.derived)
    }

    /// One day of financing, over `span` of a day, on every open position from `V18`: longs pay
    /// benchmark + markup, shorts receive benchmark - markup, the markup less the account's tier's
    /// discount from `V68`, at the rate dated for the rollover. A market with no mark, no rate, or a
    /// failed split charges nothing, and a zero charge is not recorded. Each account's balance falls by
    /// its charges at once, and house cash takes each in turn. Before `V80` each charge is an
    /// `Event::Financing`; from it the rollover's one `Event::FinancingApplied`, when anything is
    /// charged, with the rows derived.
    pub(crate) fn charge_at_terms(&mut self, span: Millis, events: &mut Vec<Event>) {
        let at = self.next_rollover.expect("charging a scheduled rollover");
        let terms = self.financing_terms(at);
        let engine = &*self;
        let mut charged: Vec<(AccountId, usize, Fixed)> = Vec::new();
        let mut rows: Vec<Charge> = Vec::new();
        for (account, acct) in engine.accounts.iter().filter(|(_, acct)| !acct.positions.is_empty()) {
            let discount_bps = engine.financing_discount_bps(*account, at);
            let (start, mut total) = (rows.len(), Fixed::ZERO);
            for (market_id, pos) in acct.positions.iter() {
                let Some(terms) = terms.get(market_id) else { continue };
                let market = &engine.markets[market_id];
                let financing = &market.spec.financing;
                let markup = discounted_markup(Fixed::from_bps(financing.markup_bps), discount_bps);
                let annual_rate = if pos.qty.is_positive() { terms.rate + markup } else { markup - terms.rate };
                let rounding = if annual_rate >= Fixed::ZERO { Rounding::Up } else { Rounding::Down };
                let notional = pos.qty.abs().mul_rounded(terms.mark, rounding);
                let amount =
                    notional.mul_rounded(annual_rate, Rounding::Up).div_int_rounded(financing.day_basis, Rounding::Up);
                let amount = financing_over(market.usd_at_mark(amount, Rounding::Up), span);
                if !amount.is_zero() {
                    rows.push(Charge { market: *market_id, amount });
                    total += amount;
                }
            }
            if rows.len() > start {
                charged.push((*account, rows.len() - start, total));
            }
        }
        let derive = self.protocol_version >= ProtocolVersion::V80;
        if derive {
            self.derived.reserve(rows.len());
        } else {
            events.reserve(rows.len());
        }
        let (mut leaves, mut net, mut taken) = (Vec::with_capacity(charged.len()), Fixed::ZERO, 0);
        for (account, count, total) in charged {
            self.accounts.get_mut(&account).expect("account holds a position").balance -= total;
            let own = &rows[taken..taken + count];
            taken += count;
            for charge in own {
                self.house_cash += charge.amount;
                let row = FinancingRow { account, market: charge.market, amount: charge.amount };
                if derive { self.derived.push(row) } else { events.push(row.event()) }
            }
            if derive {
                net += total;
                leaves.push(leaf_hash(account, own));
            }
        }
        if derive && !leaves.is_empty() {
            let (positions, accounts) = (rows.len() as u64, leaves.len() as u64);
            let root = root_of(leaves);
            events.push(Event::FinancingApplied { at, span, positions, accounts, net, root });
        }
    }

    /// Each market's terms at the rollover at `at`: its mark and the rate dated for that day, or the
    /// benchmark's undated rate; none for a market a failed split blocks, or with no mark or rate.
    fn financing_terms(&self, at: Millis) -> BTreeMap<MarketId, Terms> {
        let date = self.benchmark_date(at);
        self.markets
            .iter()
            .filter(|(_, market)| !market.corporate_action_blocked)
            .filter_map(|(id, market)| {
                let mark = market.mark?;
                let benchmark = &market.spec.financing.benchmark;
                let rate = self
                    .dated_benchmark(benchmark, date)
                    .map(|(_, rate)| rate)
                    .or_else(|| self.benchmarks.get(benchmark).map(|bps| Fixed::from_bps(*bps)))?;
                Some((*id, Terms { mark, rate }))
            })
            .collect()
    }
}

/// `markup` less `discount_bps` of it, rounded up in the house's favour. A negative markup is left as
/// listed, so a discount never makes an account pay more.
pub(crate) fn discounted_markup(markup: Fixed, discount_bps: i64) -> Fixed {
    markup.min(markup.mul_bps_rounded(MAX_RATE_BPS - discount_bps, Rounding::Up))
}

#[cfg(test)]
mod tests;
