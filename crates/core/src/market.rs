//! Market listings. Every field here is protocol: listings enter the engine as sequenced admin
//! commands, so the public log records exactly which rules applied to every fill.

use std::iter::once;

use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::fixed::Rounding;
use crate::futures::{FuturesContract, validate_contracts};
use crate::session::Schedule;
use crate::types::{AssetClass, MarketId};

/// The most any per-trade rate may be: 100%.
const MAX_RATE_BPS: i64 = 10_000;
/// The most a house exposure cap may be: ten times house NAV.
const MAX_HOUSE_CAP_BPS: i64 = 100_000;
/// The FX pairs the engine converts at, by symbol.
const EUR_USD: &str = "EUR-USD";
const GBP_USD: &str = "GBP-USD";
const USD_JPY: &str = "USD-JPY";
/// The most one unit of a quote currency may be worth: 10 USD, above any real currency's value. It
/// bounds how far a conversion scales an amount up; an FX index past it converts nothing.
const MAX_USD_PER_UNIT: Fixed = Fixed::from_int(10);

/// A market listing. Prices are in the quote currency. Every notional bound here is USD, and so is
/// every amount the market moves for an account or the house: PnL and financing accrue in the quote
/// currency and convert at the FX index, rounding toward the house. What a trader is credited or
/// may use rounds down; what a trader is charged, and the exposure and limits that guard the house,
/// round up.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketSpec {
    pub id: MarketId,
    pub symbol: String,
    pub asset_class: AssetClass,
    /// USD unless declared. Left out of the encoding while USD, so USD listings keep their roots.
    #[serde(default, skip_serializing_if = "QuoteCurrency::is_usd")]
    pub quote_currency: QuoteCurrency,
    pub schedule: Schedule,
    /// Margin bands by position notional (USD): each band's rate applies to the slice of notional
    /// inside it.
    pub margin_tiers: Vec<Tier>,
    /// Size spread bands by position notional (USD). Spread is charged only on the slice of a trade
    /// that grows the account's absolute position notional through each band, so normal size pays
    /// zero and splitting a large order doesn't avoid it.
    pub spread_tiers: Vec<Tier>,
    pub price_rules: PriceRules,
    /// Largest adverse move from the price the client saw that a market order will still fill at.
    /// Beyond this the order is declined.
    pub max_slippage_bps: i64,
    pub financing: FinancingSpec,
    /// Cap on the house's net notional in this market, in bps of house NAV.
    pub house_cap_bps: i64,
    /// Smallest notional (USD) of an order that adds risk, measured on the whole order.
    #[serde(default)]
    pub min_order_notional: Fixed,
    /// From protocol `V6`, the least the market cap may be, in USD notional, however small or
    /// negative house NAV is. Zero means none, and is left out of the encoding.
    #[serde(default, skip_serializing_if = "Fixed::is_zero")]
    pub house_cap_floor: Fixed,
    /// From protocol `V21`, the futures contracts the market prices from, in roll order, with its
    /// index roll-adjusted across them (`futures`). Empty for a market priced from spot quotes, and
    /// left out of the encoding then.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contracts: Vec<FuturesContract>,
}

/// The currency a market's prices are quoted in. Any other than USD names the FX market whose index
/// converts it to USD: the listed market for its pair. USD-JPY may name itself, for its own yen PnL.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "currency", rename_all = "snake_case", deny_unknown_fields)]
pub enum QuoteCurrency {
    #[default]
    Usd,
    /// Converted at a EUR-USD index: USD per euro.
    Eur { fx_market: MarketId },
    /// Converted at a GBP-USD index: USD per pound.
    Gbp { fx_market: MarketId },
    /// Converted at a USD-JPY index: yen per USD.
    Jpy { fx_market: MarketId },
}

