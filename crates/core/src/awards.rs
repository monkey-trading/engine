//! Bug bounty awards, from `V89`: a researcher's award, approved by a passed vote and paid in USDC from the
//! epoch closes' distributions, ahead of the operator's share and the stakers.
//!
//! - **Approval.** Only a passed normal vote applies `AdminCommand::ApproveAward`, which names the recipient,
//!   the finding's severity, the advisory and the amount. The amount must be the scale's for the severity
//!   (`Severity::amount`). The operator's signed commands and the node's unsigned ones carrying it are
//!   refused in every state, and the fast path refuses it, as every command it does not take. An approved
//!   award joins the back of the queue under the next award id.
//! - **Payment.** A close that distributes pays the queue first, oldest award first, up to the cap in force:
//!   the lesser of `excess_bps` of its distributable excess and `max`, in whole USDC base units. Each payment
//!   is credited to the recipient's cumulative distribution, which the vault's `claimDistribution` pays and
//!   its exit counts with the stakers'. An award a close does not finish stays at the front. The operator's
//!   fifth and the stakers' shares come from what the awards leave. A close that distributes nothing, at
//!   or below the liquidity floor or with nobody staked, pays no award.
//! - **The cap** is `LAUNCH_CAP` until a passed vote's `AdminCommand::SetAwardCap` moves it, within
//!   `CAP_BPS` and `CAP_MAX`. Nothing else moves it.
//!
//! Every approved award stays in state with what the closes have paid of it, each in a leaf of its own
//! (`merkle::award_key`); the cap a vote set is in the remaining-state leaf.

use std::ops::RangeInclusive;

use alloy_primitives::U256;
use serde::{Deserialize, Serialize};

use crate::command::AdminCommand;
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;
use crate::staking::UsdcUnits;
use crate::types::{AccountId, Millis};

/// One USDC in USDC base units.
const USDC: u128 = 1_000_000;
/// The cap in force until a vote moves it: 5% of a close's distributable excess, and at most 50,000 USDC.
pub const LAUNCH_CAP: AwardCap = AwardCap { excess_bps: 500, max: UsdcUnits(50_000 * USDC) };
/// The shares of a close's distributable excess a vote may set the cap at: 5% to 20%.
pub const CAP_BPS: RangeInclusive<i64> = 500..=2_000;
/// The limits a vote may set the cap at, in USDC base units: 50,000 to 250,000 USDC.
pub const CAP_MAX: RangeInclusive<u128> = 50_000 * USDC..=250_000 * USDC;
/// The most characters an advisory id may hold.
pub const MAX_ADVISORY_CHARS: usize = 64;

/// A finding's severity, as `SECURITY.md` grades it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

/// What a vote's `AdminCommand::ApproveAward` names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwardApproval {
    /// The address the award is credited to, which claims it from the vault.
    pub recipient: AccountId,
    pub severity: Severity,
    /// The published advisory's identifier, such as `GHSA-xxxx-xxxx-xxxx`.
    pub advisory: String,
    /// In USDC base units: the scale's amount for `severity`, and no other.
    pub amount: UsdcUnits,
}

/// An approved award and what the closes have paid of it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Award {
    /// The passed proposal that approved it.
    pub proposal: u64,
    pub recipient: AccountId,
    pub severity: Severity,
    pub advisory: String,
    /// In USDC base units.
    pub amount: UsdcUnits,
    /// What the closes have credited of `amount`, in USDC base units.
    pub paid: UsdcUnits,
    /// When the vote applied it.
    pub approved_at: Millis,
}

/// The most one close pays awards: the lesser of `excess_bps` of its distributable excess and `max`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwardCap {
    pub excess_bps: i64,
    /// In USDC base units.
    pub max: UsdcUnits,
}

/// `AwardApproved`: an award a passed vote approved for `account`, queued at the back under `award`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwardApproved {
    pub award: u64,
    pub proposal: u64,
    pub account: AccountId,
    pub severity: Severity,
    pub advisory: String,
    pub amount: UsdcUnits,
}

/// `AwardPaid`: what the close of `epoch` paid award `award`, credited to `account`'s cumulative
/// distribution; `owed` is what is left of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AwardPaid {
    pub award: u64,
    pub account: AccountId,
    pub epoch: u64,
    pub amount: UsdcUnits,
    pub owed: UsdcUnits,
}

impl Severity {
    /// The award the scale fixes for a finding of this severity, in USDC base units: Critical 250,000 USDC,
    /// High 50,000, Medium 10,000 and Low 1,000.
    pub const fn amount(self) -> UsdcUnits {
        UsdcUnits(
            match self {
                Severity::Critical => 250_000,
                Severity::High => 50_000,
                Severity::Medium => 10_000,
                Severity::Low => 1_000,
            } * USDC,
        )
    }
}

impl Award {
    /// What is still owed of it, in USDC base units.
    pub fn owed(&self) -> u128 {
        self.amount.0 - self.paid.0
    }
}

