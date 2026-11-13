//! Test support: a loopback stand-in for CME Group's token endpoint and market data stream, each
//! behind TLS a test CA certifies. The endpoint grants short-lived bearer tokens to the client
//! credentials it expects, or refuses every request, echoing the credentials it was sent, or gives a
//! canned answer. The stream takes a connection whose handshake carries a token the endpoint issued
//! and that has not expired, answers its subscription, and restates the top of book of each listed
//! contract of a subscribed product every 500 ms, as CME's conflation allows at most, until the
//! token expires, when it ends the session, unless told to keep it. Told to, it sends only a
//! `HEARTBEAT` as often as it is told, as CME does through a closure; answers each login after the
//! first late; or, before answering each subscription after the first, sends a heartbeat and a
//! message the feed refuses and then waits. It refuses any other login, or as many as it is told
//! to, with `LOGINERR`, echoing the token it was sent.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::routing::post;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, SecondsFormat};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::AbortHandle;
use tokio::time::{Instant, interval, sleep, sleep_until, timeout};
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

use crate::clock::now_ms;
use crate::config::ApiKey;
use crate::feeds::cme::globex;
use crate::feeds::tests::fake::{Conn, WAIT, listen};
use crate::feeds::tests::rwa_support::TlsFront;
use crate::feeds::tests::vendors::Prices;
use crate::feeds::venue::OAuthClient;

/// The client ID and secret the stand-in's token endpoint expects.
pub const CLIENT_ID: &str = "cme-api-id-7f3a9c";
pub const CLIENT_SECRET: &str = "cme-api-password-41d8e2b6";
/// How often the stream restates a subscribed contract's top of book: CME's conflation bound.
const EVERY: Duration = Duration::from_millis(500);
/// The periods the stream lists for every product.
const LISTED: [&str; 3] = ["202612", "202703", "202709"];

/// A running stand-in: its endpoints over TLS, what it has seen, and its tasks.
pub struct CmeStandIn {
    /// The token endpoint's `https://` URL.
    pub token_url: String,
    /// The market data stream's `wss://` URL.
    pub stream_url: String,
    pub state: Arc<Mutex<StandIn>>,
    pub tasks: Vec<AbortHandle>,
}

/// What the stand-in answers, and what it has seen.
#[derive(Default)]
pub struct StandIn {
    /// How the token endpoint answers.
    pub answer: Answer,
    /// Each token issued, with when it expires.
    pub issued: Vec<(String, Instant)>,
    /// Each token request: its `Authorization` and `Content-Type` headers and its body.
    pub requests: Vec<(Option<String>, Option<String>, String)>,
    /// The token each authenticated session logged in with, in order, and when each subscribed.
    pub sessions: Vec<String>,
    pub subscribed_at: Vec<Instant>,
    /// How many sessions the stream ended because their token expired.
    pub expired_sessions: usize,
    /// The subscription messages the stream read.
    pub subscriptions: Vec<Value>,
    /// How many logins the stream refuses from now on, whatever their tokens.
    pub refuse_logins: usize,
    /// How long the stream waits before answering each login after the first, or every login.
    pub login_delay: Option<Duration>,
    pub delay_every_login: bool,
    /// How long the stream waits, after a heartbeat and a message the feed refuses, before
    /// answering each subscription after the first.
    pub subscribe_delay: Option<Duration>,
    /// How long the stream reads nothing, so answers no ping, once it has accepted a subscription.
    pub stall_after_subscribing: Option<Duration>,
    /// Whether the stream sends a binary message before it accepts each subscription.
    pub binary_before_subscribing: bool,
    /// Whether the stream keeps a session open, still quoting, once its token expires.
    pub keep_expired: bool,
    /// When the feed ended each session, by the token it logged in with.
    pub ended: Vec<(String, Instant)>,
    /// Sends only a heartbeat this often in place of the tops of book.
    pub heartbeat_every: Option<Duration>,
}

/// How the token endpoint answers.
#[derive(Clone)]
pub enum Answer {
    /// Grants a bearer token living this many seconds.
    Grant(u64),
    /// Refuses with `invalid_client`, echoing the credentials it was sent.
    Refuse,
    /// Answers with this status and body.
    Canned(StatusCode, String),
}

impl Default for Answer {
    /// CME's documented lifetime.
    fn default() -> Self {
        Answer::Grant(1_799)
    }
}

