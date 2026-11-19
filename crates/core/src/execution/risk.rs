//! The margin and house exposure checks a fill that adds risk must pass, and from `V84`, while a grid
//! applies, the margin check a fill that only reduces passes when it leaves the initial requirement higher
//! (#913, D7).

use std::cell::OnceCell;

use crate::Fixed;
use crate::account::{Account, Position};
use crate::engine::{Engine, MarketState};
use crate::event::Reject;
use crate::execution::validation::profit_fee;
use crate::execution::{Exec, Priced};
use crate::fixed::Rounding;
use crate::portfolio::{Basis, PortfolioGrid};
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

/// The house totals a risk check reads. The NAV is taken at once, and the gross exposure on first
/// use, so a run of checks over one state computes each once.
pub(super) struct HouseTotals {
    nav: Fixed,
    gross: OnceCell<Fixed>,
}

/// What a fill leaves its account at, its market at the fill's index and every other at its mark: its
/// equity, its initial requirement with the weekend margins that apply now, and the close-out level of the
/// requirement its close-out reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MarginAfter {
    pub(crate) equity: Fixed,
    pub(crate) initial_margin: Fixed,
    pub(crate) maintenance_margin: Fixed,
}

impl HouseTotals {
    pub(super) fn of(engine: &Engine) -> Self {
        Self { nav: engine.house_nav(), gross: OnceCell::new() }
    }

    fn gross(&self, engine: &Engine) -> Fixed {
        *self.gross.get_or_init(|| engine.house_gross_exposure())
    }
}

impl Engine {
    /// Checks a fill that adds risk against the account's initial requirement and the house's exposure
    /// caps, in USD: realised PnL converts rounded down, and exposure rounded up. `floor_bps` is the
    /// margin floor of the account's tier (`Engine::margin_floor_of`), which each margin band of the
    /// initial requirement charges at least. From `V84`, while a grid applies, the initial requirement is
    /// the portfolio requirement at initial bands (`Engine::equity_and_initial_requirement_at`).
    /// Error cases:
    /// - the account's equity after the fill would be below its initial requirement
    /// - from `V60`, below it with the weekend margins that apply now
    /// - the fill would push the house's net exposure past the market cap or the total cap
    pub(super) fn check_risk(
        &self,
        exec: &Exec,
        priced: &Priced,
        house: &HouseTotals,
        floor_bps: i64,
    ) -> Result<(), Reject> {
        let params = self.params.as_ref().expect("callers check params are set");
        let market = &self.markets[&exec.market];
        let usd = |amount, rounding| market.spec.quote_currency.to_usd(amount, priced.fx, rounding);

        let sim = self.account_after(exec, priced);
        let price_of = fill_prices(exec.market, priced);
        match self.initial_portfolio_rules() {
            Some(grid) => {
                let basis = Basis::Initial { floor_bps, weekend: true, shut_credited: false };
                let (equity, required) = self.equity_and_portfolio_requirement(&sim, grid, basis, &[], price_of);
                self.check_portfolio_initial(&sim, grid, floor_bps, price_of, equity, required)?;
            }
            None => self.check_initial(&sim, floor_bps, price_of)?,
        }

        let nav = house.nav;
        let before = usd(market.client_qty.abs().mul(priced.mid), Rounding::Up);
        let after = usd((market.client_qty + priced.delta).abs().mul(priced.mid), Rounding::Up);
        if after > before {
            // `house_gross_exposure` values this market at its marks; swap that term for `after`.
            let exposure = |mark: Fixed| market.usd_at_mark(market.client_qty.abs().mul(mark), Rounding::Up);
            let at_mark = market.mark.map_or(Fixed::ZERO, exposure);
            let total_after = house.gross(self) - at_mark + after;
            if after > self.market_cap(&market.spec, nav) || total_after > self.total_cap(params, nav) {
                return Err(Reject::HouseCapacity);
            }
        }
        Ok(())
    }

    /// From `V84`, while a grid applies, checks a fill that only reduces, as new risk is, when it leaves its
    /// account's initial requirement higher than before it, both with the fill's market at its index and
    /// every other at its mark (#913, D7): closing one leg of a hedge gives up the group's credit, and can
    /// raise the requirement past the equity. Whether the fill raises it is judged with every group's
    /// credit kept, a member that cannot trade valued at its last mark, so a group whose initial credit a
    /// shut member has suspended (D10) is not closed down to a lone leg the account cannot carry; the
    /// equity is then held to the initial requirement after the fill, which credits no such group. A close
    /// the check passes can still leave the account below its close-out level, which the check after the
    /// command's fills closes out (`check_filled_accounts`).
    /// Error cases:
    /// - the account's equity after the fill would be below the initial requirement, where the fill raises
    ///   the requirement
    /// - below it with the weekend margins that apply now
    pub(super) fn check_close(&self, exec: &Exec, priced: &Priced, floor_bps: i64) -> Result<(), Reject> {
        let Some(grid) = self.initial_portfolio_rules() else { return Ok(()) };
        let price_of = fill_prices(exec.market, priced);
        let kept = Basis::Initial { floor_bps, weekend: true, shut_credited: true };
        let acct = &self.accounts[&exec.account];
        let (_, before) = self.equity_and_portfolio_requirement(acct, grid, kept, &[], price_of);
        let sim = self.account_after(exec, priced);
        let (_, raised) = self.equity_and_portfolio_requirement(&sim, grid, kept, &[], price_of);
        if raised <= before {
            return Ok(());
        }
        let basis = Basis::Initial { floor_bps, weekend: true, shut_credited: false };
        let (equity, required) = self.equity_and_portfolio_requirement(&sim, grid, basis, &[], price_of);
        self.check_portfolio_initial(&sim, grid, floor_bps, price_of, equity, required)
    }

