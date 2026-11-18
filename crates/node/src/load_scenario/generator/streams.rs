//! The stream subscribers: WebSocket connections to `/v1/stream`, each from its own client address,
//! some asking for signed frames and some of those for executable quotes. Each measures a command
//! frame's arrival after the command's stamp, and opens its connection again if the node closes it.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use engine_core::types::{AccountId, MarketId};
use futures_util::{SinkExt, StreamExt};
use rustls::ClientConfig;
use serde_json::json;
use tokio::time::sleep;
use tokio_tungstenite::client_async;
use tokio_tungstenite::tungstenite::Message;

use crate::clock::now_ms;
use crate::load_scenario::generator::client::connect;
use crate::load_scenario::generator::record::{Recorder, stamp_of};

/// What one subscriber asks for.
#[derive(Clone, Debug)]
pub(super) struct Subscription {
    /// Signed frames, under this challenge.
    pub signed: Option<[u8; 32]>,
    /// Executable quotes for this account in these markets, on a signed connection.
    pub quotes: Option<(AccountId, Vec<MarketId>)>,
}

impl Subscription {
    /// The request that opens the connection.
    fn path(&self) -> String {
        match self.signed {
            Some(challenge) => format!("wss://engine/v1/stream?signed=1&challenge={}", hex::encode(challenge)),
            None => "wss://engine/v1/stream".to_owned(),
        }
    }
}

/// Subscribes from `address` as `subscription` asks until the task is dropped, recording into
/// `recorder`. A connection that ends, or never opens, counts as a disconnect, and is opened again.
pub(super) async fn subscribe(
    api: SocketAddr,
    address: IpAddr,
    tls: Arc<ClientConfig>,
    subscription: Subscription,
    recorder: Recorder,
) {
    loop {
        follow(api, address, &tls, &subscription, &recorder).await;
        recorder.lock().expect("a recorder").disconnects += 1;
        sleep(Duration::from_millis(100)).await;
    }
}

/// One connection, until it ends.
async fn follow(
    api: SocketAddr,
    address: IpAddr,
    tls: &Arc<ClientConfig>,
    subscription: &Subscription,
    recorder: &Recorder,
) {
    let Ok(stream) = connect(api, address, tls).await else { return };
    let Ok((mut ws, _)) = client_async(subscription.path(), stream).await else { return };
    if let Some((account, markets)) = &subscription.quotes {
        let op = json!({ "op": "quotes", "account": account, "markets": markets });
        if ws.send(Message::text(op.to_string())).await.is_err() {
            return;
        }
    }
    while let Some(Ok(message)) = ws.next().await {
        let Message::Text(text) = message else { continue };
        let lag = stamp_of(text.as_str()).map(|stamp| u64::try_from(now_ms() - stamp).unwrap_or(0));
        let mut samples = recorder.lock().expect("a recorder");
        match lag {
            Some(lag) => {
                samples.frames += 1;
                samples.stream_lag_ms.push(lag);
            }
            None if text.contains(r#""type":"quote""#) || text.contains(r#"\"type\":\"quote\""#) => {
                samples.quote_frames += 1;
            }
            None => {}
        }
    }
}
