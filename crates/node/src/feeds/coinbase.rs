//! Coinbase Advanced Trade. The public `level2` channel sends each product's whole book as a
//! snapshot, then every change to it, stamped with the venue's event time. The adapter keeps the
//! price levels per product, so the top of book is current whenever any level changes. Messages
//! carry a per-connection `sequence_num`; a gap, or an update that cannot be applied, means the
//! kept book no longer matches the venue's, so the connection resyncs.

use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;
use engine_core::Fixed;
use engine_core::types::Millis;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;

use crate::feeds::venue::{Book, Frame, Refusal, check_time};

/// The most price levels kept on one side of a product's book, about ten times the deepest side
/// Coinbase publishes (BTC-USD, some 21,000 levels in October 2026), so a venue fault cannot grow
/// the book without bound.
const MAX_LEVELS: usize = 200_000;

/// The price levels of every product on one connection, rebuilt from `level2` messages.
pub(super) struct Books {
    /// The products the connection subscribed to; a change for any other is refused before it is
    /// kept, so the venue cannot grow books the feed never reads.
    products: BTreeSet<String>,
    /// The `sequence_num` of the last message read.
    sequence: Option<u64>,
    depths: BTreeMap<String, Depth>,
}

/// One product's price levels with a non-zero quantity.
#[derive(Default)]
struct Depth {
    bids: BTreeSet<Fixed>,
    offers: BTreeSet<Fixed>,
}

/// A channel message, or an error message (`type = "error"`), which has no sequence number.
#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow)]
    channel: Option<&'a str>,
    sequence_num: Option<u64>,
    #[serde(borrow)]
    events: Option<&'a RawValue>,
    message: Option<String>,
}

#[derive(Deserialize)]
struct Event<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    product_id: String,
    #[serde(borrow)]
    updates: Vec<Update<'a>>,
}

#[derive(Deserialize)]
struct Update<'a> {
    side: &'a str,
    event_time: &'a str,
    price_level: Fixed,
    new_quantity: Fixed,
}

impl Books {
    /// Empty books for a connection subscribed to `products`.
    pub(super) fn new(products: &[&str]) -> Self {
        let products = products.iter().map(|product| (*product).to_owned()).collect();
        Books { products, sequence: None, depths: BTreeMap::new() }
    }

    /// Reads one message at host time `now`, applying any book changes in it. Any change it cannot apply ends the
    /// connection, and with it this book.
    /// Error cases:
    /// - an error message from the venue, or a message that is neither an error nor a channel
    ///   message (the connection carries on)
    /// - a sequence gap, a sequenced message on an unknown channel, or a `level2` message that does
    ///   not parse or apply (the connection resyncs)
    pub(super) fn read(&mut self, text: &str, now: Millis) -> Result<Frame, Refusal> {
        let message: Message =
            serde_json::from_str(text).map_err(|e| Refusal::Message(format!("unparseable message: {e}")))?;
        let (Some(channel), Some(sequence)) = (message.channel, message.sequence_num) else {
            let error = message.message.ok_or_else(|| Refusal::Message("unexpected message".into()))?;
            return Err(Refusal::Message(format!("venue error: {error}")));
        };
        if let Some(last) = self.sequence.replace(sequence)
            && last.checked_add(1) != Some(sequence)
        {
            return Err(Refusal::Connection(format!("sequence gap: {last} then {sequence}")));
        }
        match channel {
            "subscriptions" => Ok(Frame::Control),
            "l2_data" => {
                let events = message.events.ok_or_else(|| Refusal::Connection("l2_data without events".into()))?;
                self.apply(events.get(), now).map(Frame::Books).map_err(Refusal::Connection)
            }
            // It took a sequence number, so it may have carried a change the book now lacks.
            other => Err(Refusal::Connection(format!("unexpected channel {other}"))),
        }
    }

    /// The best bid and offer kept for `product`, while its book has both sides.
    pub(super) fn top(&self, product: &str) -> Option<(Fixed, Fixed)> {
        let depth = self.depths.get(product)?;
        depth.bids.last().zip(depth.offers.first()).map(|(bid, ask)| (*bid, *ask))
    }

