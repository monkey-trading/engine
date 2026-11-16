//! Coinbase Exchange's public `level2_batch` feed: each product's whole book as a snapshot, then its
//! changes in batches every 50 ms, each message stamped with the venue's time. It serves Coinbase's
//! books from another host than Advanced Trade's, so a connection here keeps the `coinbase` source
//! quoting while one there fails. The adapter keeps the price levels per product, as `coinbase`
//! does; the feed numbers no messages, so a change it cannot apply is the only sign of a lost one.

use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;
use engine_core::Fixed;
use engine_core::types::Millis;
use serde::Deserialize;
use serde_json::json;

use crate::feeds::coinbase::Depth;
use crate::feeds::venue::{Book, Frame, Refusal, check_time};

/// The price levels of every product on one connection, rebuilt from `level2_batch` messages.
pub(super) struct Books {
    /// The products the connection subscribed to; a message for any other is refused before it is
    /// kept, so the venue cannot grow books the feed never reads.
    products: BTreeSet<String>,
    depths: BTreeMap<String, Depth>,
}

/// One message: a snapshot's levels as `[price, size]`, an update's changes as `[side, price, size]`,
/// or an error's message and reason.
#[derive(Deserialize)]
struct Message<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    product_id: Option<String>,
    #[serde(borrow)]
    time: Option<&'a str>,
    #[serde(default)]
    bids: Vec<(Fixed, Fixed)>,
    #[serde(default)]
    asks: Vec<(Fixed, Fixed)>,
    #[serde(default, borrow)]
    changes: Vec<(&'a str, Fixed, Fixed)>,
    message: Option<String>,
    reason: Option<String>,
}

impl Books {
    /// Empty books for a connection subscribed to `products`.
    pub(super) fn new(products: &[&str]) -> Self {
        Books { products: products.iter().map(|product| (*product).to_owned()).collect(), depths: BTreeMap::new() }
    }

    /// Reads one message at host time `now`, applying any book change in it.
    /// Error cases:
    /// - an error message from the venue, or a message of a type the feed does not define (the
    ///   connection carries on)
    /// - a snapshot or update that does not parse or apply, is for a product the connection did
    ///   not subscribe to, has no time or an implausible one (see `check_time`), or an update before
    ///   its product's snapshot (the connection resyncs)
    pub(super) fn read(&mut self, text: &str, now: Millis) -> Result<Frame, Refusal> {
        let message: Message =
            serde_json::from_str(text).map_err(|e| Refusal::Connection(format!("unparseable message: {e}")))?;
        match message.kind {
            "subscriptions" | "heartbeat" => Ok(Frame::Control),
            "snapshot" | "l2update" => self.apply(message, now).map(Frame::Books).map_err(Refusal::Connection),
            "error" => {
                let (message, reason) = (message.message.unwrap_or_default(), message.reason.unwrap_or_default());
                Err(Refusal::Message(format!("venue error: {message}: {reason}")))
            }
            other => Err(Refusal::Message(format!("unexpected message type {other}"))),
        }
    }

    /// The best bid and offer kept for `product`, while its book has both sides.
    pub(super) fn top(&self, product: &str) -> Option<(Fixed, Fixed)> {
        self.depths.get(product)?.top()
    }

    /// Applies a snapshot, which replaces its product's book, or an update, and returns the
    /// product's top stamped with the message's time, or none while a side is empty.
    /// Error cases:
    /// - as for `read`, for a snapshot or update
    fn apply(&mut self, message: Message, now: Millis) -> Result<Vec<Book>, String> {
        let product = message.product_id.ok_or("a book message without a product")?;
        if !self.products.contains(&product) {
            return Err(format!("{product} not subscribed"));
        }
        let time = message.time.ok_or_else(|| format!("{product} message without a time"))?;
        let ts = DateTime::parse_from_rfc3339(time).map_err(|e| format!("bad time {time}: {e}"))?.timestamp_millis();
        check_time(&product, ts, now)?;
        let depth = if message.kind == "snapshot" {
            self.depths.insert(product.clone(), Depth::default());
            self.depths.get_mut(&product).expect("just inserted")
        } else {
            self.depths.get_mut(&product).ok_or_else(|| format!("{product} update before snapshot"))?
        };
        let levels = message.bids.iter().map(|(price, size)| (true, *price, *size));
        let offers = message.asks.iter().map(|(price, size)| (false, *price, *size));
        for (bid, price, size) in levels.chain(offers) {
            depth.set(&product, bid, price, size)?;
        }
        for (side, price, size) in message.changes {
            let bid = match side {
                "buy" => true,
                "sell" => false,
                other => return Err(format!("unknown side {other}")),
            };
            depth.set(&product, bid, price, size)?;
        }
        Ok(depth.top().map(|(bid, ask)| Book { symbol: product, bid, ask, ts }).into_iter().collect())
    }
}