impl CmeStandIn {
    /// Starts the endpoint, granting tokens living `life_secs`, and the stream, quoting `prices` by
    /// `<product>:<period>`, each behind its own TLS front from `tls`.
    pub async fn start(tls: &TlsFront, life_secs: u64, prices: Prices) -> Self {
        let state = Arc::new(Mutex::new(StandIn { answer: Answer::Grant(life_secs), ..StandIn::default() }));
        let (endpoint, endpoint_plain) = listen().await;
        let router = Router::new().route("/as/token.oauth2", post(grant)).with_state(state.clone());
        let endpoint = tokio::spawn(async move { axum::serve(endpoint, router).await.expect("token endpoint") });
        let (stream, stream_plain) = listen().await;
        let stream = tokio::spawn(serve_stream(stream, state.clone(), prices));
        // The fronts relay bytes, so one serves HTTPS as well as WebSockets.
        let (endpoint_url, endpoint_front) = tls.relay(&endpoint_plain).await;
        let (stream_url, stream_front) = tls.relay(&stream_plain).await;
        let token_url = format!("{}/as/token.oauth2", endpoint_url.replacen("wss://", "https://", 1));
        let tasks = vec![endpoint.abort_handle(), stream.abort_handle(), endpoint_front, stream_front];
        CmeStandIn { token_url, stream_url: format!("{stream_url}/marketdatastream/v1"), state, tasks }
    }

    /// A client of the endpoint with the credentials it expects, over `tls`'s client config.
    pub fn client(&self, tls: &TlsFront, timeout: Duration) -> OAuthClient {
        let (id, secret) = (key(CLIENT_ID), key(CLIENT_SECRET));
        OAuthClient::new(&self.token_url, id, secret, &tls.client, timeout).expect("a token client")
    }

    /// Sets how the endpoint answers from now on.
    pub fn answer(&self, answer: Answer) {
        self.state.lock().expect("stand-in").answer = answer;
    }

    /// Reads what the stand-in has seen.
    pub fn seen<T>(&self, read: impl FnOnce(&StandIn) -> T) -> T {
        read(&self.state.lock().expect("stand-in"))
    }

    /// Stops the stand-in.
    pub fn stop(&self) {
        self.tasks.iter().for_each(AbortHandle::abort);
    }
}

/// `value` as a credential.
pub fn key(value: &str) -> ApiKey {
    ApiKey::from_env("CREDENTIAL", |_| Some(value.to_owned())).expect("a credential")
}

/// Answers one token request as the stand-in's `answer` says, granting only the client credentials
/// grant to the expected client.
async fn grant(State(state): State<Arc<Mutex<StandIn>>>, headers: HeaderMap, body: String) -> (StatusCode, String) {
    let header = |name: HeaderName| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let (authorization, content_type) = (header(AUTHORIZATION), header(CONTENT_TYPE));
    let mut state = state.lock().expect("stand-in");
    state.requests.push((authorization.clone(), content_type, body.clone()));
    let expected = format!("Basic {}", STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}")));
    match state.answer.clone() {
        Answer::Canned(status, body) => (status, body),
        Answer::Grant(life)
            if authorization.as_deref() == Some(expected.as_str()) && body == "grant_type=client_credentials" =>
        {
            let token = format!("tok-{}-{}", state.issued.len(), rand::random::<u64>());
            state.issued.push((token.clone(), Instant::now() + Duration::from_secs(life)));
            let answer = json!({ "access_token": token, "token_type": "bearer", "expires_in": life });
            (StatusCode::OK, answer.to_string())
        }
        Answer::Grant(_) | Answer::Refuse => {
            let echoed = format!("Invalid client or client credentials: {CLIENT_ID} {CLIENT_SECRET} {expected}");
            (StatusCode::UNAUTHORIZED, json!({ "error_description": echoed, "error": "invalid_client" }).to_string())
        }
    }
}

/// Serves the stream: each connection as `session` does.
async fn serve_stream(listener: TcpListener, state: Arc<Mutex<StandIn>>, prices: Prices) {
    while let Ok((tcp, _)) = listener.accept().await {
        tokio::spawn(session(tcp, state.clone(), prices.clone()));
    }
}

