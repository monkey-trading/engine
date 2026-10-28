//! Markets priced from futures, from protocol `V21`. A listing names its contracts in roll order,
//! each with the time the index moves onto it. The index is the median of the front contract's
//! quotes plus an adjustment: the sum of every roll's gap. The next contract's quotes are taken too,
//! and the gap between the two medians is recorded whenever both have an index. Once the next
//! contract's roll time has passed, the market rolls at the first command at which the next
//! contract has an index and a gap is recorded: the adjustment takes the gap, so the index moves
//! only as the market did since the gap was measured, even when the front contract has stopped
//! quoting, as it does at expiry.

use std::collections::{BTreeMap, BTreeSet};
use std::mem::take;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Fixed;
use crate::engine::{Engine, MarketState};
use crate::event::Event;
use crate::market::MarketSpec;
use crate::price::{Index, PriceError, SourceQuote, compute_index};
use crate::types::{MarketId, Millis};

/// Which of a market's quotes a quote joins: those of the contract it prices from, or the next
/// one's.
enum Kept {
    Front,
    Next,
}

/// A futures contract a market prices from, and when its index rolls onto it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FuturesContract {
    /// The contract's name, as the quotes for it carry it, e.g. `ESZ6`.
    pub contract: String,
    /// When the index rolls onto this contract from the one before it. The first contract's is when
    /// a listing made after it starts on it.
    pub from: DateTime<Utc>,
}

/// Where a market priced from futures stands in its roll order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FuturesState {
    /// The contract the index prices from; the market's quotes are this contract's.
    pub contract: String,
    /// Added to the contract's median to give the index: the sum of every roll's gap so far.
    pub adjustment: Fixed,
    /// The next contract's quotes, by source.
    pub next_quotes: BTreeMap<String, SourceQuote>,
    /// The front contract's median less the next contract's, at the last command at which both had
    /// an index; `None` until then, and again after each roll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<Fixed>,
}

impl FuturesContract {
    /// The roll time in milliseconds since the epoch.
    fn rolls_in_ms(&self) -> Millis {
        self.from.timestamp_millis()
    }
}

impl FuturesState {
    /// Where a market listed at `now` starts: on the last contract whose roll time has passed, or
    /// the first if none has, with no adjustment. `None` for a market not priced from futures.
    pub(crate) fn listed(spec: &MarketSpec, now: Millis) -> Option<Self> {
        let first = spec.contracts.first()?;
        let front = spec.contracts.iter().rfind(|c| c.rolls_in_ms() <= now).unwrap_or(first);
        Some(FuturesState {
            contract: front.contract.clone(),
            adjustment: Fixed::ZERO,
            next_quotes: BTreeMap::new(),
            gap: None,
        })
    }
}

impl MarketState {
    /// The market's index at `now`: the median of its fresh quotes, plus the roll adjustment for a
    /// market priced from futures.
    /// Error cases:
    /// - too few fresh quotes, or fresh quotes too far apart (see `compute_index`)
    /// - an adjusted index outside the valid input range
    pub fn index(&self, now: Millis) -> Result<Index, PriceError> {
        let index = compute_index(&self.quotes, &self.spec.price_rules, now)?;
        let Some(futures) = &self.futures else { return Ok(index) };
        // Each roll adds a gap between two valid quotes, so no count of rolls a listing can name
        // brings the sum near `Fixed`'s range.
        let mid = index.mid + futures.adjustment;
        if !mid.is_valid_input() {
            return Err(PriceError::OutOfRange { mid });
        }
        Ok(Index { mid, ..index })
    }

    /// The quotes the market keeps for `contract`: its own, for the contract it prices from, or for
    /// none named in a market not priced from futures; or the next contract's. `None` for any other
    /// contract, whose quotes it does not take.
    pub fn quotes_for(&self, contract: Option<&str>) -> Option<&BTreeMap<String, SourceQuote>> {
        match (self.keeps(contract)?, &self.futures) {
            (Kept::Next, Some(futures)) => Some(&futures.next_quotes),
            _ => Some(&self.quotes),
        }
    }

    /// `quotes_for`, to store into.
    pub(crate) fn quotes_for_mut(&mut self, contract: Option<&str>) -> Option<&mut BTreeMap<String, SourceQuote>> {
        match (self.keeps(contract)?, &mut self.futures) {
            (Kept::Next, Some(futures)) => Some(&mut futures.next_quotes),
            _ => Some(&mut self.quotes),
        }
    }

