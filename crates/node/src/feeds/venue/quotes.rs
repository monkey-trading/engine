//! The newest quote a venue task holds between book updates, the rule that decides whether a quote
//! repeats the mid already sequenced and when it falls due, and the rule that judges whether the
//! quote the engine holds counts as the venue quoting.

use engine_core::Fixed;
use engine_core::price::{MAX_QUOTE_LEAD_MS, SourceQuote};
use engine_core::types::Millis;

/// One symbol's newest quote received and not yet sequenced. A quote goes as soon as it is read,
/// whatever its stamp: from `V12` the engine stores one stamped ahead of its sequenced time as
/// stamped at it, so a venue clock ahead of the host's costs no delay, and the stored stamp never
/// claims a time the sequencer has not reached. A newer quote replaces it unless its stamp, as the
/// engine would store it (at most the highest host time a quote was read at), is before the kept
/// quote's: the host time never runs backwards for this comparison, so a host-clock step loses no
/// moved mid to a kept stamp the stepped-back clock has fallen below. A quote's lead is judged the
/// same way, against the highest host time read: sequenced time does not step back with the host
/// clock, so a quote the engine would take stays takeable after a step.
#[derive(Debug, Default)]
pub(super) struct Pending {
    /// The quote, and the stamp the engine would store it at had it been sent when read.
    quote: Option<(SourceQuote, Millis)>,
    /// The high-water mark of the host times quotes were read at, by which stamps are clamped.
    read_hwm: Millis,
}

impl Pending {
    /// Adds a quote received at host time `now`.
    /// Error cases:
    /// - the quote is dated more than `MAX_QUOTE_LEAD_MS` ahead of the highest host time read, `now`
    ///   or a higher one before it, which the engine refuses, so it is dropped; the error is its lead
    pub(super) fn add(&mut self, quote: SourceQuote, now: Millis) -> Result<(), Millis> {
        let lead = quote.ts.saturating_sub(now.max(self.read_hwm));
        if lead > MAX_QUOTE_LEAD_MS {
            return Err(lead);
        }
        self.read_hwm = self.read_hwm.max(now);
        let stamp = quote.ts.min(self.read_hwm);
        if self.quote.as_ref().is_none_or(|(_, kept)| stamp >= *kept) {
            self.quote = Some((quote, stamp));
        }
        Ok(())
    }

    /// Takes the newest quote, with the stamp it would have been stored at had it been sent when
    /// read.
    pub(super) fn take(&mut self) -> Option<(SourceQuote, Millis)> {
        self.quote.take()
    }

    /// Whether a quote waits.
    pub(super) fn waiting(&self) -> bool {
        self.quote.is_some()
    }

    /// Puts back the quote just taken with its stamp, which `verdict` held for later: nothing
    /// newer has been added since.
    pub(super) fn defer(&mut self, quote: SourceQuote, stamp: Millis) {
        self.quote = Some((quote, stamp));
    }
}

/// Whether a quote dated `ts` that the engine holds at sequenced time `at` counts as the venue
/// quoting: its time reached, and no older than `stale_after` or the market's `max_age_ms`.
pub(super) fn counts(ts: Millis, at: Millis, stale_after: Millis, max_age_ms: Millis) -> bool {
    ts <= at && at.saturating_sub(ts) <= stale_after.min(max_age_ms)
}

/// What to do with the newest quote of a symbol (see `verdict`).
#[derive(Debug, PartialEq)]
pub(super) enum Verdict {
    /// Sequence it now.
    Send,
    /// It repeats the mid the engine holds from this venue, and that quote is not yet old enough
    /// to need refreshing: keep it and sequence it at this host time.
    Wait(Millis),
    /// It adds nothing to the quote the engine holds: drop it.
    Redundant,
}

/// How old, in host time past its stamp, the quote the engine holds from a venue may get before a
/// repeat of its mid is sequenced to refresh it: half the shorter of `stale_after` and the
/// market's `max_age_ms`. The refresh then has the other half to be sequenced and applied before
/// the held quote stops counting, so a sequencer stall of up to that long costs no price; the
/// venue's own delay does not enter, because the time is counted from the held quote's stamp on
/// the clock the engine judges age by, so a quote that arrives late is refreshed the sooner.
pub(super) fn refresh_after(stale_after: Millis, max_age_ms: Millis) -> Millis {
    stale_after.min(max_age_ms) / 2
}

/// The record of the newest quote sequenced for a symbol once `quote` is sent at host time `now`:
/// its mid and the stamp the engine stores it at, unless the engine would keep the quote already
/// on record, which is stamped later (`sent`).
pub(super) fn record(sent: Option<(Fixed, Millis)>, quote: &SourceQuote, now: Millis) -> (Fixed, Millis) {
    let stamp = quote.ts.min(now);
    match sent {
        Some(kept) if stamp < kept.1 => kept,
        _ => (quote.mid, stamp),
    }
}

/// Whether `quote`, the newest for a symbol at host time `now`, need be sequenced, given
/// `sent`, the mid and stamp (at most the host time it was sent) of the newest quote sequenced for
/// it on this connection. A repeat of that mid with a newer stamp changes nothing the engine
/// prices from but how long the venue counts as quoting, so it waits until `sent` is
/// `refresh_after` old (`None`: the market's rules are not known yet, so nothing waits). A quote
/// with a new mid goes at once, as does one dated before `sent`, which the engine will not store
/// but which is no repeat to hold back.
pub(super) fn verdict(
    sent: Option<(Fixed, Millis)>,
    quote: &SourceQuote,
    refresh_after: Option<Millis>,
    now: Millis,
) -> Verdict {
    let (Some((mid, ts)), Some(refresh_after)) = (sent, refresh_after) else { return Verdict::Send };
    // The engine stores a quote stamped ahead of its sequenced time as stamped at it.
    let stamp = quote.ts.min(now);
    if quote.mid != mid || stamp < ts {
        return Verdict::Send;
    }
    if stamp == ts {
        return Verdict::Redundant;
    }
    let due = ts.saturating_add(refresh_after);
    if now >= due { Verdict::Send } else { Verdict::Wait(due) }
}
