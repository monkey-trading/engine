//! Sequencing the newest quote per symbol as book updates arrive, within a per-symbol budget, and
//! the emit timer that releases what the budget held back and refreshes a quote whose mid has not
//! moved.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::time::Duration;

use anyhow::anyhow;
use tokio::time::{Instant, Sleep};
use tracing::debug;

use engine_core::command::Command;
use engine_core::types::Millis;

use crate::clock::now_ms;
use crate::feeds::venue::quotes::{Verdict, record, refresh_after, verdict};
use crate::feeds::venue::{Pending, VenueFeed};
use crate::sequencer::Request;

/// How far ahead the emit timer idles when nothing waits to be sequenced.
const IDLE: Duration = Duration::from_secs(24 * 60 * 60);

/// How many quotes a symbol may sequence back to back before `timing.interval` between them
/// applies. Over any span a symbol sequences at most this many plus one per interval.
const BURST: u32 = 8;

/// How far inside a waiting repeat's noted delay bound the emit timer releases it: the margin for
/// the emit task waking and the sequencer stamping the quote, which drops one that reaches it
/// past the bound the read noted.
const SEND_MARGIN: Duration = Duration::from_millis(100);

impl VenueFeed {
    /// Sequences the newest quote for each symbol, unless the engine would refuse it: its market
    /// unlisted, its contract not taken, or stamped or sent while its market is closed. A book
    /// update sequences its symbol's quote at once, whatever its stamp: the engine stores one
    /// stamped ahead of its sequenced time as stamped at it. Each symbol has a budget of `BURST`
    /// quotes that refills one per `timing.interval`, so venue messages an injected host floods in
    /// cannot make a feed sequence a symbol's quotes faster than that; a symbol past its budget
    /// coalesces to the newest quote and waits for it to refill.
    /// A quote that repeats the mid last sequenced for its symbol on this connection waits instead
    /// (see `quotes::verdict`): the engine's price depends on the mid and on the quote being
    /// fresh, so it is sequenced once the one the engine holds is `quotes::refresh_after` old,
    /// the newest repeat by then going, and a new mid never waits on a repeat.
    /// Returns the next instant a held quote may sequence — the earliest the budget frees a symbol
    /// holding a quote, and when a waiting repeat falls due or its noted delay bound runs out —
    /// for the caller to re-arm the emit timer, or `None` when nothing waits.
    /// Error cases:
    /// - the sequencer has gone away
    pub(super) async fn emit(
        &mut self,
        buffered: &mut BTreeMap<String, Pending>,
        spent: &mut BTreeMap<String, Instant>,
    ) -> anyhow::Result<Option<Instant>> {
        let now = now_ms();
        let interval = self.timing.interval;
        let mut wake: Option<Instant> = None;
        for (symbol, quotes) in buffered.iter_mut() {
            // The budget is spent up to this instant; one more quote may go while it is within
            // `BURST` intervals of now.
            let free_at = spent
                .get(symbol)
                .and_then(|spent_to| spent_to.checked_sub(interval * (BURST - 1)))
                .filter(|free_at| *free_at > Instant::now());
            // When a repeat of the sequenced mid, held for later, is due to refresh it.
            let mut refresh: Option<Instant> = None;
            if free_at.is_none()
                && let Some((quote, read_stamp)) = quotes.take()
            {
                let market = self.instruments[symbol].market;
                // Held and out-of-session quotes are dropped, not sequenced for the engine to
                // refuse; an out-of-session one is logged. The engine needs the market open from
                // the venue stamp to the sequencer's stamp, which may come up to an interval
                // after this one, when the budget holds a symbol's next quote.
                let held = self.is_held(symbol);
                let in_session = self.in_session(market, quote.ts, now);
                if !held && !in_session {
                    let (venue, feed) = (self.conn.venue.name(), self.conn.url.as_str());
                    debug!(venue, feed, symbol, market, "quote from outside one open session held back");
                }
                if held || !in_session {
                    // The next quote in session goes whatever the engine holds from before.
                    self.sent.remove(symbol);
                    self.take_not_after(symbol, quote.ts);
                } else {
                    let sent = self.sent.get(symbol).copied();
                    let refresh_ms = self.max_age.get(&market).map(|max_age| {
                        let stale_after = Millis::try_from(self.timing.stale_after.as_millis()).unwrap_or(Millis::MAX);
                        refresh_after(stale_after, *max_age)
                    });
                    // A repeat's refresh may fall due after the delay bound noted when it was read
                    // runs out, past which the sequencer drops the quote: it then waits only until
                    // the bound less `SEND_MARGIN`, or goes at once when the bound is already that
                    // close. The deadline is read, not taken, so the quote still carries it.
                    let (send, wake) = match verdict(sent, &quote, refresh_ms, now) {
                        Verdict::Send => (true, None),
                        Verdict::Redundant => {
                            self.take_not_after(symbol, quote.ts);
                            (false, None)
                        }
                        Verdict::Wait(at) => {
                            let due = Instant::now() + Duration::from_millis((at - now) as u64);
                            let bound = self
                                .peek_not_after(symbol, quote.ts)
                                .map(|not_after| {
                                    not_after.checked_sub(SEND_MARGIN).unwrap_or_else(std::time::Instant::now)
                                })
                                .map(Instant::from_std);
                            match bound {
                                Some(bound) if bound <= Instant::now() => (true, None),
                                bound => (false, Some(bound.map_or(due, |bound| due.min(bound)))),
                            }
                        }
                    };
                    if send {
                        self.unconfirmed.insert(symbol.clone());
                        self.sent.insert(symbol.clone(), record(sent, &quote, now));
                        let not_after = self.take_not_after(symbol, quote.ts);
                        let cmd = Command::Prices { market, quotes: vec![quote] };
                        self.tx.send(Request::Quote { cmd, not_after }).await.map_err(|_| anyhow!("sequencer gone"))?;
                        let spent_to = spent.get(symbol).copied().unwrap_or_else(Instant::now).max(Instant::now());
                        spent.insert(symbol.clone(), spent_to + interval);
                    } else if let Some(wake) = wake {
                        refresh = Some(wake);
                        quotes.defer(quote, read_stamp);
                    }
                }
            }
            // Nothing sequenced: arm the wake for what waits. A quote held by the budget goes
            // when it refills; a repeat goes when it is due.
            let mut at = free_at.filter(|_| quotes.waiting());
            if let Some(refresh) = refresh {
                at = Some(at.map_or(refresh, |at| at.min(refresh)));
            }
            if let Some(at) = at {
                wake = Some(wake.map_or(at, |wake| wake.min(at)));
            }
        }
        Ok(wake)
    }
}

impl VenueFeed {
    /// When the bound on the age of `symbol`'s quote stamped `ts` runs out, read without consuming
    /// it (see `delay`), or `None` on a connection without pings.
    fn peek_not_after(&self, symbol: &str, ts: Millis) -> Option<std::time::Instant> {
        self.delay.as_ref().and_then(|probe| probe.peek_not_after(symbol, ts))
    }

    /// Takes when the bound on the age of `symbol`'s quote stamped `ts` runs out (see `delay`),
    /// forgetting it and any older, or `None` on a connection without pings.
    fn take_not_after(&mut self, symbol: &str, ts: Millis) -> Option<std::time::Instant> {
        self.delay.as_mut().map(|probe| probe.take_not_after(symbol, ts))
    }
}

/// Points the emit timer at `next`, or a day ahead when nothing waits.
pub(super) fn rearm(wake: &mut Pin<Box<Sleep>>, next: Option<Instant>) {
    wake.as_mut().reset(next.unwrap_or_else(|| Instant::now() + IDLE));
}