    /// Whether the listing names `contract`: none for a market not priced from futures, or one of
    /// its contracts for a market that is. A quote for a contract the listing does not name can
    /// never price the market.
    pub fn names_contract(&self, contract: Option<&str>) -> bool {
        match (&self.futures, contract) {
            (None, None) => true,
            (Some(_), Some(named)) => self.spec.contracts.iter().any(|c| c.contract == named),
            _ => false,
        }
    }

    /// Whether `contract` is the one the market priced from until its last roll, whose quotes feeds
    /// send until they learn of the roll.
    pub fn rolled_off(&self, contract: Option<&str>) -> bool {
        let (Some(futures), Some(named)) = (&self.futures, contract) else { return false };
        let contracts = &self.spec.contracts;
        let front = contracts.iter().position(|c| c.contract == futures.contract);
        front.and_then(|front| front.checked_sub(1)).is_some_and(|previous| contracts[previous].contract == named)
    }

    /// Whether a quote stamped `ts` and sequenced at `now` belongs to one open session: the market
    /// stays open from one to the other. A span longer than the freshness window holds only a quote
    /// too old to count, so only its two ends are checked.
    pub fn in_session(&self, ts: Millis, now: Millis) -> bool {
        let schedule = &self.spec.schedule;
        let (first, last) = (ts.min(now), ts.max(now));
        if last.saturating_sub(first) > self.spec.price_rules.max_age_ms {
            return schedule.is_open(first) && schedule.is_open(last);
        }
        schedule.open_throughout(first, last)
    }

    /// Drops every quote held, of either contract, stamped at or before `ts`. A recorded gap stays:
    /// it measured two contracts at one time, whatever happens after.
    pub(crate) fn drop_quotes_to(&mut self, ts: Millis) {
        self.quotes.retain(|_, quote| quote.ts > ts);
        if let Some(futures) = &mut self.futures {
            futures.next_quotes.retain(|_, quote| quote.ts > ts);
        }
    }

    /// Whether any quote held, of either contract, is stamped at or before `ts`.
    pub(crate) fn holds_quotes_to(&self, ts: Millis) -> bool {
        let next = self.futures.iter().flat_map(|futures| futures.next_quotes.values());
        self.quotes.values().chain(next).any(|quote| quote.ts <= ts)
    }

    /// Drops every quote held, of either contract, more than `max_age_ms` old at `now`: stale under
    /// that limit, so no later limit may count it.
    pub(crate) fn drop_quotes_older_than(&mut self, now: Millis, max_age_ms: Millis) {
        let fresh = |quote: &SourceQuote| quote.ts > now || now - quote.ts <= max_age_ms;
        self.quotes.retain(|_, quote| fresh(quote));
        if let Some(futures) = &mut self.futures {
            futures.next_quotes.retain(|_, quote| fresh(quote));
        }
    }

    /// Keeps only the quotes, of either contract, that could count at `now`: fresh, and from the
    /// session open at `now`.
    pub(crate) fn retain_session_quotes(&mut self, now: Millis) {
        let counts =
            |quote: &SourceQuote| now - quote.ts <= self.spec.price_rules.max_age_ms && self.in_session(quote.ts, now);
        let stale: Vec<String> = self.quotes.iter().filter(|(_, q)| !counts(q)).map(|(s, _)| s.clone()).collect();
        let stale_next: Vec<String> = (self.futures.iter().flat_map(|futures| &futures.next_quotes))
            .filter(|(_, q)| !counts(q))
            .map(|(source, _)| source.clone())
            .collect();
        stale.iter().for_each(|source| drop(self.quotes.remove(source)));
        if let Some(futures) = &mut self.futures {
            stale_next.iter().for_each(|source| drop(futures.next_quotes.remove(source)));
        }
    }

    /// Which of the market's quotes a quote for `contract` joins, if any (see `quotes_for`).
    fn keeps(&self, contract: Option<&str>) -> Option<Kept> {
        match (&self.futures, contract) {
            (None, None) => Some(Kept::Front),
            (Some(futures), Some(named)) if named == futures.contract => Some(Kept::Front),
            (Some(_), Some(named)) => self.next_contract().filter(|next| next.contract == named).map(|_| Kept::Next),
            _ => None,
        }
    }

