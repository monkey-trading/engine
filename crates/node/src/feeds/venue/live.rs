//! A feed that holds its quotes back until the node that sequences them runs. A rotation-set member
//! opens its venue connections a lead before its own turn's boundary (`node::price_feeds::Warm`), so
//! they are subscribed, their books current and their pings answered by the time it sequences its first
//! command. While it holds back, a feed sends the sequencer nothing: it keeps each symbol's newest
//! quote, with the delay bound noted when it was read or re-dated, asks the engine nothing, and counts
//! no symbol as silent. Once live, it asks the engine about its symbols, as a fresh connection does,
//! and sends the quotes it kept, which the sequencer holds to the same bounds as any other: the delay
//! bound its pings set, checked at the stamp, and the market's `max_age_ms`.

use std::collections::BTreeMap;

use tokio::sync::watch;
use tokio::time::Instant;

use crate::feeds::venue::{Pending, VenueFeed};

/// A switch that lets a feed's quotes through at once: a feed its node starts sends from the outset.
pub fn always() -> watch::Receiver<bool> {
    watch::channel(true).1
}

impl VenueFeed {
    /// This feed, holding its quotes back while `live` is not set.
    pub(super) fn until_live(self, live: watch::Receiver<bool>) -> Self {
        Self { live, ..self }
    }

    /// Whether the feed's quotes go to the sequencer now.
    pub(super) fn live(&self) -> bool {
        *self.live.borrow()
    }

    /// Whether the feed holds its quotes back now. The first look after it goes live restarts every
    /// symbol's silence and the connection's counts, as a fresh connection does: nothing it held back
    /// counts against it.
    pub(super) fn holds_back(&mut self) -> bool {
        if !self.live() {
            self.holding = true;
            return true;
        }
        if std::mem::take(&mut self.holding) {
            let now = Instant::now();
            self.last_quote.values_mut().for_each(|at| *at = now);
            self.connected_afresh();
        }
        false
    }

    /// Keeps only what a feed holding back sends once live: each symbol's newest quote and its delay
    /// bound. The bounds noted for the quotes it replaced are forgotten, since nothing takes them, and
    /// the kept quote counts its wait to the sequencer from when it is sent, as a repeat held to refresh
    /// its mid does.
    pub(super) fn keep_newest(&mut self, buffered: &mut BTreeMap<String, Pending>) {
        for (symbol, quotes) in buffered.iter_mut() {
            if let Some(ts) = quotes.kept_ts() {
                self.delay.forget_before(symbol, ts);
            }
            quotes.take_received();
        }
    }
}