impl AwardCap {
    /// What a close whose distributable excess is `excess` USDC base units may pay awards, rounded down.
    pub fn of(&self, excess: u128) -> u128 {
        // The share is at most 20%, so the result is at most `excess`.
        let share: u128 = (U256::from(excess) * U256::from(self.excess_bps) / U256::from(10_000)).to();
        share.min(self.max.0)
    }
}

/// Checks an award or a cap a vote carries, before it waits out its notice and again when it applies;
/// any other command passes.
/// Error cases:
/// - an award to the zero address, for an amount other than the scale's for its severity, or naming an
///   advisory that is not 1 to `MAX_ADVISORY_CHARS` ASCII letters, digits and hyphens
/// - a cap outside `CAP_BPS` or `CAP_MAX`
pub fn check_award_vote(command: &AdminCommand) -> Result<(), String> {
    match command {
        AdminCommand::ApproveAward(approval) => {
            if approval.recipient == AccountId([0; 20]) {
                return Err("an award's recipient is a nonzero address".into());
            }
            let scale = approval.severity.amount();
            if approval.amount != scale {
                return Err(format!(
                    "a {:?} award is {scale} USDC base units, not {}",
                    approval.severity, approval.amount
                ));
            }
            let advisory = &approval.advisory;
            let valid = (1..=MAX_ADVISORY_CHARS).contains(&advisory.len())
                && advisory.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
            if !valid {
                return Err(format!(
                    "an advisory id is 1 to {MAX_ADVISORY_CHARS} ASCII letters, digits and hyphens: {advisory:?}"
                ));
            }
            Ok(())
        }
        AdminCommand::SetAwardCap(cap) => {
            if !CAP_BPS.contains(&cap.excess_bps) {
                return Err(format!("the award cap's share {} bps is outside {CAP_BPS:?}", cap.excess_bps));
            }
            if !CAP_MAX.contains(&cap.max.0) {
                return Err(format!("the award cap's limit {} is outside {CAP_MAX:?} USDC base units", cap.max));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

impl Engine {
    /// Every award a vote approved, by id in the order approved, with what the closes have paid of it.
    pub fn awards(&self) -> &std::collections::BTreeMap<u64, Award> {
        &self.awards
    }

    /// The cap in force on what one close pays awards.
    pub fn award_cap(&self) -> AwardCap {
        self.award_cap.unwrap_or(LAUNCH_CAP)
    }

    /// Queues the award proposal `proposal` approved, at the back.
    /// Error cases:
    /// - before `V89`
    /// - an award `check_award_vote` refuses
    pub(crate) fn approve_award(
        &mut self,
        proposal: u64,
        approval: &AwardApproval,
        events: &mut Vec<Event>,
    ) -> Result<(), String> {
        self.check_awards_version()?;
        check_award_vote(&AdminCommand::ApproveAward(approval.clone()))?;
        let AwardApproval { recipient, severity, ref advisory, amount } = *approval;
        // Awards are never removed, so the count is the next id.
        let award = self.awards.len() as u64;
        let queued = Award {
            proposal,
            recipient,
            severity,
            advisory: advisory.clone(),
            amount,
            paid: UsdcUnits(0),
            approved_at: self.now,
        };
        self.awards.insert(award, queued);
        let approved =
            AwardApproved { award, proposal, account: recipient, severity, advisory: advisory.clone(), amount };
        events.push(Event::AwardApproved(approved));
        Ok(())
    }

    /// Sets the cap a passed vote names.
    /// Error cases:
    /// - before `V89`
    /// - a cap `check_award_vote` refuses
    pub(crate) fn set_award_cap(&mut self, cap: &AwardCap) -> Result<(), String> {
        self.check_awards_version()?;
        check_award_vote(&AdminCommand::SetAwardCap(*cap))?;
        self.award_cap = Some(*cap);
        Ok(())
    }

    /// Pays the queue from a close of `epoch` whose distributable excess is `excess` USDC base units, oldest
    /// award first, up to the cap in force, crediting each payment to its recipient's cumulative
    /// distribution; returns the total paid.
    pub(crate) fn pay_awards(&mut self, excess: u128, epoch: u64, events: &mut Vec<Event>) -> u128 {
        let mut left = self.award_cap().of(excess);
        let queue: Vec<u64> = self.awards.iter().filter(|(_, award)| award.owed() > 0).map(|(id, _)| *id).collect();
        let mut total = 0;
        for id in queue {
            if left == 0 {
                break;
            }
            let award = self.awards.get_mut(&id).expect("queued above");
            let amount = award.owed().min(left);
            award.paid.0 += amount;
            let (recipient, owed) = (award.recipient, award.owed());
            self.credit_distribution(recipient, amount);
            left -= amount;
            total += amount;
            let paid =
                AwardPaid { award: id, account: recipient, epoch, amount: UsdcUnits(amount), owed: UsdcUnits(owed) };
            events.push(Event::AwardPaid(paid));
        }
        total
    }

    fn check_awards_version(&self) -> Result<(), String> {
        if self.protocol_version < ProtocolVersion::V89 {
            return Err(format!("awards need protocol {:?}", ProtocolVersion::V89));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