    /// What `exec`'s fill at `priced` leaves its account at (`MarginAfter`), with `floor_bps` its tier's
    /// margin floor: a fill that leaves the account flat with a negative balance leaves it at none, which
    /// the house writes off (`Engine::settle_bad_debt`).
    pub(super) fn margin_after(&self, exec: &Exec, priced: &Priced, floor_bps: i64) -> MarginAfter {
        let sim = self.account_after(exec, priced);
        let price_of = fill_prices(exec.market, priced);
        let (equity, initial_margin) = self.equity_and_initial_requirement_at(&sim, floor_bps, price_of);
        let flat = sim.positions.values().all(|pos| pos.qty.is_zero());
        let equity = if flat { equity.max(Fixed::ZERO) } else { equity };
        let (_, requirement) = match self.portfolio_rules() {
            Some(grid) => self.equity_and_portfolio_requirement(&sim, grid, Basis::Market, &[], price_of),
            None => self.equity_and_margin_at(&sim, price_of),
        };
        let maintenance_margin = match &self.params {
            Some(params) => requirement.mul_bps_rounded(params.closeout_level_bps, self.charge_rounding()),
            None => requirement,
        };
        MarginAfter { equity, initial_margin, maintenance_margin }
    }

    /// The account `exec`'s fill at `priced` leaves, as margin reads it: its cash, with what the fill
    /// realises less its profit fee, and its positions; the resting orders are not copied.
    fn account_after(&self, exec: &Exec, priced: &Priced) -> Account {
        let market = &self.markets[&exec.market];
        let acct = &self.accounts[&exec.account];
        let mut sim = Account { balance: acct.balance, positions: acct.positions.clone(), ..Account::default() };
        let version = self.protocol_version;
        let realised = sim.positions.entry(exec.market).or_default().apply(priced.delta, priced.price, version);
        let realised = market.spec.quote_currency.to_usd(realised, priced.fx, Rounding::Down);
        // From `V55` the fee is the closing slice's, at its hold.
        let (_, fee_bps) = self.close_fee_terms(exec.account, exec.market);
        sim.balance += realised - profit_fee(realised, fee_bps, version);
        sim
    }

    /// Checks `sim`, an account after a fill valued at `price_of`'s prices, against its initial requirement
    /// with each band at least `floor_bps`, and from `V60` with the weekend margins that apply now.
    /// Error cases:
    /// - equity below the initial requirement
    /// - from `V60`, below it with the weekend margins that apply now
    fn check_initial(
        &self,
        sim: &Account,
        floor_bps: i64,
        price_of: impl Fn(MarketId, &MarketState, &Position) -> (Fixed, Option<Fixed>) + Copy,
    ) -> Result<(), Reject> {
        let (equity, required) = self.equity_and_requirement_at(sim, floor_bps, price_of);
        if equity < required {
            return Err(Reject::InsufficientMargin { equity, required });
        }
        // From `V60`, risk added in any market meets the weekend margin of each market whose window
        // covers now.
        if self.protocol_version >= ProtocolVersion::V60 {
            let required = required + self.weekend_margin_extra(sim, self.now, floor_bps, price_of);
            if equity < required {
                return Err(Reject::WeekendMargin { equity, required });
            }
        }
        Ok(())
    }

    /// Checks `equity` against `required`, `sim`'s portfolio requirement under `grid` at initial bands with
    /// the weekend margins that apply now, each position valued at `price_of`'s prices; a refusal names the
    /// requirement without the weekend margins when equity is below that too.
    /// Error cases:
    /// - equity below the requirement without the weekend margins
    /// - equity below `required` alone
    fn check_portfolio_initial(
        &self,
        sim: &Account,
        grid: &PortfolioGrid,
        floor_bps: i64,
        price_of: impl Fn(MarketId, &MarketState, &Position) -> (Fixed, Option<Fixed>) + Copy,
        equity: Fixed,
        required: Fixed,
    ) -> Result<(), Reject> {
        if equity >= required {
            return Ok(());
        }
        let plain = Basis::Initial { floor_bps, weekend: false, shut_credited: false };
        let (_, without_weekend) = self.equity_and_portfolio_requirement(sim, grid, plain, &[], price_of);
        if equity < without_weekend {
            return Err(Reject::InsufficientMargin { equity, required: without_weekend });
        }
        Err(Reject::WeekendMargin { equity, required })
    }
}

/// The prices a fill's checks value its account at: its own market at the fill's index and FX index, and
/// every other at its mark and FX mark, or its entry before the market is marked.
fn fill_prices(
    filled: MarketId,
    priced: &Priced,
) -> impl Fn(MarketId, &MarketState, &Position) -> (Fixed, Option<Fixed>) + Copy {
    let (mid, fx) = (priced.mid, priced.fx);
    move |id: MarketId, market: &MarketState, pos: &Position| {
        if id == filled { (mid, fx) } else { (market.mark.unwrap_or(pos.entry), market.fx_mark) }
    }
}