/// The subscribe message for the `level2_batch` books of `symbols`.
pub(super) fn subscriptions(symbols: &[&str]) -> Vec<String> {
    vec![json!({ "type": "subscribe", "product_ids": symbols, "channels": ["level2_batch"] }).to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    // Recorded from wss://ws-feed.exchange.coinbase.com on 2026-11-16, the snapshot cut to its top
    // two levels a side.
    const SNAPSHOT: &str = r#"{"type":"snapshot","product_id":"BTC-USD","asks":[["64979.23","0.23651287"],["64979.54","0.12311567"]],"bids":[["64979.22","0.00000090"],["64978.89","0.00015690"]],"time":"2026-11-16T18:58:21.052411Z"}"#;
    const ERROR: &str =
        r#"{"type":"error","message":"Failed to subscribe","reason":"NOPE-USD is not a valid product"}"#;
    const SUBSCRIPTIONS: &str =
        r#"{"type":"subscriptions","channels":[{"name":"level2_50","product_ids":["BTC-USD"],"account_ids":null}]}"#;
    /// The host time messages are read at: just after the recorded ones.
    const NOW: Millis = 1_794_855_502_000;

    fn top(bid: &str, ask: &str, ts: Millis) -> Frame {
        Frame::Books(vec![Book { symbol: "BTC-USD".into(), bid: bid.parse().unwrap(), ask: ask.parse().unwrap(), ts }])
    }

    /// An update with `changes`, each `[side, price, size]`, at `time`.
    fn update(time: &str, changes: &[[&str; 3]]) -> String {
        json!({ "type": "l2update", "product_id": "BTC-USD", "changes": changes, "time": time }).to_string()
    }

    #[test]
    fn a_snapshot_and_its_updates_give_the_top_of_book_at_the_venue_time() {
        let mut books = Books::new(&["BTC-USD"]);
        assert_eq!(books.read(SNAPSHOT, NOW), Ok(top("64979.22", "64979.23", 1_794_855_501_052)));
        // The best bid is pulled and a better offer arrives.
        let moved = update("2026-11-16T18:58:21.102Z", &[["buy", "64979.22", "0"], ["sell", "64979.0", "0.5"]]);
        assert_eq!(books.read(&moved, NOW), Ok(top("64978.89", "64979.0", 1_794_855_501_102)));
        // An offer pulled empties nothing: the next one up is the top.
        let pulled = update("2026-11-16T18:58:21.152Z", &[["sell", "64979.0", "0"]]);
        assert_eq!(books.read(&pulled, NOW), Ok(top("64978.89", "64979.23", 1_794_855_501_152)));
    }

    #[test]
    fn acknowledgements_carry_no_price_and_venue_errors_are_refused() {
        let mut books = Books::new(&["BTC-USD"]);
        assert_eq!(books.read(SUBSCRIPTIONS, NOW), Ok(Frame::Control));
        let error = books.read(ERROR, NOW);
        assert_eq!(
            error,
            Err(Refusal::Message("venue error: Failed to subscribe: NOPE-USD is not a valid product".into()))
        );
        assert!(matches!(books.read(r#"{"type":"ticker"}"#, NOW), Err(Refusal::Message(_))));
    }

    #[test]
    fn a_change_that_cannot_be_applied_resyncs_the_connection() {
        let at = "2026-11-16T18:58:21.102Z";
        let cases = [
            update(at, &[["buy", "-1", "1"]]),
            update(at, &[["buy", "64979.0", "-1"]]),
            update(at, &[["bid", "64979.0", "1"]]),
            update("later", &[["buy", "64979.0", "1"]]),
            update("9999-12-31T23:59:59Z", &[["buy", "64979.0", "1"]]),
            update(at, &[["buy", "64979.0", "1"]]).replace("BTC-USD", "ETH-USD"),
            update(at, &[["buy", "64979.0", "1"]]).replace(r#","time":"2026-11-16T18:58:21.102Z""#, ""),
            "{".to_owned(),
        ];
        for case in cases {
            let mut books = Books::new(&["BTC-USD"]);
            books.read(SNAPSHOT, NOW).unwrap();
            assert!(matches!(books.read(&case, NOW), Err(Refusal::Connection(_))), "applied {case}");
        }
        // An update before its product's snapshot has nothing to apply to.
        let mut books = Books::new(&["BTC-USD"]);
        assert!(matches!(books.read(&update(at, &[["buy", "64979.0", "1"]]), NOW), Err(Refusal::Connection(_))));
    }

    #[test]
    fn the_subscription_names_every_symbol_on_the_level2_batch_channel() {
        let sent: serde_json::Value = serde_json::from_str(&subscriptions(&["BTC-USD", "ETH-USD"])[0]).unwrap();
        assert_eq!(
            sent,
            json!({ "type": "subscribe", "product_ids": ["BTC-USD", "ETH-USD"], "channels": ["level2_batch"] })
        );
    }
}
