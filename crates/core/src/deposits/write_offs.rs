//! From `V83`, what the house writes off against an account after a reversal took a deposit's principal
//! from it, repaid from the same deposit's next credit.
//!
//! A credited receipt whose principal leaves its account's cash, by a reversal or a replacement that
//! loses its credit, can leave the account below zero, and once its positions close the house writes the
//! shortfall off (`BadDebt`), at once or, where a market cannot trade, later. The chain can include the
//! same transaction-local deposit again a few blocks on, and its principal is then credited whole, so
//! without more the client would keep the loss the house absorbed on top of the deposit that covered it.
//!
//! Each such deposit is kept against its account, in the order the reversals took them, until it is
//! credited again, and a write-off against the account is counted against the newest one kept. A
//! deposit's presence would have prevented the write-offs counted against it and against every deposit
//! kept after it, and none from before it was kept. The next credit of a kept deposit, as a new receipt,
//! a reinstatement, a replacement or a screening retry, with the same beneficiary and amount, pays the
//! house back those write-offs, oldest first, up to its principal, each paid back once, and takes that
//! back out of the account's emission result. What it leaves of its own count passes to the deposit kept
//! before it, which would have prevented it too. Each account's record is a leaf of its own
//! (`merkle::reversed`).

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::deposits::{DepositBeneficiary, DepositReceipt};
use crate::engine::Engine;
use crate::event::Event;
use crate::protocol::ProtocolVersion;
use crate::types::AccountId;

/// From `V83`, an account's deposits whose principal a reversal took and that have not been credited
/// again, in the order they were taken.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReversedDeposits {
    pub deposits: Vec<ReversedDeposit>,
}

/// One deposit whose principal a reversal took, and what the house has written off against it since.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReversedDeposit {
    /// Its transaction-local identity (`DepositReceipt::transaction_reference`): with the principal, the
    /// deposit, as a reorganisation can run one transaction again with another amount.
    pub transaction: String,
    pub principal: Fixed,
    /// The write-offs against the account from when it was kept until the next deposit was kept, with
    /// those the deposits kept after it left, less what returning deposits have paid back of them.
    pub written_off: Fixed,
}

impl Engine {
    /// Closes out the beneficiary of `receipt`, whose credited principal has just left its cash, having
    /// from `V83` first kept the deposit against the account for its next credit.
    pub(crate) fn close_out_after_reversal(&mut self, receipt: &DepositReceipt, events: &mut Vec<Event>) {
        let Some(account) = self.keep_reversed_principal(receipt) else { return };
        self.close_out_after_balance_change(account, events);
    }

    /// From `V83`, keeps `receipt`'s deposit, credited and just taken from its account's cash, against the
    /// account for its next credit. Returns the account, or `None` for the house's.
    pub(crate) fn keep_reversed_principal(&mut self, receipt: &DepositReceipt) -> Option<AccountId> {
        let DepositBeneficiary::Trader { account } = receipt.beneficiary else { return None };
        if self.protocol_version >= ProtocolVersion::V83 {
            let kept = ReversedDeposit {
                transaction: receipt.transaction_reference(),
                principal: receipt.amount,
                written_off: Fixed::ZERO,
            };
            self.reversed_deposits.entry(account).or_default().deposits.push(kept);
        }
        Some(account)
    }

    /// From `V83`, counts `amount` the house just wrote off against `account` against the newest deposit
    /// it keeps.
    pub(crate) fn count_write_off(&mut self, account: AccountId, amount: Fixed) {
        if self.protocol_version < ProtocolVersion::V83 || !self.reversed_deposits.contains_key(&account) {
            return;
        }
        // Looked up for change only when held: a lookup for change marks the account's leaf for the root.
        let reversed = self.reversed_deposits.get_mut(&account).expect("checked above");
        reversed.deposits.last_mut().expect("a kept record holds a deposit").written_off += amount;
    }

    /// From `V83`, after `receipt`'s principal is credited: if it is a kept deposit of its account, with
    /// the same amount, releases it and repays the house the write-offs counted against it and the
    /// deposits kept after it, oldest first, up to its principal, taking that back out of the account's
    /// emission result. What is left of its own count passes to the deposit kept before it.
    pub(crate) fn repay_write_off(&mut self, receipt: &DepositReceipt, events: &mut Vec<Event>) {
        let DepositBeneficiary::Trader { account } = receipt.beneficiary else { return };
        if self.protocol_version < ProtocolVersion::V83 {
            return;
        }
        let transaction = receipt.transaction_reference();
        let is_kept =
            |deposit: &ReversedDeposit| deposit.transaction == transaction && deposit.principal == receipt.amount;
        let kept = self.reversed_deposits.get(&account).and_then(|reversed| reversed.deposits.iter().position(is_kept));
        let Some(index) = kept else { return };
        let deposits = &mut self.reversed_deposits.get_mut(&account).expect("found above").deposits;
        let (mut amount, mut left) = (Fixed::ZERO, receipt.amount);
        for later in &mut deposits[index..] {
            let paid = left.min(later.written_off);
            later.written_off -= paid;
            (amount, left) = (amount + paid, left - paid);
        }
        let released = deposits.remove(index);
        if let Some(earlier) = index.checked_sub(1).map(|earlier| &mut deposits[earlier]) {
            earlier.written_off += released.written_off;
        }
        if deposits.is_empty() {
            self.reversed_deposits.remove(&account);
        }
        if amount.is_positive() {
            self.accounts.get_mut(&account).expect("a credited receipt's account exists").balance -= amount;
            self.house_cash += amount;
            self.record_period_result(account, Fixed::ZERO, -amount);
            events.push(Event::WriteOffRepaid { account, amount });
        }
    }
}
