//! The closing price levels trigger on: what a reduce-only close of the whole position fills at.

use crate::Fixed;
use crate::account::Position;
use crate::engine::Engine;
use crate::event::Reject;
use crate::execution::{Exec, PriceGuard};
use crate::levels::LevelKind;
use crate::types::{AccountId, MarketId, Side};

impl Engine {
    /// The reduce-only close of the whole of `pos`, `account`'s position in `market`, as a level fills it.
    pub(crate) fn closing_exec(account: AccountId, market: MarketId, pos: Position, order_id: u64) -> Exec {
        Exec {
            account,
            order_id,
            market,
            side: if pos.qty.is_positive() { Side::Sell } else { Side::Buy },
            qty: pos.qty.abs(),
            reduce_only: true,
            guard: PriceGuard::None,
            liquidation: false,
            close_checked: false,
        }
    }

    /// The price a reduce-only close of the whole of `pos` fills at index `mid`, converting at `fx`, at
    /// stress level `level`, best-price share `share` and toxicity add-on `toxicity_bps`.
    /// Error cases:
    /// - as `Engine::price_with`: with a close-spread ceiling, a pure close always prices
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn closing_price_at(
        &self,
        account: AccountId,
        market: MarketId,
        pos: Position,
        mid: Fixed,
        fx: Option<Fixed>,
        level: usize,
        share: Option<i64>,
        toxicity_bps: i64,
    ) -> Result<Fixed, Reject> {
        let exec = Self::closing_exec(account, market, pos, 0);
        self.price_with(&exec, pos, mid, fx, level, share, toxicity_bps).map(|priced| priced.price)
    }

    /// The price closing `account`'s whole position in `market` fills at now, at the live index with the
    /// stress level and the account's tier and toxicity as they stand, or `None` while the market cannot
    /// trade or the account holds no position there.
    pub(crate) fn live_closing_price(&self, account: AccountId, market: MarketId) -> Option<Fixed> {
        let pos = self.accounts.get(&account)?.positions.get(&market).copied()?;
        let exec = Self::closing_exec(account, market, pos, 0);
        let market_state = &self.markets[&market];
        if !market_state.open {
            return None;
        }
        self.price(&exec, self.best_price_share(account)).ok().map(|priced| priced.price)
    }

    /// The price a level on `account`'s position in `market` is measured from when a request sets it: its
    /// whole close at the live index, or while the market is shut, at its last mark and FX mark. `None`
    /// with no position, while the market is open with no live closing price, or with no mark to close at.
    pub(crate) fn levels_reference(&self, account: AccountId, market: MarketId) -> Option<Fixed> {
        if let Some(price) = self.live_closing_price(account, market) {
            return Some(price);
        }
        let state = &self.markets[&market];
        if state.open {
            return None;
        }
        let pos = self.accounts.get(&account)?.positions.get(&market).copied()?;
        let mark = state.mark?;
        let fx = match state.spec.quote_currency.fx_market() {
            None => None,
            Some(fx_market) if fx_market == market => Some(mark),
            Some(_) => Some(state.fx_mark?),
        };
        let (share, toxicity) = (self.best_price_share(account), self.toxicity_add_on_bps(account));
        self.closing_price_at(account, market, pos, mark, fx, state.stress.level, share, toxicity).ok()
    }

    /// The level of `account`'s position in `market` its closing price at the live index reaches, the stop
    /// loss first, or `None` while it reaches neither or the market cannot trade.
    pub(crate) fn reached_level(&self, account: AccountId, market: MarketId) -> Option<LevelKind> {
        let levels = *self.accounts.get(&account)?.levels.get(&market)?;
        let long = self.accounts[&account].positions.get(&market)?.qty.is_positive();
        let price = self.live_closing_price(account, market)?;
        let reached = |kind: LevelKind, level: Fixed| match (kind, long) {
            (LevelKind::StopLoss, true) | (LevelKind::TakeProfit, false) => price <= level,
            (LevelKind::TakeProfit, true) | (LevelKind::StopLoss, false) => price >= level,
        };
        [LevelKind::StopLoss, LevelKind::TakeProfit]
            .into_iter()
            .find(|kind| levels.get(*kind).is_some_and(|level| reached(*kind, level)))
    }
}
