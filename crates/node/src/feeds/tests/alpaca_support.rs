//! Test support: a loopback stand-in for Alpaca's consolidated stock stream, behind TLS a test CA
//! certifies, speaking the messages Alpaca documents. It welcomes each connection, accepts the login
//! it expects, or answers error 402 with text echoing the key ID and secret key it was sent and ends
//! the connection, confirms the quote subscription, and then streams each subscribed symbol's quote
//! every 20 ms, stamped to the nanosecond, as a shared price function sets it. Stopped, it ends each
//! connection it serves and drops each new one until it is started again. It accepts connection after
//! connection, so a feed's reconnects find it again.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::AbortHandle;
use tokio::time::{interval, timeout};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use crate::clock::now_ms;
use crate::feeds::tests::cme_support::key;
use crate::feeds::tests::fake::{Conn, WAIT, listen};
use crate::feeds::tests::rwa_support::TlsFront;
use crate::feeds::tests::vendors::{Prices, number};
use crate::feeds::venue::Credentials;

/// The key ID and secret key the stand-in expects.
pub const KEY_ID: &str = "AKSTANDIN7Q2M4X9W1";
pub const SECRET_KEY: &str = "alpaca-stand-in-secret-5f1c0e9a2b7d";
/// The start of the text the stand-in's refusal writes before echoing what it was sent.
pub const REFUSAL_TEXT: &str = "no account holds";
/// How often the stand-in sends each subscribed symbol's quote.
const EVERY: Duration = Duration::from_millis(20);

/// A running stand-in: its feed's URL over TLS, its switches, and its tasks.
pub struct AlpacaStandIn {
    /// The `wss://` URL of its consolidated feed.
    pub url: String,
    state: Arc<State>,
    tasks: Vec<AbortHandle>,
}

/// The stand-in's switches, and the logins it accepted.
#[derive(Default)]
struct State {
    stopped: AtomicBool,
    refuse_logins: AtomicBool,
    logins: AtomicUsize,
}

impl AlpacaStandIn {
    /// Starts the stand-in, quoting `prices`, behind its own TLS front from `tls`.
    pub async fn start(tls: &TlsFront, prices: Prices) -> Self {
        let state = Arc::new(State::default());
        let (listener, plain) = listen().await;
        let server = tokio::spawn(serve(listener, state.clone(), prices));
        let (url, front) = tls.relay(&plain).await;
        AlpacaStandIn { url: format!("{url}/v2/sip"), state, tasks: vec![server.abort_handle(), front] }
    }

    /// The credentials the stand-in expects.
    pub fn credentials(&self) -> Credentials {
        Credentials::KeyPair { key_id: key(KEY_ID), secret_key: key(SECRET_KEY) }
    }

    /// Ends every connection and drops each new one while `stopped`; serves again once not.
    pub fn set_stopped(&self, stopped: bool) {
        self.state.stopped.store(stopped, Ordering::SeqCst);
    }

    /// Refuses every login from now on.
    pub fn refuse_logins(&self) {
        self.state.refuse_logins.store(true, Ordering::SeqCst);
    }

    /// How many logins the stand-in has accepted.
    pub fn logins(&self) -> usize {
        self.state.logins.load(Ordering::SeqCst)
    }

    /// Stops the stand-in's listener and front.
    pub fn abort(&self) {
        self.tasks.iter().for_each(AbortHandle::abort);
    }
}

/// Accepts connection after connection, serving each on its own task while the stand-in runs.
async fn serve(listener: TcpListener, state: Arc<State>, prices: Prices) {
    while let Ok((tcp, _)) = listener.accept().await {
        if state.stopped.load(Ordering::SeqCst) {
            continue;
        }
        let (state, prices) = (state.clone(), prices.clone());
        tokio::spawn(async move {
            let Ok(ws) = accept_async(tcp).await else { return };
            session(ws, &state, &prices).await;
        });
    }
}

/// One connection: the welcome, the login, the subscription's confirmation, then the quotes, until
/// the feed hangs up, the login is refused, or the stand-in stops.
async fn session(mut ws: Conn, state: &State, prices: &Prices) -> Option<()> {
    send(&mut ws, json!([{ "T": "success", "msg": "connected" }])).await?;
    let login: Value = serde_json::from_str(&next_text(&mut ws).await?).ok()?;
    let expected = json!({ "action": "auth", "key": KEY_ID, "secret": SECRET_KEY });
    if state.refuse_logins.load(Ordering::SeqCst) || login != expected {
        let (key, secret) = (login["key"].as_str().unwrap_or_default(), login["secret"].as_str().unwrap_or_default());
        let msg = format!("{REFUSAL_TEXT} {key} with {secret}");
        send(&mut ws, json!([{ "T": "error", "code": 402, "msg": msg }])).await?;
        return None;
    }
    state.logins.fetch_add(1, Ordering::SeqCst);
    send(&mut ws, json!([{ "T": "success", "msg": "authenticated" }])).await?;
    let subscribe: Value = serde_json::from_str(&next_text(&mut ws).await?).ok()?;
    let symbols: Vec<String> =
        subscribe["quotes"].as_array()?.iter().filter_map(|symbol| symbol.as_str().map(str::to_owned)).collect();
    let empty: [&str; 0] = [];
    let confirmation = json!([{ "T": "subscription", "trades": empty, "quotes": symbols, "bars": empty,
        "updatedBars": empty, "dailyBars": empty, "statuses": empty, "lulds": empty, "corrections": empty,
        "cancelErrors": empty }]);
    send(&mut ws, confirmation).await?;
    let mut every = interval(EVERY);
    loop {
        tokio::select! {
            // Reading answers the feed's pings.
            read = ws.next() => {
                if !matches!(read, Some(Ok(_))) {
                    return None;
                }
            }
            _ = every.tick() => {
                if state.stopped.load(Ordering::SeqCst) {
                    return None;
                }
                let ts = now_ms();
                let at = DateTime::from_timestamp_millis(ts).expect("in range").to_rfc3339_opts(SecondsFormat::Nanos, true);
                let quotes: Vec<Value> = symbols
                    .iter()
                    .filter_map(|symbol| {
                        let (bid, ask) = prices(symbol, ts)?;
                        Some(json!({ "T": "q", "S": symbol, "bx": "V", "bp": number(bid), "bs": 3, "ax": "Q",
                            "ap": number(ask), "as": 2, "t": at, "c": ["R"], "z": "C" }))
                    })
                    .collect();
                if !quotes.is_empty() {
                    send(&mut ws, Value::Array(quotes)).await?;
                }
            }
        }
    }
}

/// Sends `message` as text; none if the connection has gone.
async fn send(ws: &mut Conn, message: Value) -> Option<()> {
    ws.send(Message::text(message.to_string())).await.ok()
}

/// The feed's next text message, skipping its pings; none if it hangs up or sends nothing in time.
async fn next_text(ws: &mut Conn) -> Option<String> {
    loop {
        match timeout(WAIT, ws.next()).await.ok()??.ok()? {
            Message::Text(text) => return Some(text.as_str().to_owned()),
            Message::Ping(_) => {}
            _ => return None,
        }
    }
}