    /// Applies `level2` events and returns the top of each changed product's book, stamped with
    /// the latest event time applied to it. A product whose book has an empty side has no top.
    /// Error cases:
    /// - the events do not parse, an event type or side is unknown, a price level is outside the
    ///   valid input range, or a quantity is negative
    /// - an event time is implausible at `now` (see `check_time`), whether or not the change
    ///   leaves a top of book
    /// - an event is for a product the connection did not subscribe to, an update arrives for a
    ///   product with no snapshot, or an event has no updates
    fn apply(&mut self, events: &str, now: Millis) -> Result<Vec<Book>, String> {
        let events: Vec<Event> = serde_json::from_str(events).map_err(|e| format!("bad level2 events: {e}"))?;
        let tops: Vec<Option<Book>> =
            events.into_iter().map(|event| self.apply_event(event, now)).collect::<Result<_, _>>()?;
        Ok(tops.into_iter().flatten().collect())
    }

    /// Applies one product's snapshot or update.
    /// Error cases:
    /// - as for `apply`
    fn apply_event(&mut self, event: Event, now: Millis) -> Result<Option<Book>, String> {
        let Event { kind, product_id, updates } = event;
        if !self.products.contains(&product_id) {
            return Err(format!("{product_id} not subscribed"));
        }
        let depth = match kind {
            "snapshot" => {
                let depth = self.depths.entry(product_id.clone()).or_default();
                *depth = Depth::default();
                depth
            }
            "update" => {
                self.depths.get_mut(&product_id).ok_or_else(|| format!("{product_id} update before snapshot"))?
            }
            other => return Err(format!("unknown event type {other}")),
        };
        let mut ts: Option<Millis> = None;
        for update in updates {
            let levels = match update.side {
                "bid" => &mut depth.bids,
                "offer" => &mut depth.offers,
                other => return Err(format!("unknown side {other}")),
            };
            if !update.price_level.is_valid_input() {
                return Err(format!("{product_id} price level {} out of range", update.price_level));
            }
            if update.new_quantity.is_negative() {
                return Err(format!("{product_id} negative quantity {}", update.new_quantity));
            }
            if update.new_quantity.is_zero() {
                levels.remove(&update.price_level);
            } else if levels.insert(update.price_level) && levels.len() > MAX_LEVELS {
                return Err(format!("{product_id} {} side past {MAX_LEVELS} levels", update.side));
            }
            let event_time = DateTime::parse_from_rfc3339(update.event_time)
                .map_err(|e| format!("bad event_time {}: {e}", update.event_time))?
                .timestamp_millis();
            check_time(&product_id, event_time, now)?;
            ts = ts.max(Some(event_time));
        }
        let ts = ts.ok_or_else(|| format!("{product_id} event without updates"))?;
        let top = depth.bids.last().zip(depth.offers.first());
        Ok(top.map(|(bid, ask)| Book { symbol: product_id, bid: *bid, ask: *ask, ts }))
    }
}

