//! The checks every venue's books pass before they are sequenced, shared by the adapters.

use std::collections::BTreeMap;

use engine_core::Fixed;
use engine_core::price::SourceQuote;
use engine_core::types::Millis;

use crate::config::Venue;
use crate::feeds::venue::{Book, Instrument};

/// The furthest a venue time may lead the host clock and still be clock skew. Beyond it the
/// message is corrupt, such as a time in the wrong unit, so it is refused, and the Coinbase and
/// Kraken books resync rather than keep a change from it. A smaller lead past the engine's
/// `MAX_QUOTE_LEAD_MS` only drops the quote (see `Pending::add`); that check alone would keep the
/// corrupt change in a kept book.
const MAX_PLAUSIBLE_LEAD_MS: Millis = 24 * 60 * 60 * 1_000;

/// The venue's mid for one book, as a quote for the instrument its symbol names, with the symbol,
/// refusing a book no working venue could show at host time `now`.
/// Error cases:
/// - the symbol is not one this connection subscribed to
/// - the bid or ask is outside the valid input range, or the book is crossed or locked
/// - the timestamp is implausible (see `check_time`)
pub(in crate::feeds) fn to_quote(
    venue: Venue,
    instruments: &BTreeMap<String, Instrument>,
    book: Book,
    now: Millis,
) -> Result<(String, SourceQuote), String> {
    let Book { symbol, bid, ask, ts } = book;
    let instrument = instruments.get(&symbol).ok_or_else(|| format!("unsubscribed symbol {symbol}"))?;
    if !bid.is_valid_input() || !ask.is_valid_input() {
        return Err(format!("{symbol} bid {bid} or ask {ask} out of range"));
    }
    if bid >= ask {
        return Err(format!("{symbol} book crossed: bid {bid}, ask {ask}"));
    }
    check_time(&symbol, ts, now)?;
    let mid = (bid + ask).div_int(2);
    let quote = SourceQuote { source: venue.name().to_owned(), mid, ts, contract: instrument.contract.clone() };
    Ok((symbol, quote))
}

/// Refuses a top-of-book size no working venue could show.
/// Error cases:
/// - `size` is not positive, or is above `Fixed::MAX_INPUT`
pub(in crate::feeds) fn check_size(symbol: &str, side: &str, size: Fixed) -> Result<(), String> {
    if !size.is_valid_input() {
        return Err(format!("{symbol} {side} size {size} out of range"));
    }
    Ok(())
}

/// Refuses a venue time no working venue could send at host time `now`. A time up to
/// `MAX_PLAUSIBLE_LEAD_MS` ahead is not refused: refusing it would leave the engine on older
/// quotes when the host clock is the one behind.
/// Error cases:
/// - `ts` is before the epoch, or more than `MAX_PLAUSIBLE_LEAD_MS` after `now`
pub(in crate::feeds) fn check_time(symbol: &str, ts: Millis, now: Millis) -> Result<(), String> {
    if ts < 0 {
        return Err(format!("{symbol} timestamp {ts} before the epoch"));
    }
    if ts > now.saturating_add(MAX_PLAUSIBLE_LEAD_MS) {
        return Err(format!("{symbol} timestamp {ts} is further ahead of the host clock than any clock error"));
    }
    Ok(())
}
