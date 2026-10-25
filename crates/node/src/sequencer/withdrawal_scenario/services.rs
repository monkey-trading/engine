//! Deterministic local quotes and signed Roughtime replies, with explicit task lifetimes.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Json;
use axum::http::StatusCode;
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use engine_core::Fixed;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{RwLock, watch};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{interval, timeout};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

use crate::clock::now_ms;
use crate::roughtime::VERSION_1;
use crate::roughtime::testing::TestServer;

pub(super) struct Services {
    pub feed: String,
    pub time: Vec<Value>,
    pub rpcs: Vec<String>,
    price: watch::Sender<Fixed>,
    tasks: Vec<JoinHandle<anyhow::Result<()>>>,
}

impl Drop for Services {
    /// Stops local servers and the feed server's owned connection tasks.
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Services {
    /// Starts two independently signed Roughtime servers, one controllable venue, and three RPC
    /// proxies whose requests never overlap the scenario's block production.
    /// Error cases:
    /// - loopback sockets cannot bind
    pub async fn start(rpc: &str, mining: Arc<RwLock<()>>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let feed = format!("ws://{}", listener.local_addr()?);
        let (price, receiver) = watch::channel(Fixed::from_int(100));
        let mut tasks = vec![tokio::spawn(venue(listener, receiver))];
        let mut time = Vec::new();
        let mut rpcs = Vec::new();
        for _ in 0..3 {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            rpcs.push(format!("http://{}", listener.local_addr()?));
            let target = rpc.to_owned();
            let client = reqwest::Client::builder().timeout(Duration::from_secs(5)).build()?;
            let mining = mining.clone();
            let app = axum::Router::new().route(
                "/",
                post(move |Json(body): Json<Value>| {
                    let (client, target, mining) = (client.clone(), target.clone(), mining.clone());
                    async move {
                        let _mining = mining.read().await;
                        let response =
                            client.post(target).json(&body).send().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
                        let json = response.json::<Value>().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
                        Ok::<_, StatusCode>(Json(json))
                    }
                }),
            );
            tasks.push(tokio::spawn(async move { Ok(axum::serve(listener, app).await?) }));
        }
        for seed in [51, 52] {
            let socket = UdpSocket::bind("127.0.0.1:0").await?;
            let server = TestServer::new(seed, VERSION_1);
            time.push(json!({
                "name":format!("local-{seed}"), "address":socket.local_addr()?.to_string(),
                "public_key":STANDARD.encode(server.public_key().as_bytes())
            }));
            tasks.push(tokio::spawn(roughtime(socket, server)));
        }
        Ok(Self { feed, time, rpcs, price, tasks })
    }

    /// Changes the quoted mid; the scenario waits for the actual API mark before trading.
    pub fn set_price(&self, price: i64) {
        self.price.send_replace(Fixed::from_int(price));
    }

    /// Fails if a required local server exited; silent background failures cannot look healthy.
    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.tasks.iter().all(|task| !task.is_finished()), "a scenario server stopped");
        Ok(())
    }
}

/// Answers nonce-bound signed packets against the same system clock the real node checks.
/// Error cases:
/// - the loopback UDP socket fails
async fn roughtime(socket: UdpSocket, server: TestServer) -> anyhow::Result<()> {
    let mut packet = [0; 2048];
    loop {
        let (len, peer) = socket.recv_from(&mut packet).await?;
        let reply = server.reply(&packet[..len], u64::try_from(now_ms() / 1000)?, 1);
        socket.send_to(&reply, peer).await?;
    }
}

/// Accepts reconnecting clients and owns every spawned connection in a JoinSet.
/// Error cases:
/// - the listener or a connection task fails
async fn venue(listener: TcpListener, price: watch::Receiver<Fixed>) -> anyhow::Result<()> {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                clients.spawn(quote(socket, price.clone()));
            }
            result = clients.join_next(), if !clients.is_empty() => {
                // Node restarts close their feed socket; reconnecting is part of this scenario.
                result.context("feed connection task exists")?.context("feed task panicked")??;
            }
        }
    }
}

/// Publishes exact mids using ordinary Bitstamp book messages with noncrossed prices.
/// Error cases:
/// - the handshake fails or the client never subscribes
async fn quote(socket: TcpStream, price: watch::Receiver<Fixed>) -> anyhow::Result<()> {
    let mut ws = timeout(Duration::from_secs(5), accept_async(socket)).await??;
    timeout(Duration::from_secs(5), ws.next()).await?.context("venue client closed before subscribing")??;
    let mut tick = interval(Duration::from_millis(80));
    loop {
        tokio::select! {
            message = ws.next() => match message {
                Some(Ok(Message::Ping(data))) => if ws.send(Message::Pong(data)).await.is_err() { return Ok(()) },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                _ => {}
            },
            _ = tick.tick() => {
                let mid = *price.borrow();
                let step = Fixed::from_raw(10_000_000);
                let text = json!({"event":"data","channel":"order_book_btcusd","data":{
                    "microtimestamp":(now_ms()*1000).to_string(),
                    "bids":[[(mid-step).to_string(),"100"]],"asks":[[(mid+step).to_string(),"100"]]
                }}).to_string();
                if ws.send(Message::Text(text.into())).await.is_err() { return Ok(()) }
            }
        }
    }
}
