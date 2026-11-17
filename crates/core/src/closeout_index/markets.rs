//! Each market's part of the close-out index: the rules its keys hold under, the valuation every key
//! in it contains, the keyed holders by the ends of their ranges, and the unkeyed holders.

use std::collections::BTreeSet;
use std::ops::Bound;

use crate::Fixed;
use crate::closeout_index::key::KeyedPosition;
use crate::closeout_index::valuation::Valuation;
use crate::market::{MarketSpec, QuoteCurrency, Tier};
use crate::types::AccountId;

const FIRST_ACCOUNT: AccountId = AccountId([0; 20]);
const LAST_ACCOUNT: AccountId = AccountId([0xff; 20]);

/// What a market's keys are computed under, from its listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MarketRules {
    pub(super) margin_tiers: Vec<Tier>,
    pub(super) quote_currency: QuoteCurrency,
    pub(super) weekend_margin_bps: i64,
}

/// A market's keyed holders, by the ends of their ranges, and its unkeyed ones.
#[derive(Debug, Default)]
pub(super) struct MarketKeys {
    /// The listing's rules the keys hold under; `None` for a market not listed.
    pub(super) rules: Option<MarketRules>,
    /// The valuation every key in the market contains; `None` until the market is first valued.
    pub(super) seen: Option<Valuation>,
    /// Keyed holders by their price range's ends; a lower end of zero is not entered.
    price_low: BTreeSet<(Fixed, AccountId)>,
    price_high: BTreeSet<(Fixed, AccountId)>,
    /// The same for the FX index of a market quoted outside USD.
    fx_low: BTreeSet<(Fixed, AccountId)>,
    fx_high: BTreeSet<(Fixed, AccountId)>,
    /// Unkeyed holders, and those of them that must be closed out.
    pub(super) unkeyed: BTreeSet<AccountId>,
    pub(super) below: BTreeSet<AccountId>,
    /// Keyed holders whose key does not show them meeting the requirement with weekend margins.
    pub(super) weekend_weak: BTreeSet<AccountId>,
}

impl MarketRules {
    /// The rules of `spec`.
    pub(super) fn of(spec: &MarketSpec) -> Self {
        MarketRules {
            margin_tiers: spec.margin_tiers.clone(),
            quote_currency: spec.quote_currency,
            weekend_margin_bps: spec.weekend_margin_bps,
        }
    }

    /// Whether the rules hold the same as `spec`'s.
    pub(super) fn matches(&self, spec: &MarketSpec) -> bool {
        self.margin_tiers == spec.margin_tiers
            && self.quote_currency == spec.quote_currency
            && self.weekend_margin_bps == spec.weekend_margin_bps
    }
}

impl MarketKeys {
    /// An empty part for a market with `spec`, or one not listed.
    pub(super) fn new(spec: Option<&MarketSpec>) -> Self {
        MarketKeys { rules: spec.map(MarketRules::of), ..MarketKeys::default() }
    }

    /// Enters `account`'s keyed position in the range sets.
    pub(super) fn enter(&mut self, account: AccountId, pos: &KeyedPosition) {
        if pos.price.low.is_positive() {
            self.price_low.insert((pos.price.low, account));
        }
        self.price_high.insert((pos.price.high, account));
        if let Some(fx) = pos.fx {
            self.fx_low.insert((fx.low, account));
            self.fx_high.insert((fx.high, account));
        }
    }

    /// Removes `account`'s keyed position from the range sets.
    pub(super) fn leave(&mut self, account: AccountId, pos: &KeyedPosition) {
        self.price_low.remove(&(pos.price.low, account));
        self.price_high.remove(&(pos.price.high, account));
        if let Some(fx) = pos.fx {
            self.fx_low.remove(&(fx.low, account));
            self.fx_high.remove(&(fx.high, account));
        }
        self.weekend_weak.remove(&account);
    }

    /// The keyed holders whose ranges `valuation` lies outside: every one when it is no single point.
    pub(super) fn crossed(&self, valuation: Valuation) -> BTreeSet<AccountId> {
        let (by_price, by_fx) = self.crossed_by(valuation);
        by_price.into_iter().chain(by_fx).collect()
    }

    /// The keyed holders whose price range `valuation` lies outside, and those whose FX range it lies
    /// outside; when it is no single point, every keyed holder, by price.
    pub(super) fn crossed_by(&self, valuation: Valuation) -> (Vec<AccountId>, Vec<AccountId>) {
        let currency = self.rules.as_ref().map_or(QuoteCurrency::Usd, |rules| rules.quote_currency);
        if !valuation.is_point(currency) {
            return (self.keyed().collect(), Vec::new());
        }
        let outside = |low: &BTreeSet<(Fixed, AccountId)>, high: &BTreeSet<(Fixed, AccountId)>, at: Option<Fixed>| {
            let Some(at) = at else { return Vec::new() };
            let above = low.range((Bound::Excluded((at, LAST_ACCOUNT)), Bound::Unbounded));
            let under = high.range(..(at, FIRST_ACCOUNT));
            above.chain(under).map(|(_, account)| *account).collect()
        };
        (
            outside(&self.price_low, &self.price_high, valuation.price),
            outside(&self.fx_low, &self.fx_high, valuation.fx),
        )
    }

    /// Every keyed holder: each keyed position has a finite upper end.
    pub(super) fn keyed(&self) -> impl Iterator<Item = AccountId> + '_ {
        self.price_high.iter().map(|(_, account)| *account)
    }

    /// How many keyed holders the market has.
    pub(super) fn keyed_count(&self) -> usize {
        self.price_high.len()
    }
}