/// A band in a tier table. `up_to: None` means unbounded and must be last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tier {
    #[serde(default)]
    pub up_to: Option<Fixed>,
    pub bps: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceRules {
    /// A source quote older than this (relative to the sequenced time) is ignored.
    pub max_age_ms: i64,
    /// If fresh sources disagree by more than this the market has no price.
    pub max_divergence_bps: i64,
    pub min_sources: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinancingSpec {
    /// Key into the benchmark rate table, e.g. "SOFR", "SONIA", "ESTR".
    pub benchmark: String,
    /// Annual markup over the benchmark: longs pay benchmark + markup, shorts receive
    /// benchmark - markup (and pay if that is negative).
    pub markup_bps: i64,
    /// 360 or 365.
    pub day_basis: i64,
}

impl MarketSpec {
    /// Checks the listing is internally consistent and every rate is in range.
    /// Error cases:
    /// - an empty symbol, or a schedule that can never open
    /// - a tier table that is empty, out of order, or not unbounded at the end
    /// - a margin rate outside 1..=10000 bps, or a spread rate outside 0..=10000 bps
    /// - `min_sources` of zero, a non-positive `max_age_ms`, or a divergence outside 0..=10000 bps
    /// - a slippage bound outside 0..=10000 bps, or a house cap outside 1..=100000 bps
    /// - a financing markup outside ±10000 bps, or a day basis other than 360 or 365
    /// - a minimum order notional or a house cap floor below zero or above `Fixed::MAX_INPUT`
    /// - futures contracts that are unnamed, named twice, or out of roll order
    pub fn validate(&self) -> Result<(), String> {
        if self.symbol.is_empty() {
            return Err("symbol is empty".into());
        }
        self.schedule.validate()?;
        validate_tiers("margin_tiers", &self.margin_tiers, 1)?;
        validate_tiers("spread_tiers", &self.spread_tiers, 0)?;
        let rules = &self.price_rules;
        if rules.min_sources == 0 || rules.max_age_ms <= 0 {
            return Err("price_rules needs min_sources >= 1 and max_age_ms > 0".into());
        }
        if !(0..=MAX_RATE_BPS).contains(&rules.max_divergence_bps)
            || !(0..=MAX_RATE_BPS).contains(&self.max_slippage_bps)
        {
            return Err("max_divergence_bps and max_slippage_bps must be in 0..=10000".into());
        }
        if !(1..=MAX_HOUSE_CAP_BPS).contains(&self.house_cap_bps) {
            return Err("house_cap_bps must be in 1..=100000".into());
        }
        if !(-MAX_RATE_BPS..=MAX_RATE_BPS).contains(&self.financing.markup_bps) {
            return Err("financing.markup_bps must be within ±10000".into());
        }
        if !matches!(self.financing.day_basis, 360 | 365) {
            return Err("financing.day_basis must be 360 or 365".into());
        }
        if self.min_order_notional.is_negative() || self.min_order_notional > Fixed::MAX_INPUT {
            return Err("min_order_notional out of range".into());
        }
        if self.house_cap_floor.is_negative() || self.house_cap_floor > Fixed::MAX_INPUT {
            return Err("house_cap_floor out of range".into());
        }
        validate_contracts(&self.contracts)
    }

    /// Margin required for a position of `notional` (either sign), each band's charge rounded as
    /// `rounding` says.
    pub fn margin_for(&self, notional: Fixed, rounding: Rounding) -> Fixed {
        staircase(&self.margin_tiers, Fixed::ZERO, notional.abs(), rounding)
    }

    /// Checks this listing's quote currency against the FX pairs, given the symbol of each market
    /// already listed. A pair's own listing (EUR-USD, GBP-USD, or USD-JPY, by symbol) is quoted in its
    /// second currency: USD, or yen at its own index for USD-JPY. Any other listing quoted outside USD
    /// converts at the listed market for its pair.
    /// Error cases:
    /// - a pair's listing quoted in a currency other than the pair's second
    /// - a quote currency other than USD whose FX market is not the listed market for its pair
    pub fn check_fx_market<'m>(&'m self, listed_symbol: impl Fn(MarketId) -> Option<&'m str>) -> Result<(), String> {
        let own = match self.symbol.as_str() {
            EUR_USD | GBP_USD => Some(QuoteCurrency::Usd),
            USD_JPY => Some(QuoteCurrency::Jpy { fx_market: self.id }),
            _ => None,
        };
        if let Some(own) = own
            && own != self.quote_currency
        {
            return Err(format!("{} must be quoted as {own:?}", self.symbol));
        }
        let (Some(fx_market), Some(pair)) = (self.quote_currency.fx_market(), self.quote_currency.pair()) else {
            return Ok(());
        };
        let symbol = if fx_market == self.id { Some(self.symbol.as_str()) } else { listed_symbol(fx_market) };
        let is_pair = symbol == Some(pair);
        if !is_pair {
            return Err(format!("fx_market {fx_market} is not the listed {pair} market"));
        }
        Ok(())
    }
}

