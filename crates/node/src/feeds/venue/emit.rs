//! Sequencing the newest reached quote per symbol as book updates arrive, and the emit timer that
//! paces what arrives faster than the configured rate and releases what the host clock has not
//! reached yet.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::time::Duration;

use anyhow::anyhow;
use tokio::time::{Instant, Sleep};
use tracing::debug;

use engine_core::command::Command;

use crate::clock::now_ms;
use crate::feeds::venue::{Pending, VenueFeed};
use crate::sequencer::Request;

/// How far ahead the emit timer idles when nothing waits to be sequenced.
const IDLE: Duration = Duration::from_secs(24 * 60 * 60);

impl VenueFeed {
    /// Sequences the newest quote the host clock has reached for each symbol, unless the engine
    /// would refuse it: its market unlisted, its contract not taken, or stamped or sent while its
    /// market is closed. A book update sequences its symbol's quote at once; a symbol's quotes
    /// wait `timing.interval` between sequencings, and what arrives faster coalesces to the
    /// newest, so venue messages an injected host floods in cannot make a feed sequence a symbol's
    /// quotes more often than that. A quote stamped ahead of the host clock is never sent before
    /// the clock reaches it: a submission must not claim a time the clock has not reached.
    /// Returns the next instant a held quote may sequence — the earliest pace deadline over
    /// symbols holding a reached quote, and the moment the host clock reaches the earliest quote
    /// ahead of it — for the caller to re-arm the emit timer, or `None` when nothing waits.
    /// Error cases:
    /// - the sequencer has gone away
    pub(super) async fn emit(
        &mut self,
        buffered: &mut BTreeMap<String, Pending>,
        paced: &mut BTreeMap<String, Instant>,
    ) -> anyhow::Result<Option<Instant>> {
        let now = now_ms();
        let mut wake: Option<Instant> = None;
        for (symbol, quotes) in buffered.iter_mut() {
            let due = paced.get(symbol).copied().filter(|due| *due > Instant::now());
            if due.is_none()
                && let Some(quote) = quotes.take(now)
            {
                let market = self.instruments[symbol].market;
                if self.is_held(symbol) {
                    continue;
                }
                // The engine needs the market open from the venue stamp to the sequencer's stamp,
                // which may come up to an interval after this one, when the pace holds a symbol's
                // next quote.
                if !self.in_session(market, quote.ts, now) {
                    let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
                    debug!(venue, feed, symbol, market, "quote from outside one open session held back");
                    continue;
                }
                self.unconfirmed.insert(symbol.clone());
                let cmd = Command::Prices { market, quotes: vec![quote] };
                self.tx.send(Request::Submit { cmd, reply: None }).await.map_err(|_| anyhow!("sequencer gone"))?;
                paced.insert(symbol.clone(), Instant::now() + self.timing.interval);
                continue;
            }
            // Nothing sequenced: arm the wake for what waits. A reached quote goes when the pace
            // frees the symbol; one the host clock has not reached goes when the clock reaches it,
            // and then still waits out the pace if it must.
            let mut at = if due.is_some_and(|_| quotes.reached(now)) { due } else { None };
            if let Some(ahead) = quotes.next_ahead(now) {
                let reached_at = Instant::now() + Duration::from_millis((ahead - now) as u64);
                at = Some(at.map_or(reached_at, |at| at.min(reached_at)));
            }
            if let Some(at) = at {
                wake = Some(wake.map_or(at, |wake| wake.min(at)));
            }
        }
        Ok(wake)
    }
}

/// Points the emit timer at `next`, or a day ahead when nothing waits.
pub(super) fn rearm(wake: &mut Pin<Box<Sleep>>, next: Option<Instant>) {
    wake.as_mut().reset(next.unwrap_or_else(|| Instant::now() + IDLE));
}
