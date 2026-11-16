//! Test support: a loopback Kraken v2 `book` endpoint that sends what Kraken sends. A subscription
//! gets an acknowledgement and each symbol's top ten levels a side, then an update to its top 25
//! times a second; every snapshot and update carries Kraken's checksum of the book it leaves, and a
//! ping is answered in stream order, behind what was sent before it.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::interval;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use crate::clock::now_ms;
use crate::feeds::kraken;
use crate::feeds::tests::coinbase_support::UPDATES_PER_SECOND;
use crate::feeds::tests::fake::{Conn, listen, rfc3339};

/// The levels a side Kraken's `book` subscription keeps, as the feed subscribes.
const DEPTH: usize = 10;

/// The endpoint, serving `(symbol, mid in cents)` books to each connection until it closes or this
/// is dropped.
pub struct KrakenVenue {
    pub url: String,
    serving: JoinHandle<()>,
}

/// One symbol's book: each level's price, in cents, and its size as Kraken writes it.
struct Book {
    symbol: String,
    mid_cents: i64,
    bids: BTreeMap<i64, &'static str>,
    asks: BTreeMap<i64, &'static str>,
}

impl KrakenVenue {
    /// An endpoint on a loopback port serving `books`, each a symbol and its mid in cents.
    pub async fn start(books: &[(&'static str, i64)]) -> Self {
        let (listener, url) = listen().await;
        let books = books.to_vec();
        let serving = tokio::spawn(async move {
            // Dropped with this task, which ends every connection.
            let mut connections = JoinSet::new();
            while let Ok((tcp, _)) = listener.accept().await {
                let books = books.clone();
                connections.spawn(async move {
                    if let Ok(ws) = accept_async(tcp).await {
                        serve(ws, &books).await;
                    }
                });
            }
        });
        KrakenVenue { url, serving }
    }
}

impl Drop for KrakenVenue {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

impl Book {
    /// `symbol`'s book a cent either side of `mid_cents`, ten levels deep.
    fn new(symbol: &str, mid_cents: i64) -> Self {
        let depth = i64::try_from(DEPTH).expect("ten");
        let bids = (1..=depth).map(|step| (mid_cents - step, "1.50000000")).collect();
        let asks = (1..=depth).map(|step| (mid_cents + step, "1.50000000")).collect();
        Book { symbol: symbol.to_owned(), mid_cents, bids, asks }
    }

    /// The book as a snapshot message.
    fn snapshot(&self) -> String {
        let bids: Vec<(i64, &str)> = self.bids.iter().rev().map(|(price, qty)| (*price, *qty)).collect();
        let asks: Vec<(i64, &str)> = self.asks.iter().map(|(price, qty)| (*price, *qty)).collect();
        message("snapshot", &self.symbol, &bids, &asks, self.checksum())
    }

    /// Sets a bid at the mid on odd `tick`s and clears it on even ones, so the mid moves every
    /// time, keeping the best ten levels a side as Kraken does; returns the update message.
    fn update(&mut self, tick: u64) -> String {
        let qty = if tick % 2 == 1 { "0.25000000" } else { "0" };
        if qty == "0" {
            self.bids.remove(&self.mid_cents);
        } else {
            self.bids.insert(self.mid_cents, qty);
        }
        while self.bids.len() > DEPTH {
            self.bids.pop_first();
        }
        message("update", &self.symbol, &[(self.mid_cents, qty)], &[], self.checksum())
    }

    /// Kraken's checksum of the book: the asks from the best up, then the bids from the best down.
    fn checksum(&self) -> u32 {
        let (asks, bids) = (self.asks.iter(), self.bids.iter().rev());
        let levels: Vec<(String, &str)> = asks.chain(bids).map(|(price, qty)| (price_text(*price), *qty)).collect();
        kraken::checksum(levels.iter().map(|(price, qty)| (price.as_str(), *qty)))
    }
}

/// Serves one connection until it closes: each subscription acknowledged and answered with its
/// symbols' snapshots, then an update to each `UPDATES_PER_SECOND` times a second. tungstenite
/// answers a ping when it reads it, behind every message sent before.
async fn serve(mut ws: Conn, served: &[(&'static str, i64)]) {
    let mut books: Vec<Book> = Vec::new();
    let mut updates = interval(Duration::from_secs(1) / UPDATES_PER_SECOND);
    let mut tick = 0;
    loop {
        let messages = tokio::select! {
            read = ws.next() => match read {
                Some(Ok(Message::Text(text))) => {
                    let request: Value = serde_json::from_str(text.as_str()).expect("the feed sends JSON");
                    let symbols = request["params"]["symbol"].as_array().cloned().unwrap_or_default();
                    let mut answer = Vec::new();
                    for (symbol, mid_cents) in served.iter().filter(|(symbol, _)| symbols.iter().any(|s| s == symbol)) {
                        let result = json!({ "channel": "book", "depth": DEPTH, "snapshot": true, "symbol": symbol });
                        answer.push(json!({ "method": "subscribe", "result": result, "success": true }).to_string());
                        books.push(Book::new(symbol, *mid_cents));
                        answer.push(books.last().expect("just pushed").snapshot());
                    }
                    answer
                }
                Some(Ok(_)) => Vec::new(),
                _ => return,
            },
            _ = updates.tick() => {
                tick += 1;
                books.iter_mut().map(|book| book.update(tick)).collect()
            }
        };
        for message in messages {
            if ws.send(Message::text(message)).await.is_err() {
                return;
            }
        }
    }
}

/// A book message of `kind` for `symbol`, with each level's price in cents and its size.
fn message(kind: &str, symbol: &str, bids: &[(i64, &str)], asks: &[(i64, &str)], checksum: u32) -> String {
    let levels = |levels: &[(i64, &str)]| {
        let written: Vec<String> =
            levels.iter().map(|(price, qty)| format!(r#"{{"price":{},"qty":{qty}}}"#, price_text(*price))).collect();
        written.join(",")
    };
    let at = rfc3339(now_ms());
    format!(
        r#"{{"channel":"book","type":"{kind}","data":[{{"symbol":"{symbol}","bids":[{}],"asks":[{}],"checksum":{checksum},"timestamp":"{at}"}}]}}"#,
        levels(bids),
        levels(asks)
    )
}

/// A price in cents as Kraken writes it.
fn price_text(cents: i64) -> String {
    format!("{}.{:02}", cents / 100, cents % 100)
}
