//! Test support: a loopback Coinbase Advanced Trade `level2` endpoint that behaves as the venue does
//! under its real load. Each subscription gets a snapshot of its product's whole book, megabytes for
//! BTC-USD and ETH-USD, then an update to its top at Coinbase's rate; every message is numbered on its
//! connection, and a ping is answered in stream order, behind what was sent before it. A subscription
//! can be served from a source behind the venue's own, its snapshot and updates stamped that far in
//! the past, as Coinbase served one product on a connection on 16 November 2026 while the other
//! stayed current and its pings were answered at once.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use engine_core::types::Millis;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{interval, timeout};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use crate::clock::now_ms;
use crate::feeds::tests::fake::{Conn, WAIT, listen, rfc3339};

/// About the rate Coinbase updates BTC-USD's and ETH-USD's books at, a second.
pub const UPDATES_PER_SECOND: u32 = 25;

/// BTC-USD at about Coinbase's depth: a snapshot of some 4 MB.
pub const BTC: Product = Product { id: "BTC-USD", mid_cents: 6_500_000, depth: 18_000 };

/// ETH-USD at about Coinbase's depth: a snapshot of some 2 MB.
pub const ETH: Product = Product { id: "ETH-USD", mid_cents: 182_000, depth: 9_000 };

/// How far behind the venue's own time each product's next subscriptions are served, in order.
type Behind = Arc<Mutex<BTreeMap<&'static str, VecDeque<Duration>>>>;

/// A product the endpoint serves: its id, the mid its book centres on, in cents, and the levels on
/// each side of its snapshot, a cent apart.
#[derive(Debug, Clone, Copy)]
pub struct Product {
    pub id: &'static str,
    pub mid_cents: i64,
    pub depth: usize,
}

/// The endpoint, serving each connection until it closes or this is dropped.
pub struct CoinbaseVenue {
    pub url: String,
    /// Every text message a feed sent, with the number of the connection it came on, from 0.
    received: mpsc::UnboundedReceiver<(usize, String)>,
    behind: Behind,
    serving: JoinHandle<()>,
}

/// One connection's subscriptions: each product's book and how far behind it is served.
struct Subscribed {
    products: BTreeMap<&'static str, (Product, Duration)>,
    sequence: u64,
}

impl CoinbaseVenue {
    /// An endpoint on a loopback port serving `products`, every subscription current.
    pub async fn start(products: &[Product]) -> Self {
        let (listener, url) = listen().await;
        let (sent, received) = mpsc::unbounded_channel();
        let behind = Behind::default();
        let (products, shared) = (products.to_vec(), Arc::clone(&behind));
        let serving = tokio::spawn(async move {
            // Dropped with this task, which ends every connection.
            let mut connections = JoinSet::new();
            for connection in 0.. {
                let Ok((tcp, _)) = listener.accept().await else { return };
                let (products, behind, sent) = (products.clone(), Arc::clone(&shared), sent.clone());
                connections.spawn(async move {
                    if let Ok(ws) = accept_async(tcp).await {
                        serve(ws, connection, products, behind, sent).await;
                    }
                });
            }
        });
        CoinbaseVenue { url, received, behind, serving }
    }

    /// Serves `product`'s next subscriptions, on any connection, each from a source this far
    /// behind the venue's own time, in order; one past them is current.
    pub fn behind(&self, product: &'static str, lags: &[Duration]) {
        self.behind.lock().expect("the lags").entry(product).or_default().extend(lags);
    }

    /// The next text message a feed sent, with its connection's number, failing after `WAIT`.
    pub async fn next_received(&mut self) -> (usize, String) {
        timeout(WAIT, self.received.recv()).await.expect("no message in time").expect("the endpoint stopped")
    }

    /// Every message received and not yet read.
    pub fn drain(&mut self) -> Vec<(usize, String)> {
        std::iter::from_fn(|| self.received.try_recv().ok()).collect()
    }
}

impl Drop for CoinbaseVenue {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

impl Subscribed {
    /// The next message's `sequence_num`.
    fn next(&mut self) -> u64 {
        let sequence = self.sequence;
        self.sequence += 1;
        sequence
    }

