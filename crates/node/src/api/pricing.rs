//! How a market is priced right now, from the quotes the engine holds at its last stamp: how many
//! sources are fresh against `min_sources`, and what each source is doing. Read from the API's copy
//! of the engine, never on the sequencer's price path.

use engine_core::Fixed;
use engine_core::engine::MarketState;
use engine_core::price::PriceError;
use engine_core::types::Millis;

use crate::feeds::status::{SymbolState, VenueStatus};
use serde::Serialize;

/// What one source of a market is doing at the last stamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceState {
    /// Fresh, and inside the index.
    Counts,
    /// Fresh, but one of the extremes of sources that diverge past the rule, so no index forms.
    Diverging,
    /// Its newest quote is older than the rule's `max_age_ms`, and its connection reports nothing wrong.
    Stale,
    /// Its connection is up but has sent no fresh quote for this market for the silence timeout.
    Silent,
    /// Its connection's stream runs behind the delay bound, so its quotes do not count.
    Delayed,
    /// Its connection is down.
    Down,
    /// The node withholds its connection: a secret it needs was not delivered.
    Off,
    /// The market takes no quotes from it now: closed, or a contract it has rolled off.
    NotExpected,
    /// The node expects it to quote this market and the engine holds no quote from it.
    NeverQuoted,
}

/// One source of a market.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SourcePricing {
    pub source: String,
    pub state: SourceState,
    /// The venue's stamp of its newest quote the engine holds; `null` for a source that has not quoted.
    pub last_quote_ms: Option<Millis>,
    /// The stamp's age at `as_of_ms`.
    pub age_ms: Option<Millis>,
}

/// Why a market has no price.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Unpriced {
    /// Fewer fresh sources than `min_sources`.
    TooFewSources,
    /// The fresh sources are further apart than `max_divergence_bps`.
    Diverging,
    /// A futures market's roll-adjusted index is outside the valid range.
    OutOfRange,
}

/// A market's pricing at the engine's last stamp.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Pricing {
    pub priced: bool,
    /// Why there is no price; absent while priced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unpriced: Option<Unpriced>,
    /// The engine's last stamp, which every age is measured at.
    pub as_of_ms: Millis,
    pub fresh_sources: usize,
    pub min_sources: usize,
    pub max_age_ms: Millis,
    pub max_divergence_bps: i64,
    /// The spread of the fresh sources, once there are enough of them to have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub divergence_bps: Option<i64>,
    pub sources: Vec<SourcePricing>,
}

/// `market`'s pricing at `now`, the engine's last stamp.
pub(crate) fn pricing(market: &MarketState, now: Millis, venues: &[VenueStatus]) -> Pricing {
    let rules = market.spec.price_rules;
    let fresh = |quote: &engine_core::price::SourceQuote| {
        quote.is_well_formed() && quote.ts <= now && now.saturating_sub(quote.ts) <= rules.max_age_ms
    };
    let mut mids: Vec<Fixed> = market.quotes.values().filter(|quote| fresh(quote)).map(|quote| quote.mid).collect();
    mids.sort();
    let divergence_bps = (mids.len() >= rules.min_sources.max(1)).then(|| {
        let n = mids.len();
        let mid = if n % 2 == 1 { mids[n / 2] } else { (mids[n / 2 - 1] + mids[n / 2]).div_int(2) };
        (mids[n - 1] - mids[0]).bps_of(mid)
    });
    let verdict = market.index(now);
    let unpriced = match &verdict {
        Ok(_) => None,
        Err(PriceError::Stale { .. }) => Some(Unpriced::TooFewSources),
        Err(PriceError::Divergent { .. }) => Some(Unpriced::Diverging),
        Err(PriceError::OutOfRange { .. }) => Some(Unpriced::OutOfRange),
    };
    let diverging = unpriced == Some(Unpriced::Diverging);
    let mut sources: Vec<SourcePricing> = market
        .quotes
        .values()
        .map(|quote| {
            let state = if !fresh(quote) {
                feed_cause(symbol_state(venues, &quote.source, market)).unwrap_or(SourceState::Stale)
            } else if diverging && (Some(&quote.mid) == mids.first() || Some(&quote.mid) == mids.last()) {
                SourceState::Diverging
            } else {
                SourceState::Counts
            };
            SourcePricing {
                source: quote.source.clone(),
                state,
                last_quote_ms: Some(quote.ts),
                age_ms: Some(now.saturating_sub(quote.ts)),
            }
        })
        .collect();
    // A source the node expects to quote the market and the engine holds nothing from.
    for venue in venues.iter().filter(|venue| !market.quotes.contains_key(&venue.venue)) {
        let Some(state) = symbol_state(venues, &venue.venue, market) else { continue };
        let state = feed_cause(Some(state)).unwrap_or(SourceState::NeverQuoted);
        sources.push(SourcePricing { source: venue.venue.clone(), state, last_quote_ms: None, age_ms: None });
    }
    Pricing {
        priced: verdict.is_ok(),
        unpriced,
        as_of_ms: now,
        fresh_sources: mids.len(),
        min_sources: rules.min_sources,
        max_age_ms: rules.max_age_ms,
        max_divergence_bps: rules.max_divergence_bps,
        divergence_bps,
        sources,
    }
}

