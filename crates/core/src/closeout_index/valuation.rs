//! What the close-out check values each held market at, and an account's standing there, with every
//! step checked: where the check's own arithmetic would overflow or panic, the account is reported
//! below its level, so the check site calls `close_out` and meets that failure where it does today.

use std::collections::BTreeMap;

use crate::Fixed;
use crate::account::{Account, Position};
use crate::engine::{Engine, MarketState};
use crate::fixed::Rounding;
use crate::market::{QuoteCurrency, Tier, floored_margin};
use crate::protocol::ProtocolVersion;
use crate::types::MarketId;

/// The largest magnitude a key's sums may reach: a quarter of `Fixed`'s range, so no running sum of
/// the check's folds, at any point inside a key's ranges, overflows.
pub(super) const LIMIT: Fixed = Fixed::from_raw(i128::MAX / 4);

/// What the check values every position in a market at: from `V6` the index a close-out would fill at
/// and its FX index while the market can trade, otherwise its mark (`None`: each position at its
/// entry, before the market's first mark) and FX mark. A USD market's FX index is always `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Valuation {
    pub(super) price: Option<Fixed>,
    pub(super) fx: Option<Fixed>,
}

/// Each valued market's valuation.
pub(super) type Valuations = BTreeMap<MarketId, Valuation>;

/// An account's standing at the valuations of a refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Assessment {
    /// Below its level, or not valued without an overflow: the check site calls `close_out`.
    Below,
    /// At or above its level; `keyable` when every held market has a point valuation and its headroom
    /// covers at least a basis point of its notional.
    Above { keyable: bool },
}

impl Valuation {
    /// `price` and `fx`, with a USD market's FX index dropped, as the conversion ignores it.
    pub(super) fn of(currency: QuoteCurrency, price: Option<Fixed>, fx: Option<Fixed>) -> Self {
        Valuation { price, fx: if currency.is_usd() { None } else { fx } }
    }

    /// What the check values every position in `market` at, given the index a fill would take there
    /// now and its FX index, `live`, while it can trade (`Engine::tradeable_index`): those, or else its
    /// mark and FX mark.
    pub(crate) fn at(market: &MarketState, live: Option<(Fixed, Option<Fixed>)>) -> Self {
        let (price, fx) = match live {
            Some((mid, fx)) => (Some(mid), fx),
            None => (market.mark, market.fx_mark),
        };
        Valuation::of(market.spec.quote_currency, price, fx)
    }

    /// The price and FX index `pos` is valued at: its entry while the market has no price.
    pub(crate) fn price_for(&self, pos: &Position) -> (Fixed, Option<Fixed>) {
        (self.price.unwrap_or(pos.entry), self.fx)
    }

    /// Whether one point values every position in a market quoted in `currency`: a price, and an FX
    /// index for a market quoted outside USD.
    pub(super) fn is_point(&self, currency: QuoteCurrency) -> bool {
        self.price.is_some() && (currency.is_usd() || self.fx.is_some())
    }
}

impl Engine {
    /// The valuation `next_closeout` gives every position in `market_id` now. The market is listed.
    pub(crate) fn closeout_valuation(&self, market_id: MarketId) -> Valuation {
        let live = if self.protocol_version >= ProtocolVersion::V6 { self.tradeable_index(market_id) } else { None };
        Valuation::at(&self.markets[&market_id], live)
    }

    /// The valuation a weekend gap's close-out gives every position in `market_id`: its mark (or each
    /// position's entry before it has one) and FX mark. The market is listed.
    pub(super) fn weekend_valuation(&self, market_id: MarketId) -> Valuation {
        let market = &self.markets[&market_id];
        Valuation::of(market.spec.quote_currency, market.mark, market.fx_mark)
    }

