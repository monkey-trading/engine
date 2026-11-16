//! A connection's task publishes what each of its symbols is doing for the API (`feeds::status`),
//! once a check interval and when its connection is lost. Read from the task's own fields, so the
//! sequencer's path does no work for it.

use crate::clock::now_ms;
use crate::feeds::status::{Connection, FeedStatus, SymbolState, SymbolStatus};
use crate::feeds::venue::VenueFeed;

/// Where a connection publishes: the shared snapshots, and its place among them.
#[derive(Clone, Debug, Default)]
pub(super) struct Report {
    status: FeedStatus,
    index: usize,
}

impl Report {
    pub(super) fn new(status: FeedStatus, index: usize) -> Self {
        Self { status, index }
    }
}

impl VenueFeed {
    /// What each of the connection's symbols is doing now.
    pub(super) fn symbol_states(&self) -> Vec<SymbolStatus> {
        let now = now_ms();
        self.instruments
            .iter()
            .map(|(symbol, instrument)| {
                let state = if self.down || !self.link_up {
                    SymbolState::Down
                } else if self.delayed {
                    SymbolState::Delayed
                } else if self.silent.contains(symbol) {
                    SymbolState::Silent
                } else if self.is_held(symbol) || self.closed(instrument.market, now) {
                    SymbolState::NotExpected
                } else {
                    SymbolState::Quoting
                };
                SymbolStatus {
                    symbol: symbol.clone(),
                    market: instrument.market,
                    contract: instrument.contract.clone(),
                    state,
                }
            })
            .collect()
    }

    /// Publishes the connection's snapshot.
    pub(super) fn publish_status(&self) {
        let connection = Connection { symbols: self.symbol_states() };
        self.report.status.publish(self.conn.venue.name(), self.report.index, connection);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::feeds::venue::tests::kraken_feed;

    /// What `feed` published, by symbol.
    fn published(status: &FeedStatus) -> Vec<(String, SymbolState)> {
        let venues = status.venues();
        venues.iter().flat_map(|venue| venue.symbols.iter().map(|s| (s.symbol.clone(), s.state))).collect()
    }

    #[test]
    fn a_connection_publishes_what_each_symbol_is_doing_and_what_ends_it() {
        let mut feed = kraken_feed(&[(1, "BTC/USD"), (2, "ETH/USD")], Duration::from_secs(1));
        let status = FeedStatus::default();
        feed.report = Report::new(status.clone(), 0);
        feed.link_up = true;
        let state = |status: &FeedStatus, symbol: &str| {
            published(status).into_iter().find(|(name, _)| name == symbol).map(|(_, state)| state)
        };
        feed.publish_status();
        assert_eq!(state(&status, "BTC/USD"), Some(SymbolState::Quoting));
        feed.silent.insert("BTC/USD".into());
        feed.idle.insert("ETH/USD".into());
        feed.publish_status();
        assert_eq!(state(&status, "BTC/USD"), Some(SymbolState::Silent));
        assert_eq!(state(&status, "ETH/USD"), Some(SymbolState::NotExpected));
        feed.delayed = true;
        feed.publish_status();
        assert_eq!(state(&status, "BTC/USD"), Some(SymbolState::Delayed));
        feed.down = true;
        feed.publish_status();
        assert_eq!(
            (state(&status, "BTC/USD"), state(&status, "ETH/USD")),
            (Some(SymbolState::Down), Some(SymbolState::Down))
        );
        assert_eq!(status.venues()[0].venue, "kraken");
        // The connection drops while no outage is alerted: the venue is down, not quoting.
        (feed.down, feed.delayed, feed.link_up) = (false, false, false);
        feed.silent.clear();
        feed.idle.clear();
        feed.publish_status();
        assert_eq!(state(&status, "BTC/USD"), Some(SymbolState::Down));
        // The venue recovers.
        feed.link_up = true;
        (feed.down, feed.delayed) = (false, false);
        feed.silent.clear();
        feed.idle.clear();
        feed.publish_status();
        assert_eq!(
            published(&status),
            [("BTC/USD".into(), SymbolState::Quoting), ("ETH/USD".into(), SymbolState::Quoting)]
        );
    }
}
