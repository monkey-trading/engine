//! What an epoch close distributes: house NAV and house cash above the liquidity floor, less what
//! can still reverse and what clients cannot pay, within the withdrawal capacity.

use crate::Fixed;
use crate::engine::Engine;
use crate::protocol::ProtocolVersion;

#[cfg(test)]
thread_local! {
    /// Test-only, per test thread: how many times `account_reserves` has walked the accounts.
    pub(super) static RESERVE_WALKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

impl Engine {
    /// What an epoch close at the current state would distribute, in USD before the whole-USDC
    /// rounding, or `None` before staking is initialised: house NAV and cash above the floor, less
    /// the house's unfinalised deposits, what clients' reversals could cost the house (every client's
    /// unfinalised deposit before `V75`), the clients' debts, and within the withdrawal capacity. It
    /// walks every account.
    pub fn distributable_excess(&self) -> Option<Fixed> {
        Some(self.excess_over(self.staking.as_ref()?.config.liquidity_floor))
    }

    /// The excess over `floor` that an epoch close distributes. From `V28` it is
    /// `min(NAV', cash') - floor`, clamped at zero: `cash'` is house cash less the reversible principal
    /// `P`, the house's and every client's unfinalised deposits, and the clients' negative cash `C`;
    /// `NAV'` is house NAV less, from `V29`, the clients' negative marked equity `D`, which counts a
    /// positive-cash account whose losing positions cannot close now, and from `V30` less `P` too, so
    /// `min(NAV - P - D, cash - P - C)` and cash debt is not deducted twice. From `V31` `cash'` is also
    /// at most the unapproved withdrawal capacity, from which each allocation, paid or not, is reserved;
    /// `V31`'s activation reserves every earlier allocation once, and a negative gap it leaves blocks
    /// approvals until funded. From `V75` the clients' parts are what their reversals would leave the
    /// house to bear (`reversal_shortfalls`). A close's work is `O(accounts + open positions + stakers)`.
    pub(super) fn excess_over(&self, floor: Fixed) -> Fixed {
        self.excess_with(floor, self.account_reserves())
    }

    /// `excess_over` with the accounts' part already walked (`account_reserves`), which an epoch close
    /// leaves as it was, so the closes one command makes in turn walk the accounts once.
    pub(super) fn excess_with(&self, floor: Fixed, (reserved, uncollectible, principal): (i128, i128, i128)) -> Fixed {
        let nav = self.house_nav();
        // Reversible principal and outstanding client gains must remain covered together.
        // Cash debt is already in the independent funded-cash ceiling, so it is not deducted here.
        let principal = if self.protocol_version >= ProtocolVersion::V30 { principal } else { 0 };
        let distributable_nav = Fixed::from_raw(nav.raw().saturating_sub(principal).saturating_sub(uncollectible));
        let available_raw =
            self.house_cash.raw().saturating_sub(self.unfinalised_house_deposits.raw()).saturating_sub(reserved);
        let available = Fixed::from_raw(if self.protocol_version >= ProtocolVersion::V28 {
            available_raw.max(0)
        } else {
            available_raw
        });
        let available = if self.protocol_version >= ProtocolVersion::V31 {
            available.min(self.withdrawal_cash.expect("V31 requires V22 capacity").max(Fixed::ZERO))
        } else {
            available
        };
        if self.protocol_version >= ProtocolVersion::V28 {
            Fixed::from_raw(distributable_nav.raw().min(available.raw()).saturating_sub(floor.raw()).max(0))
        } else {
            (nav.min(available) - floor).max(Fixed::ZERO)
        }
    }

    /// The part of `excess_over` that walks every account: the cash reserve, the NAV deduction and the
    /// reversible principal, in raw `Fixed` units.
    pub(super) fn account_reserves(&self) -> (i128, i128, i128) {
        #[cfg(test)]
        RESERVE_WALKS.set(RESERVE_WALKS.get() + 1);
        // The floor stays as house liquidity, and every principal still able to reverse stays
        // locked, exactly as `house_capital` and a reversal take it back: the house's unfinalised
        // deposits and, before `V75`, each client's whole, whose realised gains are therefore not
        // distributed until the deposit finalises. From `V75` a client's is only what a reversal
        // would leave the house to bear (`reversal_shortfalls`). NAV can exceed that cash while
        // clients' open positions sit underwater; the unrealised excess is not paid either.
        // TODO: maintain the account reserve incrementally before account-count growth makes
        // epoch close exceed its sequencing latency budget.
        if self.protocol_version >= ProtocolVersion::V75 {
            self.reversal_shortfalls()
        } else if self.protocol_version >= ProtocolVersion::V28 {
            self.accounts.values().fold(
                (0i128, 0i128, self.unfinalised_house_deposits.raw()),
                |(reserved, uncollectible, principal), account| {
                    let deficit = if self.protocol_version >= ProtocolVersion::V29 {
                        self.equity_and_margin(account, None).0.raw().saturating_neg().max(0)
                    } else {
                        0
                    };
                    (
                        reserved
                            .saturating_add(account.unfinalised_deposits.raw())
                            .saturating_add(account.balance.raw().saturating_neg().max(0)),
                        uncollectible.saturating_add(deficit),
                        principal.saturating_add(account.unfinalised_deposits.raw()),
                    )
                },
            )
        } else {
            (self.accounts.values().map(|account| account.unfinalised_deposits).sum::<Fixed>().raw(), 0, 0)
        }
    }

    /// From `V75`, `excess_over`'s cash reserve, NAV deduction and principal, in raw `Fixed` units. A
    /// client's unfinalised deposit is the client's own cash, and a reversal takes it back from that
    /// cash first, so the house bears only what each account's cash, and its marked equity, would fall
    /// below zero by were every unfinalised deposit of its to reverse: `max(0, locked - cash)` from
    /// house cash and `max(0, locked - equity)` from house NAV. With nothing locked these are `V28`'s
    /// negative cash and `V29`'s negative equity. The house's own unfinalised deposits sit in house
    /// cash, and stay its principal whole.
    fn reversal_shortfalls(&self) -> (i128, i128, i128) {
        let (reserved, uncollectible) =
            self.accounts.values().fold((0i128, 0i128), |(reserved, uncollectible), account| {
                let locked = account.unfinalised_deposits.raw();
                let equity = self.equity_and_margin(account, None).0.raw();
                (
                    reserved.saturating_add(locked.saturating_sub(account.balance.raw()).max(0)),
                    uncollectible.saturating_add(locked.saturating_sub(equity).max(0)),
                )
            });
        (reserved, uncollectible, self.unfinalised_house_deposits.raw())
    }
}
