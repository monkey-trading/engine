//! The operator's share split by time in office, from `V50`.
//!
//! While rotation is on, each command first credits the sequenced time since the command before it
//! to the account that held the office over it, as that command left the state: the candidate seated
//! under the key in effect, or the key's address for an operator no election installed
//! (`Engine::operator_share_account`). A handover therefore counts from the command that applies it,
//! including a close's own command, whose time before it is the outgoing operator's. The open epoch
//! counts from the command that closed the epoch before it, or from `V50`'s activation or staking's
//! initialisation under it, to the command that closes it, skipped epochs included, as that close
//! pays the excess they built up. The close splits `floor(excess / 5)`, from `V89` of what the awards
//! leave, by that time: each account earns `floor(share · served / counted)`, where `counted` is all
//! the time the open epoch counted, so the time rotation was off, which no one served, and the
//! rounding go to stakers. The count
//! restarts at every close, one that distributes nothing included. A close the activating command
//! runs before `V50` applies pays under the rules it leaves, and an activation from before `V47`
//! first seats the candidate of an election already in effect, which those rules never recorded.
//!
//! From `V51`, once a daily turn has applied, the account in office is the turn's member from its
//! `TurnTaken` to the end of its turn (`turns`), so the time before a member takes its turn, and an
//! incumbent's time past its turn, are no one's. From `V53`, before any turn is in effect, the account
//! in office holds it only until the first accepted turn applies. From `V71`, a turn applied only after
//! its end (a late promotion) holds the office until the next turn applies, not to its nominal end.
//!
//! From `V70`, a proven fault's haircut withholds its integral from the member's time in office, and
//! the close pays the member only the time it kept (`operator_faults`).
//!
//! From `V83` the open epoch counts only to its own end: time served after it is kept apart, as stints
//! of one holder under one haircut, for the epochs it falls in. A close splits the share over the time
//! from the count's start to the epoch's end, however late the close runs, so holding a close earns
//! nothing, and the next epoch counts from that end, starting with the stints inside it. An epoch that
//! had already ended when `V83` activated keeps the count the rules before it ran past its end, to the
//! activation, unserved time included, and each epoch up to the one the activation falls in counts from
//! the activation, so one that ended before it counts nothing.

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
    /// When the open epoch began counting, in sequenced milliseconds; from `V83`'s activation, past the
    /// open epoch's end where the activation found it ended.
    pub since: Millis,
    /// Each account's sequenced time in office since `since`, in milliseconds; never zero. From `V83`
    /// only the time before the open epoch's end, or before `inherited_to`.
    pub served: BTreeMap<AccountId, Millis>,
    /// From `V83`, the time in office after the open epoch's end, in order, for the closes after it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub later: Vec<Stint>,
    /// From `V83`'s activation, where the open epoch had already ended by it: the activation's time, to
    /// which the rules before it counted, past the end, and to which the epoch's close counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherited_to: Option<Millis>,
}