    /// The answer to a feed's `request`: for a subscription, a snapshot of each product newly
    /// subscribed, then the acknowledgement listing every product the connection holds; for an
    /// unsubscription, the acknowledgement. A product it does not serve is passed over.
    fn answer(&mut self, request: &str, served: &[Product], behind: &Behind) -> Vec<String> {
        let request: Value = serde_json::from_str(request).expect("the feed sends JSON");
        let ids = request["product_ids"].as_array().map(|ids| ids.iter().filter_map(Value::as_str).collect::<Vec<_>>());
        let mut answer = Vec::new();
        for id in ids.unwrap_or_default() {
            let Some(product) = served.iter().find(|product| product.id == id) else { continue };
            match request["type"].as_str() {
                Some("subscribe") if !self.products.contains_key(product.id) => {
                    let lags = &mut behind.lock().expect("the lags");
                    let lag = lags.get_mut(product.id).and_then(VecDeque::pop_front).unwrap_or_default();
                    self.products.insert(product.id, (*product, lag));
                    let sequence = self.next();
                    answer.push(snapshot(sequence, product, now_ms() - millis(lag)));
                }
                Some("unsubscribe") => {
                    self.products.remove(product.id);
                }
                _ => {}
            }
        }
        let held: Vec<&str> = self.products.keys().copied().collect();
        let events = json!([{ "subscriptions": { "level2": held } }]);
        let sequence = self.next();
        answer.push(json!({ "channel": "subscriptions", "timestamp": rfc3339(now_ms()), "sequence_num": sequence, "events": events }).to_string());
        answer
    }

    /// An update to each product's top: a bid at its mid is set on odd `tick`s and cleared on
    /// even ones, so its mid moves every time.
    fn updates(&mut self, tick: u64) -> Vec<String> {
        let products: Vec<(Product, Duration)> = self.products.values().copied().collect();
        products
            .into_iter()
            .map(|(product, lag)| {
                let quantity = if tick % 2 == 1 { "0.25" } else { "0" };
                let level = level("bid", product.mid_cents, quantity, now_ms() - millis(lag));
                let events = json!([{ "type": "update", "product_id": product.id, "updates": [level] }]);
                let sequence = self.next();
                json!({ "channel": "l2_data", "timestamp": rfc3339(now_ms()), "sequence_num": sequence, "events": events })
                    .to_string()
            })
            .collect()
    }
}

/// Serves one connection until it closes: each request answered, and each subscribed product
/// updated `UPDATES_PER_SECOND` times a second. tungstenite answers a ping when it reads it, behind
/// every message sent before.
async fn serve(
    mut ws: Conn,
    connection: usize,
    served: Vec<Product>,
    behind: Behind,
    received: mpsc::UnboundedSender<(usize, String)>,
) {
    let mut subscribed = Subscribed { products: BTreeMap::new(), sequence: 0 };
    let mut updates = interval(Duration::from_secs(1) / UPDATES_PER_SECOND);
    let mut tick = 0;
    loop {
        let messages = tokio::select! {
            read = ws.next() => match read {
                Some(Ok(Message::Text(text))) => {
                    let _ = received.send((connection, text.as_str().to_owned()));
                    subscribed.answer(text.as_str(), &served, &behind)
                }
                Some(Ok(_)) => Vec::new(),
                _ => return,
            },
            _ = updates.tick() => {
                tick += 1;
                subscribed.updates(tick)
            }
        };
        for message in messages {
            if ws.send(Message::text(message)).await.is_err() {
                return;
            }
        }
    }
}

/// A product's snapshot at venue time `ts`: `depth` bids a cent apart below its mid and as many
/// offers above it.
fn snapshot(sequence: u64, product: &Product, ts: Millis) -> String {
    let depth = i64::try_from(product.depth).expect("a test depth");
    let bids = (1..=depth).map(|step| level("bid", product.mid_cents - step, "1.5", ts));
    let offers = (1..=depth).map(|step| level("offer", product.mid_cents + step, "1.5", ts));
    let events =
        json!([{ "type": "snapshot", "product_id": product.id, "updates": bids.chain(offers).collect::<Vec<_>>() }]);
    json!({ "channel": "l2_data", "timestamp": rfc3339(now_ms()), "sequence_num": sequence, "events": events })
        .to_string()
}

/// One price level, `cents` its price, at venue time `ts`.
fn level(side: &str, cents: i64, quantity: &str, ts: Millis) -> Value {
    let price = format!("{}.{:02}", cents / 100, cents % 100);
    json!({ "side": side, "event_time": rfc3339(ts), "price_level": price, "new_quantity": quantity })
}

/// `lag` in milliseconds.
fn millis(lag: Duration) -> Millis {
    Millis::try_from(lag.as_millis()).expect("a test lag")
}