impl QuoteCurrency {
    /// True for USD, which needs no conversion.
    pub fn is_usd(&self) -> bool {
        *self == QuoteCurrency::Usd
    }

    /// The symbol of the FX market whose index converts this currency, or `None` for USD.
    fn pair(self) -> Option<&'static str> {
        match self {
            QuoteCurrency::Usd => None,
            QuoteCurrency::Eur { .. } => Some(EUR_USD),
            QuoteCurrency::Gbp { .. } => Some(GBP_USD),
            QuoteCurrency::Jpy { .. } => Some(USD_JPY),
        }
    }

    /// The market whose index converts this currency to USD, or `None` for USD.
    pub fn fx_market(self) -> Option<MarketId> {
        match self {
            QuoteCurrency::Usd => None,
            QuoteCurrency::Eur { fx_market } | QuoteCurrency::Gbp { fx_market } | QuoteCurrency::Jpy { fx_market } => {
                Some(fx_market)
            }
        }
    }

    /// `amount` in this currency, in USD at `fx`, its FX market's index: times a EUR-USD or GBP-USD
    /// index, over a USD-JPY one, rounded as `rounding` says. A USD amount is returned as it is.
    /// Panics on a non-zero amount in another currency with no index, which callers rule out.
    pub fn to_usd(self, amount: Fixed, fx: Option<Fixed>, rounding: Rounding) -> Fixed {
        match (self, fx) {
            (QuoteCurrency::Usd, _) => amount,
            (QuoteCurrency::Eur { .. } | QuoteCurrency::Gbp { .. }, Some(fx)) => amount.mul_rounded(fx, rounding),
            (QuoteCurrency::Jpy { .. }, Some(fx)) => amount.div_rounded(fx, rounding),
            (_, None) => {
                assert!(amount.is_zero(), "{amount} in {self:?} has no FX index to convert at");
                amount
            }
        }
    }

    /// `amount` in USD, in this currency at `fx`: the inverse of [`QuoteCurrency::to_usd`], rounded
    /// as `rounding` says.
    /// Panics on a non-zero amount in another currency with no index, which callers rule out.
    pub fn from_usd(self, amount: Fixed, fx: Option<Fixed>, rounding: Rounding) -> Fixed {
        match (self, fx) {
            (QuoteCurrency::Usd, _) => amount,
            (QuoteCurrency::Eur { .. } | QuoteCurrency::Gbp { .. }, Some(fx)) => amount.div_rounded(fx, rounding),
            (QuoteCurrency::Jpy { .. }, Some(fx)) => amount.mul_rounded(fx, rounding),
            (_, None) => {
                assert!(amount.is_zero(), "{amount} USD has no FX index to convert to {self:?} at");
                amount
            }
        }
    }

    /// Whether `fx`, a valid index of this currency's FX market, can convert it: one unit of the
    /// currency is worth at most `MAX_USD_PER_UNIT`.
    pub fn converts_at(self, fx: Fixed) -> bool {
        self.to_usd(Fixed::ONE, Some(fx), Rounding::Up) <= MAX_USD_PER_UNIT
    }
}

/// Checks a tier table: non-empty, strictly increasing bounds, unbounded last band, and every rate
/// in `min_bps..=10000`.
/// Error cases:
/// - an empty table
/// - a bounded last band, or an unbounded band before the last
/// - a bound that does not exceed the previous one
/// - a rate outside `min_bps..=10000`
fn validate_tiers(name: &str, tiers: &[Tier], min_bps: i64) -> Result<(), String> {
    let Some((last, bounded)) = tiers.split_last() else {
        return Err(format!("{name} is empty"));
    };
    if last.up_to.is_some() {
        return Err(format!("{name}: last tier must be unbounded"));
    }
    let bounds: Vec<Fixed> = bounded
        .iter()
        .map(|t| t.up_to.ok_or_else(|| format!("{name}: only the last tier may be unbounded")))
        .collect::<Result<_, _>>()?;
    if bounds.first().is_some_and(|b| !b.is_positive()) || bounds.windows(2).any(|w| w[1] <= w[0]) {
        return Err(format!("{name}: bands must be positive and strictly increasing"));
    }
    if tiers.iter().any(|t| !(min_bps..=MAX_RATE_BPS).contains(&t.bps)) {
        return Err(format!("{name}: rates must be in {min_bps}..=10000 bps"));
    }
    Ok(())
}