/// From `V83`, one account's unbroken time in office under one haircut, past the open epoch's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stint {
    pub account: AccountId,
    /// When it began and ended, in sequenced milliseconds; `from < to`.
    pub from: Millis,
    pub to: Millis,
    /// When the account's haircut began decaying, if a proven fault's applied over it (`operator_faults`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub haircut: Option<Millis>,
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
        let haircut = self.operator_faults.haircuts.get(&holder).copied();
        // From `V83` the open epoch counts to its end, and the time after it is kept for later closes.
        let end = match &self.staking {
            Some(staking) if self.protocol_version >= ProtocolVersion::V83 => staking.config.epoch_end(staking.epoch),
            _ => Millis::MAX,
        };
        let counted = to.min(end);
        // From `V70` a proven fault's haircut withholds its integral over the same time.
        let withheld = self.withheld_over(haircut, from, counted);
        let Some(staking) = self.staking.as_mut() else { return };
        let office = staking.office.as_mut().expect("V50 counts time in office once staking is initialised");
        let served = counted - from;
        if served > 0 {
            *office.served.entry(holder).or_default() += served;
        }
        if withheld > 0 {
            *self.operator_faults.withheld.entry(holder).or_default() += withheld;
        }
        let after = from.max(end);
        if to > after {
            match office.later.last_mut() {
                Some(last) if last.account == holder && last.haircut == haircut && last.to == after => last.to = to,
                _ => office.later.push(Stint { account: holder, from: after, to, haircut }),
            }
        }
    }

    /// From `V50`, `share` USDC base units split by the open epoch's time in office, in account order:
    /// `floor(share · served / counted)` each, leaving out the parts that round to nothing. Each part
    /// comes with what a proven fault's haircut withheld from it: from `V70` an account the haircut
    /// withheld `w` bps·ms from is paid `floor(share · (10,000 · served − w) / (10,000 · counted))`.
    /// `counted` runs from the count's start to now, and from `V83` to the open epoch's end, or to
    /// `V83`'s activation where that found the end passed (`TimeInOffice::inherited_to`).
    pub(crate) fn split_by_time_in_office(&self, share: u128) -> Vec<(AccountId, u128, u128)> {
        let staking = self.staking.as_ref().expect("a close splits the share once staking is initialised");
        let office = staking.office.as_ref().expect("V50 counts time in office once staking is initialised");
        // Positive whenever anyone served: every credit lies between `since` and now, and from `V83`
        // before the later of the open epoch's end and an inherited count's end; at least the time served,
        // so no part exceeds the share.
        let counted = if self.protocol_version >= ProtocolVersion::V83 {
            let end = staking.config.epoch_end(staking.epoch).max(office.inherited_to.unwrap_or(Millis::MIN));
            let served: Millis = office.served.values().sum();
            (end - office.since).max(served)
        } else {
            self.now - office.since
        };
        let counted = U256::from(counted as u128);
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
    /// under it, and before `V83` at each close, which from `V70` also drops the haircuts withheld so far
    /// and those that have decayed to nothing.
    pub(crate) fn start_time_in_office(&mut self) {
        let since = self.now;
        if let Some(staking) = self.staking.as_mut() {
            staking.office =
                Some(TimeInOffice { since, served: BTreeMap::new(), later: Vec::new(), inherited_to: None });
        }
        self.operator_faults.withheld.clear();
        self.drop_spent_haircuts();
    }

    /// From `V83`, after a close: the new open epoch counts from `since`, the closed epoch's end, or from
    /// where the closed epoch's count began or ended if that is later, as for counts inherited at `V83`'s
    /// activation past one or more ends, so no time counts in two epochs; with the time its stints served
    /// before its own end, and the haircuts' integral over that time; the rest stays for the closes after
    /// it. Drops the haircuts that have decayed to nothing, as each close does.
    pub(crate) fn carry_time_in_office(&mut self, since: Millis) {
        let staking = self.staking.as_mut().expect("a close carries time in office once staking is initialised");
        let end = staking.config.epoch_end(staking.epoch);
        let office = staking.office.as_mut().expect("V50 counts time in office once staking is initialised");
        let since = since.max(office.since).max(office.inherited_to.unwrap_or(Millis::MIN));
        let stints = std::mem::take(&mut office.later);
        let (mut served, mut withheld, mut later) = (BTreeMap::new(), BTreeMap::new(), Vec::new());
        for stint in stints {
            let to = stint.to.min(end);
            if to > stint.from {
                *served.entry(stint.account).or_default() += to - stint.from;
                let part = self.withheld_over(stint.haircut, stint.from, to);
                if part > 0 {
                    *withheld.entry(stint.account).or_default() += part;
                }
            }
            if stint.to > end {
                later.push(Stint { from: stint.from.max(end), ..stint });
            }
        }
        let staking = self.staking.as_mut().expect("read above");
        staking.office = Some(TimeInOffice { since, served, later, inherited_to: None });
        self.operator_faults.withheld = withheld;
        self.drop_spent_haircuts();
    }

    /// At `V83`'s activation: where the open epoch has already ended, the count the rules before it ran
    /// past the end stops now, so the epoch's close counts to now and the next epoch from now, and the
    /// time after now is kept for the closes after it.
    pub(crate) fn inherit_time_in_office(&mut self) {
        let now = self.now;
        let Some(staking) = self.staking.as_mut() else { return };
        let end = staking.config.epoch_end(staking.epoch);
        if let Some(office) = staking.office.as_mut().filter(|_| now > end) {
            office.inherited_to = Some(now);
        }
    }
}