/// The source state a connection's own report names, when it names a cause.
fn feed_cause(state: Option<SymbolState>) -> Option<SourceState> {
    match state? {
        SymbolState::Stale => Some(SourceState::Stale),
        SymbolState::Silent => Some(SourceState::Silent),
        SymbolState::Delayed => Some(SourceState::Delayed),
        SymbolState::Down => Some(SourceState::Down),
        SymbolState::Off => Some(SourceState::Off),
        SymbolState::NotExpected => Some(SourceState::NotExpected),
        SymbolState::Quoting => None,
    }
}

/// The state of `venue`'s symbol that prices `market`: its front contract's for a futures market, so
/// the idle contracts behind it say nothing, and the worst of its symbols for it otherwise.
fn symbol_state(venues: &[VenueStatus], venue: &str, market: &MarketState) -> Option<SymbolState> {
    state_of(venues, venue, market.spec.id, market.futures.as_ref().map(|futures| futures.contract.as_str()))
}

/// `symbol_state` for market `id` whose front contract is `front`, if it has one.
fn state_of(
    venues: &[VenueStatus],
    venue: &str,
    id: engine_core::types::MarketId,
    front: Option<&str>,
) -> Option<SymbolState> {
    venues
        .iter()
        .filter(|candidate| candidate.venue == venue)
        .flat_map(|candidate| &candidate.symbols)
        .filter(|symbol| symbol.market == id && front.is_none_or(|front| symbol.contract.as_deref() == Some(front)))
        .map(|symbol| symbol.state)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feeds::status::SymbolStatus;

    fn symbol(name: &str, contract: &str, state: SymbolState) -> SymbolStatus {
        SymbolStatus { symbol: name.into(), market: 20, contract: Some(contract.into()), state }
    }

    #[test]
    fn a_futures_market_reads_its_front_contracts_symbol_not_the_worst_of_all_of_them() {
        let venues = [VenueStatus {
            venue: "cme".into(),
            state: SymbolState::Silent,
            symbols: vec![
                symbol("ESH7", "H7", SymbolState::NotExpected),
                symbol("ESZ6", "Z6", SymbolState::Silent),
                symbol("ESM7", "M7", SymbolState::Down),
            ],
        }];
        assert_eq!(state_of(&venues, "cme", 20, Some("Z6")), Some(SymbolState::Silent));
        assert_eq!(state_of(&venues, "cme", 20, Some("H7")), Some(SymbolState::NotExpected));
        assert_eq!(state_of(&venues, "cme", 20, Some("U7")), None);
        assert_eq!(state_of(&venues, "cme", 20, None), Some(SymbolState::Down));
        assert_eq!(state_of(&venues, "cme", 21, Some("Z6")), None);
        assert_eq!(state_of(&venues, "massive", 20, None), None);
    }
}
