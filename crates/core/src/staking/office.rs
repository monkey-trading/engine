//! The operator's share split by time in office, from `V50`.
//!
//! While rotation is on, each command first credits the sequenced time since the command before it
//! to the account that held the office over it, as that command left the state: the candidate seated
//! under the key in effect, or the key's address for an operator no election installed
//! (`Engine::operator_share_account`). A handover therefore counts from the command that applies it,
//! including a close's own command, whose time before it is the outgoing operator's. The open epoch
//! counts from the command that closed the epoch before it, or from `V50`'s activation or staking's
//! initialisation under it, to the command that closes it, skipped epochs included, as that close
//! pays the excess they built up. The close splits `floor(excess / 5)` by that time: each account
//! earns `floor(share · served / counted)`, where `counted` is all the time the open epoch counted,
//! so the time rotation was off, which no one served, and the rounding go to stakers.
//!
//! From `V51`, once a daily turn has applied, the account in office is the turn's member from its
//! `TurnTaken` to the end of its turn (`turns`), so the time before a member takes its turn, and an
//! incumbent's time past its turn, are no one's. From `V53`, before any turn is in effect, the account
//! in office holds it only until the first accepted turn applies. From `V71`, a turn applied only after
//! its end (a late promotion) holds the office until the next turn applies, not to its nominal end.
//!
//! From `V70`, a proven fault's haircut withholds its integral from the member's time in office, and
//! the close pays the member only the time it kept (`operator_faults`).

use std::collections::BTreeMap;

use alloy_primitives::U256;
use serde::{Deserialize, Serialize};

use crate::engine::Engine;
use crate::event::Event;
use crate::operator_faults::WHOLE_BPS;
use crate::protocol::ProtocolVersion;
use crate::staking::UsdcUnits;
use crate::types::{AccountId, Millis};

/// From `V50`, when the open distribution epoch began counting time in office, and each account's
/// time in office since.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeInOffice {
    /// When the open epoch began counting, in sequenced milliseconds.
    pub since: Millis,
    /// Each account's sequenced time in office since `since`, in milliseconds; never zero.
    pub served: BTreeMap<AccountId, Millis>,
}

impl Engine {
    /// From `V50`, credits the sequenced time from `from` to now to the account in office over it,
    /// before this command's handover can move the office.
    pub(crate) fn accrue_time_in_office(&mut self, from: Millis) {
        if self.protocol_version < ProtocolVersion::V50 {
            return;
        }
        // From `V51`, once a turn has applied, the member holds the office from its `TurnTaken` to the
        // end of its turn; from `V53`, the account before it only until the first turn applies.
        let holder = match self.turn_office() {
            Some(turn) => turn,
            None => self.operator_share_account().map(|account| (account, self.first_turn_applies_at())),
        };
        let Some((holder, until)) = holder else { return };
        let to = self.now.min(until);
        // From `V70` a proven fault's haircut withholds its integral over the same time.
        let withheld = self.withheld_in_office(holder, from, to);
        let Some(staking) = self.staking.as_mut() else { return };
        let office = staking.office.as_mut().expect("V50 counts time in office once staking is initialised");
        let served = to - from;
        if served > 0 {
            *office.served.entry(holder).or_default() += served;
        }
        if withheld > 0 {
            *self.operator_faults.withheld.entry(holder).or_default() += withheld;
        }
    }

    /// From `V50`, `share` USDC base units split by the open epoch's time in office, in account order:
    /// `floor(share · served / counted)` each, leaving out the parts that round to nothing. Each part
    /// comes with what a proven fault's haircut withheld from it: from `V70` an account the haircut
    /// withheld `w` bps·ms from is paid `floor(share · (10,000 · served − w) / (10,000 · counted))`.
    pub(crate) fn split_by_time_in_office(&self, share: u128) -> Vec<(AccountId, u128, u128)> {
        let office = self.staking.as_ref().and_then(|staking| staking.office.as_ref());
        let office = office.expect("V50 counts time in office once staking is initialised");
        // Positive whenever anyone served: every credit lies between `since` and now.
        let counted = U256::from((self.now - office.since) as u128);
        let whole_bps = U256::from(WHOLE_BPS as u128);
        office
            .served
            .iter()
            .filter_map(|(account, served)| {
                let served = U256::from(*served as u128);
                // At most `share`, as no account served longer than the epoch counted.
                let whole: u128 = (U256::from(share) * served / counted).to();
                // A haircut withholds at most 10,000 bps of each millisecond, so at most `whole`.
                let kept = self.operator_faults.withheld.get(account).map_or(whole, |withheld| {
                    let kept_bps_ms = (served * whole_bps).saturating_sub(U256::from(*withheld as u128));
                    (U256::from(share) * kept_bps_ms / (counted * whole_bps)).to()
                });
                (whole > 0).then_some((*account, kept, whole - kept))
            })
            .collect()
    }

    /// Credits each part of the operator's share of the close of epoch `closed`, `share` USDC base units,
    /// reporting what a proven fault's haircut withheld from it, and returns the total credited.
    pub(crate) fn credit_operator_shares(&mut self, share: u128, closed: u64, events: &mut Vec<Event>) -> u128 {
        let mut credited = 0;
        for (account, amount, withheld) in self.operator_shares(share) {
            if amount > 0 {
                self.credit_distribution(account, amount);
                credited += amount;
                events.push(Event::OperatorShareAllocated { account, epoch: closed, amount: UsdcUnits(amount) });
            }
            if withheld > 0 {
                events.push(Event::OperatorShareWithheld { account, epoch: closed, amount: UsdcUnits(withheld) });
            }
        }
        credited
    }

    /// Starts counting time in office now, afresh: at `V50`'s activation, at staking's initialisation
    /// under it, and at each close, which from `V70` also drops the haircuts withheld so far and those
    /// that have decayed to nothing.
    pub(crate) fn start_time_in_office(&mut self) {
        let since = self.now;
        if let Some(staking) = self.staking.as_mut() {
            staking.office = Some(TimeInOffice { since, served: BTreeMap::new() });
        }
        self.operator_faults.withheld.clear();
        self.drop_spent_haircuts();
    }
}
