//! What each price feed connection is doing, for the API to serve: the state of every venue symbol
//! the node expects quotes for, published by the connection's own task off the sequencer's path.
//! A reader copies the latest snapshots under one short lock; nothing here is read by the sequencer.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use engine_core::types::MarketId;
use serde::Serialize;

/// What a venue symbol is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SymbolState {
    /// Its market does not take quotes now (closed, or a contract it has rolled off): none is expected.
    /// Ranked below `Quoting`, so a venue that quotes anything shows as quoting.
    NotExpected,
    /// Quoting, and its quotes count.
    Quoting,
    /// The connection quotes the symbol, but its books are too old by their venue's time to count.
    Stale,
    /// The connection is up but this symbol has sent no fresh quote for the silence timeout.
    Silent,
    /// The connection's stream runs further behind than the delay bound, so its quotes do not count.
    Delayed,
    /// The connection is down: no quote has been sequenced since the outage began.
    Down,
    /// The node withholds the connection: a secret it needs was not delivered.
    Off,
}

/// One symbol of a connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct SymbolStatus {
    pub symbol: String,
    pub market: MarketId,
    /// The futures contract the symbol quotes; `null` for a market priced from spot quotes.
    #[serde(skip)]
    pub contract: Option<String>,
    pub state: SymbolState,
}

/// One connection of a venue, as its task last saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Connection {
    pub symbols: Vec<SymbolStatus>,
}

/// The latest snapshot of every connection, keyed by venue and the order its task started in.
#[derive(Clone, Debug, Default)]
pub(crate) struct FeedStatus(Arc<Mutex<BTreeMap<(String, usize), Connection>>>);

impl FeedStatus {
    /// Replaces connection `index` of `venue`'s snapshot.
    pub(crate) fn publish(&self, venue: &str, index: usize, connection: Connection) {
        let mut all = self.lock();
        if all.get(&(venue.to_owned(), index)) != Some(&connection) {
            all.insert((venue.to_owned(), index), connection);
        }
    }

    /// Every venue, in venue order. A venue's connections are one source: a symbol that several of
    /// them carry (a redundant second connection) is listed once, in the best state any of them reports,
    /// since the source counts while one of them is current.
    pub(crate) fn venues(&self) -> Vec<VenueStatus> {
        type Carried = BTreeMap<(MarketId, Option<String>), SymbolStatus>;
        let mut venues: BTreeMap<String, (usize, Carried)> = BTreeMap::new();
        for ((venue, _), connection) in self.lock().iter() {
            let (connections, carried) = venues.entry(venue.clone()).or_default();
            *connections += 1;
            for symbol in &connection.symbols {
                let held = carried.entry((symbol.market, symbol.contract.clone())).or_insert_with(|| symbol.clone());
                held.state = held.state.min(symbol.state);
            }
        }
        venues
            .into_iter()
            .map(|(venue, (connections, carried))| {
                let symbols: Vec<SymbolStatus> = carried.into_values().collect();
                let state = symbols.iter().map(|symbol| symbol.state).max().unwrap_or(SymbolState::Quoting);
                VenueStatus { venue, connections, state, symbols }
            })
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, usize), Connection>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A venue as `GET /v1/status` serves it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct VenueStatus {
    pub venue: String,
    /// How many connections carry it.
    pub connections: usize,
    /// The worst state of any of its symbols.
    pub state: SymbolState,
    pub symbols: Vec<SymbolStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str, market: MarketId, state: SymbolState) -> SymbolStatus {
        SymbolStatus { symbol: name.into(), market, contract: None, state }
    }

    fn contract(name: &str, contract: &str, state: SymbolState) -> SymbolStatus {
        SymbolStatus { contract: Some(contract.into()), ..symbol(name, 1, state) }
    }

    #[test]
    fn a_venue_that_quotes_anything_shows_as_quoting_whatever_its_idle_contracts() {
        let status = FeedStatus::default();
        status.publish(
            "cme",
            0,
            Connection {
                symbols: vec![
                    contract("ESZ6", "Z6", SymbolState::Quoting),
                    contract("ESH7", "H7", SymbolState::NotExpected),
                    contract("ESM7", "M7", SymbolState::NotExpected),
                ],
            },
        );
        assert_eq!(status.venues()[0].state, SymbolState::Quoting);
    }

    #[test]
    fn two_connections_of_one_source_list_a_shared_symbol_once_in_its_best_state() {
        let status = FeedStatus::default();
        let both = |first: SymbolState, second: SymbolState| {
            status.publish("coinbase", 0, Connection { symbols: vec![symbol("BTC-USD", 1, first)] });
            status.publish(
                "coinbase",
                1,
                Connection { symbols: vec![symbol("BTC-USD", 1, second), symbol("ETH-USD", 2, SymbolState::Quoting)] },
            );
            status.venues()
        };
        let venues = both(SymbolState::Down, SymbolState::Quoting);
        assert_eq!((venues.len(), venues[0].connections, venues[0].symbols.len()), (1, 2, 2));
        assert_eq!((venues[0].state, venues[0].symbols[0].state), (SymbolState::Quoting, SymbolState::Quoting));
        // Only when every connection fails the symbol does the source.
        let venues = both(SymbolState::Down, SymbolState::Silent);
        assert_eq!(venues[0].symbols[0].state, SymbolState::Silent);
        assert_eq!(venues[0].state, SymbolState::Silent);
        let venues = both(SymbolState::Down, SymbolState::Down);
        assert_eq!(venues[0].symbols[0].state, SymbolState::Down);
    }

    #[test]
    fn a_venues_state_is_the_worst_of_its_symbols_and_connections() {
        let status = FeedStatus::default();
        status.publish(
            "coinbase",
            0,
            Connection {
                symbols: vec![symbol("BTC-USD", 1, SymbolState::Silent), symbol("ETH-USD", 2, SymbolState::Quoting)],
            },
        );
        status.publish("coinbase", 1, Connection { symbols: vec![symbol("SOL-USD", 3, SymbolState::Quoting)] });
        status.publish("kraken", 0, Connection { symbols: vec![symbol("BTC/USD", 1, SymbolState::Down)] });
        let venues = status.venues();
        assert_eq!(
            venues.iter().map(|v| (v.venue.as_str(), v.state)).collect::<Vec<_>>(),
            [("coinbase", SymbolState::Silent), ("kraken", SymbolState::Down),]
        );
    }
}
