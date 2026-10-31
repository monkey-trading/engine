//! The quotes a venue task holds between book updates, and the rule that judges whether the
//! quote the engine holds counts as the venue quoting.

use std::collections::BTreeMap;
use std::mem::replace;

use engine_core::price::{MAX_QUOTE_LEAD_MS, SourceQuote};
use engine_core::types::Millis;

/// One market's quotes received and not yet sequenced: the newest the host clock has reached, and
/// those dated ahead of it, kept by venue time until the clock reaches them. The quote taken is the
/// one the engine would hold under the rules before protocol `V12` had each quote been sent as it
/// arrived: one still ahead of its time gives way to any later quote, as when the venue's clock
/// steps back, and one already reached gives way only to a quote dated at or after it. A host clock
/// a little behind the venue then still prices from the venue, a moment late, rather than handing
/// the engine quotes those rules cannot use yet. Quotes ahead are kept one per venue millisecond
/// and at most `MAX_QUOTE_LEAD_MS` ahead, which bounds how many are held.
///
/// A submission must never claim a venue time the host clock has not reached, though from `V12`
/// the engine would count a quote dated ahead at once, as dated at the sequenced time: the stored
/// stamp would be honest, but `max_age_ms` and `stale_after` police freshness against the venue
/// stamp the log records, and the feed's own silence and outage judgements read it too. So a quote
/// ahead waits here for the clock, and `emit` releases it the moment the clock reaches it.
#[derive(Debug, Default)]
pub(super) struct Pending {
    reached: Option<SourceQuote>,
    ahead: BTreeMap<Millis, SourceQuote>,
}

impl Pending {
    /// Adds a quote received at host time `now`.
    /// Error cases:
    /// - the quote is dated more than `MAX_QUOTE_LEAD_MS` ahead of `now`, which the engine
    ///   refuses, so it is dropped; the error is its lead
    pub(super) fn add(&mut self, quote: SourceQuote, now: Millis) -> Result<(), Millis> {
        let lead = quote.ts.saturating_sub(now);
        if lead > MAX_QUOTE_LEAD_MS {
            return Err(lead);
        }
        self.catch_up(now);
        self.ahead.retain(|ts, _| *ts <= quote.ts);
        if lead > 0 {
            self.ahead.insert(quote.ts, quote);
        } else {
            self.keep(quote);
        }
        Ok(())
    }

    /// Takes the newest quote the host clock has reached at `now`, keeping those still ahead.
    pub(super) fn take(&mut self, now: Millis) -> Option<SourceQuote> {
        self.catch_up(now);
        self.reached.take()
    }

    /// Whether a quote the host clock has reached at `now` waits to be taken.
    pub(super) fn reached(&mut self, now: Millis) -> bool {
        self.catch_up(now);
        self.reached.is_some()
    }

    /// The earliest venue time still ahead of the host clock at `now`: when the clock reaches the
    /// first waiting quote.
    pub(super) fn next_ahead(&mut self, now: Millis) -> Option<Millis> {
        self.catch_up(now);
        self.ahead.keys().next().copied()
    }

    /// Moves the quotes the host clock has reached at `now` out of `ahead`, oldest first.
    fn catch_up(&mut self, now: Millis) {
        let ahead = self.ahead.split_off(&now.saturating_add(1));
        replace(&mut self.ahead, ahead).into_values().for_each(|quote| self.keep(quote));
    }

    /// Keeps a reached quote unless the one kept is dated after it.
    fn keep(&mut self, quote: SourceQuote) {
        if self.reached.as_ref().is_none_or(|reached| quote.ts >= reached.ts) {
            self.reached = Some(quote);
        }
    }
}

/// Whether a quote dated `ts` that the engine holds at sequenced time `at` counts as the venue
/// quoting: its time reached, and no older than `stale_after` or the market's `max_age_ms`.
pub(super) fn counts(ts: Millis, at: Millis, stale_after: Millis, max_age_ms: Millis) -> bool {
    ts <= at && at.saturating_sub(ts) <= stale_after.min(max_age_ms)
}
