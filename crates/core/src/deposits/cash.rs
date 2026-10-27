//! Principal cash and its withdrawal lock, moved together for credited receipts.

use crate::Fixed;
use crate::deposits::{DepositBeneficiary, DepositReceipt, DepositReject};
use crate::engine::Engine;

impl Engine {
    /// Changes cash, and its principal lock where `lock`, together, after checking both arithmetic
    /// results. A receipt credited after its finality takes no lock.
    /// Error cases:
    /// - cash or locked principal would overflow the fixed-point representation
    pub(crate) fn move_deposit_cash(
        &mut self,
        receipt: &DepositReceipt,
        credit: bool,
        lock: bool,
    ) -> Result<(), DepositReject> {
        let (balance, locked) = match receipt.beneficiary {
            DepositBeneficiary::Trader { account } => {
                self.accounts.get(&account).map_or((Fixed::ZERO, Fixed::ZERO), |a| (a.balance, a.unfinalised_deposits))
            }
            DepositBeneficiary::House => (self.house_cash, self.unfinalised_house_deposits),
        };
        let delta = if credit { receipt.amount.raw() } else { -receipt.amount.raw() };
        let balance =
            balance.raw().checked_add(delta).filter(|b| *b != i128::MIN).ok_or(DepositReject::ArithmeticOverflow)?;
        if !credit
            && balance < 0
            && let DepositBeneficiary::Trader { account } = receipt.beneficiary
            && self.accounts.get(&account).is_some_and(|a| a.positions.is_empty())
            && self.house_cash.raw().checked_add(balance).is_none()
        {
            return Err(DepositReject::ArithmeticOverflow);
        }
        let locked =
            if lock { locked.raw().checked_add(delta).ok_or(DepositReject::ArithmeticOverflow)? } else { locked.raw() };
        assert!(locked >= 0, "a live receipt's principal is locked until finalised or reversed");
        match receipt.beneficiary {
            DepositBeneficiary::Trader { account } => {
                let acct = self.accounts.entry(account).or_default();
                acct.balance = Fixed::from_raw(balance);
                acct.unfinalised_deposits = Fixed::from_raw(locked);
            }
            DepositBeneficiary::House => {
                self.house_cash = Fixed::from_raw(balance);
                self.unfinalised_house_deposits = Fixed::from_raw(locked);
            }
        }
        Ok(())
    }

    /// Releases a live receipt's lock without crediting its principal again.
    pub(super) fn unlock_deposit(&mut self, receipt: &DepositReceipt) {
        let locked = match receipt.beneficiary {
            DepositBeneficiary::Trader { account } => {
                &mut self.accounts.get_mut(&account).expect("credited account exists").unfinalised_deposits
            }
            DepositBeneficiary::House => &mut self.unfinalised_house_deposits,
        };
        assert!(*locked >= receipt.amount, "a live receipt's principal is locked");
        *locked -= receipt.amount;
        // Final principal is physical payout capacity from V22; a reversal never released it.
        if let Some(cash) = &mut self.withdrawal_cash {
            *cash += receipt.amount;
        }
    }
}
