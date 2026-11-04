//! Test support: loopback market-data vendors that speak Massive's and TraderMade's protocols,
//! logins included, and stream each subscribed symbol's quotes every 20ms, as a shared price
//! function sets them, until the feed hangs up. Each accepts connection after connection, so a
//! feed's reconnects find it again.

use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use engine_core::Fixed;
use engine_core::types::Millis;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use crate::clock::now_ms;
use crate::feeds::tests::fake::{Conn, WAIT, next_text};

/// A vendor's bid and ask for a symbol at a time, or none while it quotes nothing.
pub type Prices = Arc<dyn Fn(&str, Millis) -> Option<(Fixed, Fixed)> + Send + Sync>;

/// The API key every fake vendor expects.
pub const KEY: &str = "test-key-0123456789abcdef01234567";
/// How often a fake vendor sends each symbol's quote.
const EVERY: Duration = Duration::from_millis(20);

/// Serves Massive: answers the authentication, reads the subscription, then streams `C` events
/// for `C.` symbols and `Q` events for `Q.` symbols.
pub fn serve_massive(listener: TcpListener, prices: Prices) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some((mut ws, auth)) = accept_next(&listener).await else { continue };
            let prices = prices.clone();
            tokio::spawn(async move {
                assert_eq!(serde_json::from_str::<Value>(&auth).unwrap(), json!({ "action": "auth", "params": KEY }));
                let status =
                    |status: &str| json!([{ "ev": "status", "status": status, "message": status }]).to_string();
                if ws.send(Message::text(status("auth_success"))).await.is_err() {
                    return;
                }
                let subscribe: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
                let params = subscribe["params"].as_str().expect("params").to_owned();
                let _ = ws.send(Message::text(status("success"))).await;
                let mut every = interval(EVERY);
                loop {
                    every.tick().await;
                    let ts = now_ms();
                    let events: Vec<Value> = params
                        .split(',')
                        .filter_map(|param| {
                            let (channel, symbol) = param.split_once('.')?;
                            let (bid, ask) = prices(symbol, ts)?;
                            let (bid, ask) = (number(bid), number(ask));
                            Some(match channel {
                                "C" => json!({ "ev": "C", "p": symbol.replace('-', "/"), "x": 44, "b": bid, "a": ask, "t": ts }),
                                _ => json!({ "ev": "Q", "sym": symbol, "bp": bid, "bs": 10, "ap": ask, "as": 12, "t": ts }),
                            })
                        })
                        .collect();
                    if ws.send(Message::text(Value::Array(events).to_string())).await.is_err() {
                        return;
                    }
                }
            });
        }
    })
}

/// Serves TraderMade: answers the login, acknowledges the subscription, then streams `QUOTE`
/// messages for each pair.
pub fn serve_tradermade(listener: TcpListener, prices: Prices) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some((mut ws, login)) = accept_next(&listener).await else { continue };
            let prices = prices.clone();
            tokio::spawn(async move {
                let login: Value = serde_json::from_str(&login).unwrap();
                assert_eq!(login, json!({ "action": "login", "key": KEY, "fmt": "JSON" }));
                let ok = json!({ "type": "login_ok", "key": KEY, "fmt": "JSON" }).to_string();
                if ws.send(Message::text(ok)).await.is_err() {
                    return;
                }
                let subscribe: Value = serde_json::from_str(&next_text(&mut ws).await).unwrap();
                let symbols: Vec<String> = subscribe["symbols"]
                    .as_array()
                    .expect("symbols")
                    .iter()
                    .map(|s| s.as_str().expect("a symbol").trim_end_matches(":QUOTE").to_owned())
                    .collect();
                let ack = json!({ "type": "sub_ack", "accepted": [], "denied": [], "invalid": [] }).to_string();
                let _ = ws.send(Message::text(ack)).await;
                let mut every = interval(EVERY);
                loop {
                    every.tick().await;
                    let ts = now_ms();
                    for symbol in &symbols {
                        let Some((bid, ask)) = prices(symbol, ts) else { continue };
                        let at = DateTime::from_timestamp_millis(ts).expect("in range").format("%Y%m%d-%H:%M:%S%.3f");
                        let quote = json!({ "s": symbol, "t": "QUOTE", "b": bid.to_string(), "a": ask.to_string(),
                            "bv": "1000000", "av": "1000000", "ts": at.to_string() });
                        if ws.send(Message::text(quote.to_string())).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    })
}

/// The next WebSocket connection, however long it takes, and the first message the feed sends on
/// it; none if the handshake or that message fails.
async fn accept_next(listener: &TcpListener) -> Option<(Conn, String)> {
    let (tcp, _) = listener.accept().await.ok()?;
    let mut ws = accept_async(tcp).await.ok()?;
    loop {
        match timeout(WAIT, ws.next()).await.ok()??.ok()? {
            Message::Text(text) => return Some((ws, text.as_str().to_owned())),
            Message::Ping(_) => {}
            _ => return None,
        }
    }
}

/// A price as a JSON number, written exactly as its decimal text.
fn number(price: Fixed) -> Value {
    serde_json::from_str(&price.to_string()).expect("a decimal is a JSON number")
}