/// The subscribe message for the `level2` books of `symbols`.
pub(super) fn subscriptions(symbols: &[&str]) -> Vec<String> {
    vec![json!({ "type": "subscribe", "product_ids": symbols, "channel": "level2" }).to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Recorded from wss://advanced-trade-ws.coinbase.com; the snapshot is cut to its top two
    // levels a side, and the update follows it on the same connection.
    const SNAPSHOT: &str = r#"{"channel":"l2_data","timestamp":"2026-10-20T09:51:15.867711707Z","sequence_num":11,"events":[{"type":"snapshot","product_id":"ETH-USD","updates":[{"side":"bid","event_time":"2026-10-20T09:51:15.714274Z","price_level":"2376.8","new_quantity":"1.5126332"},{"side":"bid","event_time":"2026-10-20T09:51:15.714274Z","price_level":"2376.76","new_quantity":"0.04539488"},{"side":"offer","event_time":"2026-10-20T09:51:15.714274Z","price_level":"2376.81","new_quantity":"1.50457515"},{"side":"offer","event_time":"2026-10-20T09:51:15.714274Z","price_level":"2376.82","new_quantity":"0.78536748"}]}]}"#;
    const UPDATE: &str = r#"{"channel":"l2_data","timestamp":"2026-10-20T09:51:15.877258658Z","sequence_num":12,"events":[{"type":"update","product_id":"ETH-USD","updates":[{"side":"bid","event_time":"2026-10-20T09:51:15.761244Z","price_level":"2341.14","new_quantity":"0.001"},{"side":"bid","event_time":"2026-10-20T09:51:15.761244Z","price_level":"2306.03","new_quantity":"0.001"}]}]}"#;
    const SUBSCRIPTIONS: &str = r#"{"channel":"subscriptions","timestamp":"2026-10-20T09:51:15.877783282Z","sequence_num":13,"events":[{"subscriptions":{"level2":["BTC-USD","ETH-USD"]}}]}"#;
    const ERROR: &str = r#"{"type":"error","message":"authentication failure"}"#;
    /// The host time messages are read at: just after the recorded ones.
    const NOW: Millis = 1_792_489_876_000;

    /// An ETH-USD top of book.
    fn top(bid: &str, ask: &str, ts: Millis) -> Frame {
        let book = Book { symbol: "ETH-USD".into(), bid: bid.parse().unwrap(), ask: ask.parse().unwrap(), ts };
        Frame::Books(vec![book])
    }

    /// `UPDATE` with its updates replaced, one `(side, event_time, price, quantity)` each.
    fn update(sequence: u64, updates: &[(&str, &str, &str, &str)]) -> String {
        let updates: Vec<_> = updates
            .iter()
            .map(|(side, at, price, qty)| json!({ "side": side, "event_time": at, "price_level": price, "new_quantity": qty }))
            .collect();
        let events = json!([{ "type": "update", "product_id": "ETH-USD", "updates": updates }]);
        json!({ "channel": "l2_data", "timestamp": "2026-10-20T09:51:15.9Z", "sequence_num": sequence, "events": events })
            .to_string()
    }

    #[test]
    fn a_snapshot_gives_the_top_of_book_at_the_venue_event_time() {
        // 2026-10-20T09:51:15.714274Z, truncated to the millisecond.
        assert_eq!(Books::new(&["ETH-USD"]).read(SNAPSHOT, NOW), Ok(top("2376.8", "2376.81", 1_792_489_875_714)));
    }

    #[test]
    fn updates_move_the_top_and_carry_their_latest_event_time() {
        let mut books = Books::new(&["ETH-USD"]);
        books.read(SNAPSHOT, NOW).unwrap();
        // Levels below the top change: the top is unchanged but now current at the update's time.
        assert_eq!(books.read(UPDATE, NOW), Ok(top("2376.8", "2376.81", 1_792_489_875_761)));
        // The best bid is pulled and a better offer arrives; the later of the two times stamps it.
        let moved = update(
            13,
            &[
                ("bid", "2026-10-20T09:51:15.950000Z", "2376.8", "0"),
                ("offer", "2026-10-20T09:51:15.940000Z", "2376.79", "0.5"),
            ],
        );
        assert_eq!(books.read(&moved, NOW), Ok(top("2376.76", "2376.79", 1_792_489_875_950)));
    }

    #[test]
    fn a_new_snapshot_replaces_the_product_book() {
        let mut books = Books::new(&["ETH-USD"]);
        books.read(SNAPSHOT, NOW).unwrap();
        let resent = SNAPSHOT
            .replace(r#""sequence_num":11"#, r#""sequence_num":12"#)
            .replace(r#""price_level":"2376.8""#, r#""price_level":"2376.7""#)
            .replace(r#""price_level":"2376.76""#, r#""price_level":"2376.69""#);
        // The earlier snapshot's better bid at 2376.8 is gone.
        assert_eq!(books.read(&resent, NOW), Ok(top("2376.7", "2376.81", 1_792_489_875_714)));
    }

    #[test]
    fn a_book_with_an_empty_side_has_no_top() {
        let mut books = Books::new(&["ETH-USD"]);
        books.read(SNAPSHOT, NOW).unwrap();
        let emptied = update(
            12,
            &[
                ("offer", "2026-10-20T09:51:15.950000Z", "2376.81", "0"),
                ("offer", "2026-10-20T09:51:15.950000Z", "2376.82", "0"),
            ],
        );
        assert_eq!(books.read(&emptied, NOW), Ok(Frame::Books(vec![])));
    }

    #[test]
    fn a_sequenced_message_on_an_unknown_channel_resyncs() {
        let mut books = Books::new(&["ETH-USD"]);
        books.read(SNAPSHOT, NOW).unwrap();
        let unknown = UPDATE.replace(r#""channel":"l2_data""#, r#""channel":"l2_delta""#);
        assert!(matches!(books.read(&unknown, NOW), Err(Refusal::Connection(reason)) if reason.contains("l2_delta")));
    }

    #[test]
    fn acknowledgements_carry_no_price_and_venue_errors_are_refused() {
        let mut books = Books::new(&["ETH-USD"]);
        assert_eq!(books.read(SUBSCRIPTIONS, NOW), Ok(Frame::Control));
        assert_eq!(books.read(ERROR, NOW), Err(Refusal::Message("venue error: authentication failure".into())));
        assert!(matches!(books.read(r#"{"channel":"l2_data"}"#, NOW), Err(Refusal::Message(_))));
        assert!(matches!(books.read("{", NOW), Err(Refusal::Message(_))));
    }

    #[test]
    fn a_sequence_gap_resyncs_the_connection() {
        let mut books = Books::new(&["ETH-USD"]);
        books.read(SNAPSHOT, NOW).unwrap();
        let skipped = UPDATE.replace(r#""sequence_num":12"#, r#""sequence_num":14"#);
        assert_eq!(books.read(&skipped, NOW), Err(Refusal::Connection("sequence gap: 11 then 14".into())));
    }

    #[test]
    fn an_update_that_cannot_be_applied_resyncs_the_connection() {
        let cases = [
            UPDATE.replace(r#""price_level":"2341.14""#, r#""price_level":"2341.1.4""#),
            UPDATE.replace(r#""side":"bid""#, r#""side":"ask""#),
            UPDATE.replace(r#""new_quantity":"0.001""#, r#""new_quantity":"-0.001""#),
            // A level below the top that no working venue could quote, set or removed.
            UPDATE.replace(r#""price_level":"2306.03""#, r#""price_level":"-1""#),
            UPDATE.replace(
                r#""price_level":"2306.03","new_quantity":"0.001""#,
                r#""price_level":"0","new_quantity":"0""#,
            ),
            UPDATE.replace("2026-10-20T09:51:15.761244Z", "later"),
            UPDATE.replace("2026-10-20T09:51:15.761244Z", "1969-12-31T23:59:59.999Z"),
            // Corrupt, thousands of years ahead.
            UPDATE.replace("2026-10-20T09:51:15.761244Z", "9999-12-31T23:59:59Z"),
            UPDATE.replace(r#""type":"update""#, r#""type":"delta""#),
            UPDATE.replace(r#""product_id":"ETH-USD""#, r#""product_id":"BTC-USD""#),
        ];
        for case in cases {
            let mut books = Books::new(&["ETH-USD"]);
            books.read(SNAPSHOT, NOW).unwrap();
            assert!(matches!(books.read(&case, NOW), Err(Refusal::Connection(_))), "applied {case}");
        }
    }

    #[test]
    fn a_book_side_past_its_level_cap_resyncs_the_connection() {
        let bids = |sequence: u64, levels: std::ops::Range<usize>| {
            let level = |i: usize| {
                format!(
                    r#"{{"side":"bid","event_time":"2026-10-20T09:51:15.761244Z","price_level":"1.{i:06}","new_quantity":"0.001"}}"#
                )
            };
            let updates: Vec<String> = levels.map(level).collect();
            let events = format!(r#"[{{"type":"update","product_id":"ETH-USD","updates":[{}]}}]"#, updates.join(","));
            format!(
                r#"{{"channel":"l2_data","timestamp":"2026-10-20T09:51:15.9Z","sequence_num":{sequence},"events":{events}}}"#
            )
        };
        let mut books = Books::new(&["ETH-USD"]);
        books.read(SNAPSHOT, NOW).unwrap();
        // The snapshot holds two bids, so this fills the side to the cap, which is still a book.
        assert!(books.read(&bids(12, 0..MAX_LEVELS - 2), NOW).is_ok());
        // One more level is past it.
        let past = books.read(&bids(13, MAX_LEVELS..MAX_LEVELS + 1), NOW);
        assert!(matches!(&past, Err(Refusal::Connection(reason)) if reason.contains("past 200000 levels")), "{past:?}");
    }

    #[test]
    fn the_subscription_names_every_symbol_on_the_level2_channel() {
        let sent: serde_json::Value = serde_json::from_str(&subscriptions(&["BTC-USD", "ETH-USD"])[0]).unwrap();
        assert_eq!(sent, json!({ "type": "subscribe", "product_ids": ["BTC-USD", "ETH-USD"], "channel": "level2" }));
    }
}