/// One session: the login the handshake's token makes, the subscription, then a top of book every
/// `EVERY` until the feed hangs up or the token expires.
async fn session(tcp: TcpStream, state: Arc<Mutex<StandIn>>, prices: Prices) {
    let mut bearer = None;
    // tungstenite fixes the callback's error type.
    #[allow(clippy::result_large_err)]
    let read_bearer = |request: &Request, response: Response| {
        bearer = request.headers().get(AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::to_owned);
        Ok(response)
    };
    let Ok(mut ws) = accept_hdr_async(tcp, read_bearer).await else { return };
    let token = bearer.and_then(|bearer| bearer.strip_prefix("Bearer ").map(str::to_owned)).unwrap_or_default();
    let (expires, delay) = {
        let mut state = state.lock().expect("stand-in");
        let valid = state.issued.iter().find(|(issued, at)| *issued == token && *at > Instant::now());
        let mut expires = valid.map(|(_, at)| *at);
        if expires.is_some() && state.refuse_logins > 0 {
            state.refuse_logins -= 1;
            expires = None;
        }
        (expires, state.login_delay.filter(|_| state.delay_every_login || !state.sessions.is_empty()))
    };
    if let Some(delay) = delay {
        sleep(delay).await;
    }
    let Some(expires) = expires else {
        let refused = json!({ "header": header("LOGINERR"), "payload": { "token": token } });
        let _ = ws.send(Message::text(refused.to_string())).await;
        return;
    };
    let login = json!({ "header": header("LOGIN_STATUS"), "payload": { "status": "AUTHENTICATED" } });
    if ws.send(Message::text(login.to_string())).await.is_err() {
        return;
    }
    let Some(subscription) = next_json(&mut ws).await else { return };
    let symbols: Vec<(String, String)> = subscription["payload"]["subscriptions"]
        .as_array()
        .expect("subscriptions")
        .iter()
        .flat_map(|entry| {
            let product = entry["productCode"].as_str().expect("a product").to_owned();
            LISTED.map(|period| (product.clone(), period.to_owned()))
        })
        .collect();
    let (later, subscribe_delay, keep_expired, binary) = {
        let mut state = state.lock().expect("stand-in");
        let later = !state.sessions.is_empty();
        state.sessions.push(token.clone());
        state.subscribed_at.push(Instant::now());
        state.subscriptions.push(subscription);
        (later, state.subscribe_delay, state.keep_expired, state.binary_before_subscribing)
    };
    if binary && ws.send(Message::binary(vec![0, 1, 2])).await.is_err() {
        return;
    }
    if let Some(delay) = subscribe_delay.filter(|_| later) {
        let refused = json!({ "header": header("TRD"), "payload": [] });
        for message in [json!({ "header": header("HEARTBEAT") }), refused] {
            if ws.send(Message::text(message.to_string())).await.is_err() {
                return;
            }
        }
        sleep(delay).await;
    }
    let subscribed = json!({ "header": header("SUBSCRIPTION_STATUS"), "payload": { "status": "SUBSCRIBED" } });
    if ws.send(Message::text(subscribed.to_string())).await.is_err() {
        return;
    }
    let (heartbeats, stall) = {
        let state = state.lock().expect("stand-in");
        (state.heartbeat_every, state.stall_after_subscribing)
    };
    if let Some(stall) = stall {
        sleep(stall).await;
    }
    let mut every = interval(heartbeats.unwrap_or(EVERY));
    let mut expiry = Box::pin(sleep_until(expires));
    loop {
        tokio::select! {
            // Reading answers the feed's pings.
            read = ws.next() => if !matches!(read, Some(Ok(_))) {
                state.lock().expect("stand-in").ended.push((token, Instant::now()));
                return;
            },
            () = &mut expiry, if !keep_expired => {
                state.lock().expect("stand-in").expired_sessions += 1;
                let _ = ws.close(None).await;
                return;
            }
            _ = every.tick() => {
                let message = match heartbeats {
                    Some(_) => json!({ "header": header("HEARTBEAT") }).to_string(),
                    None => top_of_book(&symbols, &prices),
                };
                if ws.send(Message::text(message)).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// A message header of `kind`, sent now.
fn header(kind: &str) -> Value {
    let sent = DateTime::from_timestamp_millis(now_ms()).expect("in range").to_rfc3339_opts(SecondsFormat::Nanos, true);
    json!({ "messageType": kind, "sentTime": sent, "sequenceNumber": "1", "version": "1.0" })
}

/// A `TOB` message restating each of `symbols`' books that `prices` quotes now, one level a side.
fn top_of_book(symbols: &[(String, String)], prices: &Prices) -> String {
    let ts = now_ms();
    let tops: Vec<Value> = symbols
        .iter()
        .filter_map(|(product, period)| {
            let (bid, ask) = prices(&format!("{product}:{period}"), ts)?;
            let level = |price: String| json!([{ "lastUpdateTime": "2026-11-13T15:04:04.000000000Z", "orderCnt": "3", "price": price, "qty": "5" }]);
            let instrument = json!({ "definitionSource": "E", "exchangeMic": "XCME", "id": "118", "marketSegmentId": "72",
                "periodCode": period, "productCode": product, "productGroup": product, "productType": "FUT", "symbol": globex(product, period, 1) });
            Some(json!({ "instrument": instrument, "bidLevel": level(bid.to_string()), "askLevel": level(ask.to_string()) }))
        })
        .collect();
    json!({ "header": header("TOB"), "payload": tops }).to_string()
}

/// The next text message the feed sends, as JSON; none if the connection ends first.
async fn next_json(ws: &mut Conn) -> Option<Value> {
    loop {
        match timeout(WAIT, ws.next()).await.ok()??.ok()? {
            Message::Text(text) => return serde_json::from_str(text.as_str()).ok(),
            Message::Ping(_) | Message::Pong(_) => {}
            _ => return None,
        }
    }
}