    /// The contract after the one the market prices from, if its listing names one.
    pub fn next_contract(&self) -> Option<&FuturesContract> {
        let futures = self.futures.as_ref()?;
        let contracts = &self.spec.contracts;
        // The listing always names the contract the market prices from (`relisted`).
        let front = contracts.iter().position(|c| c.contract == futures.contract).expect("front contract is listed");
        contracts.get(front + 1)
    }

    /// The futures state for this market under a new listing `spec`: where it stands, and the next
    /// contract's quotes and the gap to it while that contract stays next.
    /// Error cases:
    /// - `spec` names futures contracts and the market is listed without them, or the reverse
    /// - `spec` does not name the contract the market prices from
    pub(crate) fn relisted(&self, spec: &MarketSpec) -> Result<Option<FuturesState>, String> {
        let id = spec.id;
        match (&self.futures, spec.contracts.is_empty()) {
            (None, true) => Ok(None),
            (None, false) => Err(format!("market {id} is listed without futures contracts")),
            (Some(_), true) => Err(format!("market {id} prices from futures contracts")),
            (Some(futures), false) => {
                let position = spec.contracts.iter().position(|c| c.contract == futures.contract);
                let front = position
                    .ok_or_else(|| format!("contract {} market {id} prices from is not listed", futures.contract))?;
                let next = spec.contracts.get(front + 1).map(|c| c.contract.as_str());
                let mut kept = futures.clone();
                if next != self.next_contract().map(|c| c.contract.as_str()) {
                    kept.next_quotes.clear();
                    kept.gap = None;
                }
                Ok(Some(kept))
            }
        }
    }
}

impl Engine {
    /// For every market priced from futures with a next contract: records the gap between the two
    /// contracts' medians when both have an index now, then rolls the market if the next contract's
    /// roll time has passed, it has an index, and a gap is recorded. A roll adds the gap to the
    /// adjustment, and the next contract's quotes become the market's.
    pub(crate) fn roll_futures(&mut self, events: &mut Vec<Event>) {
        let now = self.now;
        let ids: Vec<MarketId> =
            self.markets.iter().filter(|(_, market)| market.next_contract().is_some()).map(|(id, _)| *id).collect();
        for market_id in ids {
            let market = &self.markets[&market_id];
            let futures = market.futures.as_ref().expect("a market with a next contract prices from futures");
            let next = market.next_contract().expect("selected above");
            let rules = &market.spec.price_rules;
            let next_index = compute_index(&futures.next_quotes, rules, now);
            let gap = match (compute_index(&market.quotes, rules, now), &next_index) {
                (Ok(front), Ok(next)) => Some(front.mid - next.mid),
                _ => futures.gap,
            };
            let due = next.rolls_in_ms() <= now && next_index.is_ok() && gap.is_some();
            if gap == futures.gap && !due {
                continue;
            }
            let contract = next.contract.clone();
            // Only a market whose gap moves or that rolls is taken for change, so the state root
            // rehashes no other.
            let market = self.markets.get_mut(&market_id).expect("listed");
            let futures = market.futures.as_mut().expect("checked above");
            futures.gap = gap;
            if due {
                futures.adjustment += gap.expect("checked above");
                futures.gap = None;
                futures.contract = contract.clone();
                market.quotes = take(&mut futures.next_quotes);
                events.push(Event::MarketRolled { market: market_id, contract, adjustment: futures.adjustment });
            }
        }
    }
}

/// Checks a listing's futures contracts: each named once, in roll order.
/// Error cases:
/// - a contract with an empty name, or a name listed twice
/// - a contract that does not roll in after the one before it
pub(crate) fn validate_contracts(contracts: &[FuturesContract]) -> Result<(), String> {
    let mut names = BTreeSet::new();
    if let Some(c) = contracts.iter().find(|c| c.contract.is_empty() || !names.insert(c.contract.as_str())) {
        return Err(format!("futures contract {:?} is empty or listed twice", c.contract));
    }
    if let Some(pair) = contracts.windows(2).find(|pair| pair[1].from <= pair[0].from) {
        return Err(format!("futures contract {} does not roll in after {}", pair[1].contract, pair[0].contract));
    }
    Ok(())
}