/// Cost of moving from notional `from` to `to` (`from <= to`) through banded rates: each band's bps
/// applies to the slice of `[from, to]` inside it, and each band's charge rounds as `rounding` says.
pub fn staircase(tiers: &[Tier], from: Fixed, to: Fixed, rounding: Rounding) -> Fixed {
    if to <= from {
        return Fixed::ZERO;
    }
    let lows = once(Fixed::ZERO).chain(tiers.iter().filter_map(|t| t.up_to));
    tiers
        .iter()
        .zip(lows)
        .map(|(t, lo)| {
            let start = from.max(lo);
            let end = t.up_to.map_or(to, |hi| to.min(hi));
            if end > start { (end - start).mul_bps_rounded(t.bps, rounding) } else { Fixed::ZERO }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(n: i64) -> Fixed {
        Fixed::from_int(n)
    }

    fn tiers() -> Vec<Tier> {
        vec![
            Tier { up_to: Some(f(1_000_000)), bps: 0 },
            Tier { up_to: Some(f(5_000_000)), bps: 2 },
            Tier { up_to: None, bps: 5 },
        ]
    }

    #[test]
    fn staircase_charges_each_slice_at_its_band_rate() {
        let t = tiers();
        let cost = |from, to| staircase(&t, from, to, Rounding::TowardZero);
        assert_eq!(cost(f(0), f(500_000)), f(0));
        // 1m..2m at 2bps.
        assert_eq!(cost(f(0), f(2_000_000)), f(200));
        // 1m..5m at 2bps = 800, 5m..6m at 5bps = 500.
        assert_eq!(cost(f(0), f(6_000_000)), f(1_300));
        assert_eq!(cost(f(4_000_000), f(6_000_000)), f(700));
        // A shrinking move costs nothing.
        assert_eq!(cost(f(6_000_000), f(4_000_000)), f(0));
    }

    #[test]
    fn staircase_rounds_each_band_charge_as_asked() {
        let t = tiers();
        // One unit of precision either side of 5m charges a fraction of a unit in each band, which
        // truncates to nothing and rounds up to one unit per band. An exact charge does not move.
        let (from, to) = (f(4_999_999) + "0.999999999".parse().unwrap(), f(5_000_000) + Fixed::from_raw(1));
        assert_eq!(staircase(&t, from, to, Rounding::TowardZero), Fixed::ZERO);
        assert_eq!(staircase(&t, from, to, Rounding::Up), Fixed::from_raw(2));
        let half = f(1_000_000) + "0.5".parse().unwrap();
        assert_eq!(staircase(&t, f(0), half, Rounding::Up), "0.0001".parse().unwrap());
    }

    #[test]
    fn splitting_does_not_avoid_size_spread() {
        let t = tiers();
        let cost = |from, to| staircase(&t, from, to, Rounding::TowardZero);
        assert_eq!(cost(f(0), f(6_000_000)), cost(f(0), f(3_000_000)) + cost(f(3_000_000), f(6_000_000)));
    }

    #[test]
    fn tier_validation() {
        assert!(validate_tiers("t", &tiers(), 0).is_ok());
        // A zero rate is fine for spreads but not for margin.
        assert!(validate_tiers("t", &tiers(), 1).is_err());
        assert!(validate_tiers("t", &[], 0).is_err());
        assert!(validate_tiers("t", &[Tier { up_to: Some(f(1)), bps: 1 }], 0).is_err());
        let backwards =
            vec![Tier { up_to: Some(f(5)), bps: 1 }, Tier { up_to: Some(f(2)), bps: 1 }, Tier { up_to: None, bps: 1 }];
        assert!(validate_tiers("t", &backwards, 0).is_err());
        let huge = vec![Tier { up_to: None, bps: i64::MAX }];
        assert!(validate_tiers("t", &huge, 0).is_err());
    }

    #[test]
    fn unknown_tier_fields_are_rejected() {
        assert!(serde_json::from_str::<Tier>(r#"{"up_to":"1","bps":1,"bsp":2}"#).is_err());
    }

    const EUR: QuoteCurrency = QuoteCurrency::Eur { fx_market: 10 };
    const JPY: QuoteCurrency = QuoteCurrency::Jpy { fx_market: 12 };

    fn d(s: &str) -> Fixed {
        s.parse().unwrap()
    }

    #[test]
    fn euros_multiply_and_yen_divide_by_the_fx_index_rounding_the_way_asked() {
        use Rounding::{Down, Up};
        // (currency, amount, index, rounded down, rounded up): a gain and a loss through a USD-quote
        // pair (EUR-USD, times the index) and a USD-base pair (USD-JPY, over the index).
        let cases = [
            (EUR, "0.000000003", "1.5", "0.000000004", "0.000000005"),
            (EUR, "-0.000000003", "1.5", "-0.000000005", "-0.000000004"),
            (EUR, "200", "1.08", "216", "216"),
            (JPY, "100", "150", "0.666666666", "0.666666667"),
            (JPY, "-100", "150", "-0.666666667", "-0.666666666"),
            (JPY, "-300", "150", "-2", "-2"),
        ];
        for (currency, amount, fx, down, up) in cases {
            let convert = |rounding| currency.to_usd(d(amount), Some(d(fx)), rounding);
            assert_eq!((convert(Down), convert(Up)), (d(down), d(up)), "{currency:?} {amount} at {fx}");
        }
        assert_eq!(EUR.from_usd(d("1"), Some(d("3")), Down), d("0.333333333"));
        assert_eq!(EUR.from_usd(d("1"), Some(d("3")), Up), d("0.333333334"));
        assert_eq!(JPY.from_usd(d("0.000000003"), Some(d("1.5")), Up), d("0.000000005"));
        assert_eq!(JPY.from_usd(d("2"), Some(d("150")), Down), d("300"));
        let usd = QuoteCurrency::Usd;
        assert_eq!((usd.to_usd(d("7"), None, Up), usd.from_usd(d("7"), None, Down)), (d("7"), d("7")));
    }

    #[test]
    fn a_market_with_no_fx_index_converts_only_zero() {
        assert_eq!(EUR.to_usd(Fixed::ZERO, None, Rounding::Down), Fixed::ZERO);
        assert_eq!(JPY.from_usd(Fixed::ZERO, None, Rounding::Up), Fixed::ZERO);
        let converted = std::panic::catch_unwind(|| EUR.to_usd(d("1"), None, Rounding::Down));
        assert!(converted.is_err(), "a non-zero amount without an index must not convert");
    }

    #[test]
    fn an_fx_index_converts_while_a_unit_is_worth_at_most_ten_dollars() {
        assert!(EUR.converts_at(d("10")));
        assert!(!EUR.converts_at(d("10.000000001")));
        // A yen is worth 1 / index USD, so a USD-JPY index converts from 0.1 up.
        assert!(JPY.converts_at(d("0.1")));
        assert!(!JPY.converts_at(d("0.099999999")));
        assert!(JPY.converts_at(Fixed::MAX_INPUT));
        assert!(EUR.converts_at(Fixed::from_raw(1)));
    }

    #[test]
    fn a_usd_listing_leaves_the_currency_out_and_others_name_their_fx_market() {
        let spec = |extra: serde_json::Value| {
            let mut json = serde_json::json!({
                "id": 23, "symbol": "DE40", "asset_class": "index", "schedule": { "kind": "always_open" },
                "margin_tiers": [{ "bps": 50 }], "spread_tiers": [{ "bps": 0 }],
                "price_rules": { "max_age_ms": 1000, "max_divergence_bps": 10, "min_sources": 2 },
                "max_slippage_bps": 10, "financing": { "benchmark": "ESTR", "markup_bps": 250, "day_basis": 360 },
                "house_cap_bps": 5000,
            });
            json.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<MarketSpec>(json)
        };
        let usd = spec(serde_json::json!({})).unwrap();
        assert_eq!(usd.quote_currency, QuoteCurrency::Usd);
        assert!(!serde_json::to_string(&usd).unwrap().contains("quote_currency"));
        let de40 = spec(serde_json::json!({ "quote_currency": { "currency": "eur", "fx_market": 10 } })).unwrap();
        assert_eq!(de40.quote_currency, EUR);
        assert_eq!(de40.quote_currency.fx_market(), Some(10));
        let encoded = serde_json::to_string(&de40).unwrap();
        assert!(encoded.contains(r#""quote_currency":{"currency":"eur","fx_market":10}"#), "{encoded}");
        for bad in [
            serde_json::json!({ "quote_currency": { "currency": "eur" } }),
            serde_json::json!({ "quote_currency": { "currency": "chf", "fx_market": 10 } }),
            serde_json::json!({ "quote_currency": { "currency": "eur", "fx_market": 10, "fx": 1 } }),
        ] {
            assert!(spec(bad.clone()).is_err(), "{bad}");
        }
    }
}