    /// Whether `acct` is below `level_bps` of its margin requirement at `valuations`, as
    /// `next_closeout` judges it: equity and requirement folded in market order, a market not
    /// listed skipped, and a requirement of zero never below. A step that would overflow or panic
    /// reports it below.
    pub(super) fn assess(&self, acct: &Account, valuations: &Valuations, level_bps: i64) -> Assessment {
        let charge = self.charge_rounding();
        let (mut equity, mut margin, mut notional) = (acct.balance, Fixed::ZERO, Fixed::ZERO);
        let mut keyable = true;
        for (id, pos) in &acct.positions {
            // `next_closeout` reads the trading state of every market held, so one not listed panics.
            let (Some(market), Some(valuation)) = (self.markets.get(id), valuations.get(id)) else {
                return Assessment::Below;
            };
            let currency = market.spec.quote_currency;
            keyable &= valuation.is_point(currency);
            let (price, fx) = valuation.price_for(pos);
            let summed = position_value(self.protocol_version, charge, currency, pos, price, fx)
                .and_then(|(pnl, held)| Some((pnl, held, margin_of(&market.spec.margin_tiers, held, 0, charge)?)))
                .and_then(|(pnl, held, required)| {
                    Some((add(equity, pnl)?, add(margin, required)?, add(notional, held)?))
                });
            let Some(sums) = summed else { return Assessment::Below };
            (equity, margin, notional) = sums;
        }
        if margin.is_zero() {
            return Assessment::Above { keyable: false };
        }
        let Some(level) = at_level(margin, level_bps, charge) else { return Assessment::Below };
        if equity < level {
            return Assessment::Below;
        }
        let worth = sub(equity, level).zip(notional.checked_mul_div(Fixed::ONE, Fixed::from_int(10_000), Rounding::Up));
        Assessment::Above { keyable: keyable && worth.is_some_and(|(headroom, bp)| headroom >= bp) }
    }
}

/// A position's PnL and notional in USD at `price` and `fx`, as `Engine::equity_and_margin_at` values
/// it, or `None` where that overflows or panics.
pub(super) fn position_value(
    version: ProtocolVersion,
    charge: Rounding,
    currency: QuoteCurrency,
    pos: &Position,
    price: Fixed,
    fx: Option<Fixed>,
) -> Option<(Fixed, Fixed)> {
    let quantity = Fixed::from_raw(pos.qty.raw().checked_abs()?);
    let notional = currency.checked_to_usd(quantity.checked_mul_div(price, Fixed::ONE, charge)?, fx, Rounding::Up)?;
    let pnl = currency.checked_to_usd(pos.checked_unrealised(price, version)?, fx, Rounding::Down)?;
    Some((pnl, notional))
}

/// The requirement for `notional` under margin bands `tiers` with each band at least `floor_bps`, as
/// `MarketSpec::floored_margin_for` gives it, or `None` past `LIMIT`, below which it cannot overflow:
/// each band charges at most its slice and one unit.
pub(super) fn margin_of(tiers: &[Tier], notional: Fixed, floor_bps: i64, charge: Rounding) -> Option<Fixed> {
    (notional <= LIMIT).then(|| floored_margin(tiers, notional, floor_bps, charge))
}

/// `margin` at the close-out level, rounded as a requirement is.
pub(super) fn at_level(margin: Fixed, level_bps: i64, charge: Rounding) -> Option<Fixed> {
    margin.checked_mul_div(Fixed::from_raw(i128::from(level_bps)), Fixed::from_raw(10_000), charge)
}

/// Checked addition.
pub(super) fn add(a: Fixed, b: Fixed) -> Option<Fixed> {
    a.raw().checked_add(b.raw()).map(Fixed::from_raw)
}

/// Checked subtraction.
pub(super) fn sub(a: Fixed, b: Fixed) -> Option<Fixed> {
    a.raw().checked_sub(b.raw()).map(Fixed::from_raw)
}

/// Checked absolute value.
pub(super) fn abs(a: Fixed) -> Option<Fixed> {
    a.raw().checked_abs().map(Fixed::from_raw)
}
