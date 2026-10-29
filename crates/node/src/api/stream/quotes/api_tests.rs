//! Signed quotes through the API against a real sequencer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use engine_core::Fixed;
use engine_core::command::{Command, OrderKind, OrderRequest};
use engine_core::event::{Event, Reject};
use engine_core::price::SourceQuote;
use engine_core::types::Side;
use engine_wire::{Connection, Deployment, FrameVerifier, SignedFrame};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use super::tests::TRADER;
use super::*;
use crate::api::signing_tests::{parse, state};
use crate::api::{Signing, router};
use crate::clock::test_clock::TestClock;
use crate::tee::EnclaveKey;
use crate::test_support::{TestSequencer, deposit, dev_config, scratch_dir, trusted};

const BTC: MarketId = 1;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CHALLENGE: &str = "8f0e5c2a9b7d41e6a3c0f19d2e84b7c65a1d0e3f9b2c7a48e6d1f05b3a9c2e71";
const QUERY: &str = "signed=1&challenge=8f0e5c2a9b7d41e6a3c0f19d2e84b7c65a1d0e3f9b2c7a48e6d1f05b3a9c2e71";

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_connection_streams_verified_quotes_that_fill_at_their_price_until_the_index_moves() {
    let dir = scratch_dir("quotes-api");
    let node = TestSequencer::start_legacy(&dir, 1024);
    node.submit(deposit(50_000_000, "dep")).await;
    node.submit(live_prices(100_000)).await;
    let cfg = dev_config();
    let mut app = state(&node, &dir, &cfg.api);
    let vault = cfg.custody.vault.parse().unwrap();
    // No heartbeat gets in the way of the quotes.
    app.signing = Arc::new(Signing::new(
        Arc::new(EnclaveKey::test_key()),
        cfg.custody.chain_id,
        vault,
        Duration::from_secs(3600),
        trusted(Arc::new(TestClock::default())),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = router(app).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await });

    let (mut ws, _) = connect_async(format!("ws://{addr}/v1/stream?{QUERY}")).await.unwrap();
    let deployment = Deployment { chain_id: cfg.custody.chain_id, vault: vault.0 };
    let connection = Connection::new(parse(CHALLENGE), &format!("/v1/stream?{QUERY}"));
    let mut verifier = FrameVerifier::new(EnclaveKey::test_key().address(), deployment, connection);
    ws.send(WsMessage::text(format!(r#"{{"op":"quotes","account":"{TRADER}","markets":[1]}}"#))).await.unwrap();

    // The first message is a verified signed frame holding the market's quote.
    let quote = next_quote(&mut ws, &mut verifier).await;
    let ask = &quote["ask"][0];
    let (qty, price): (Fixed, Fixed) =
        (ask["qty"].as_str().unwrap().parse().unwrap(), ask["price"].as_str().unwrap().parse().unwrap());
    assert_eq!((quote["market"].as_u64(), quote["open"].as_bool()), (Some(1), Some(true)));
    // Quiet while nothing changes.
    assert!(timeout(Duration::from_millis(500), next_quote(&mut ws, &mut verifier)).await.is_err());

    // An order of the step's whole size at its price with no slippage allowed fills at or below it.
    let events = node.submit(order(Side::Buy, qty, price, 0)).await;
    let fill = events.iter().find_map(|event| if let Event::Fill(fill) = event { Some(fill) } else { None });
    assert!(fill.expect("the order fills").price <= price, "{events:?}");
    // The position moved, so the quotes are sent again.
    let after = next_quote(&mut ws, &mut verifier).await;
    assert_ne!(after["ask"], quote["ask"]);
    assert!(after["close"]["price"].is_string());

    // The index moves: the new quote arrives, and the old price with no slippage allowed is declined.
    node.submit(live_prices(101_000)).await;
    let moved = next_quote(&mut ws, &mut verifier).await;
    assert_eq!(moved["mark"], "101000");
    let events = node.submit(order(Side::Buy, Fixed::ONE, price, 0)).await;
    assert!(
        events.iter().any(|event| matches!(event, Event::OrderRejected { reason: Reject::Slippage { .. }, .. })),
        "{events:?}"
    );
    let ask = &moved["ask"][0];
    let (qty, price) = (ask["qty"].as_str().unwrap().parse().unwrap(), ask["price"].as_str().unwrap().parse().unwrap());
    let events = node.submit(order(Side::Buy, qty, price, 0)).await;
    assert!(events.iter().any(|event| matches!(event, Event::Fill(_))), "{events:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsigned_connection_cannot_subscribe_to_quotes() {
    let dir = scratch_dir("quotes-unsigned");
    let node = TestSequencer::start_legacy(&dir, 1024);
    node.submit(deposit(1_000_000, "dep")).await;
    node.submit(live_prices(100_000)).await;
    let cfg = dev_config();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let service = router(state(&node, &dir, &cfg.api)).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, service).await });
    let (mut ws, _) = connect_async(format!("ws://{addr}/v1/stream")).await.unwrap();
    ws.send(WsMessage::text(format!(r#"{{"op":"quotes","account":"{TRADER}","markets":[1]}}"#))).await.unwrap();
    node.submit(live_prices(101_000)).await;
    // Only the public log's messages come back.
    while let Ok(Some(Ok(WsMessage::Text(text)))) = timeout(Duration::from_millis(500), ws.next()).await {
        assert!(!text.contains(r#""type":"quote""#), "{text}");
    }
}

/// The next quote message in a frame that verifies, skipping other messages.
async fn next_quote(ws: &mut Ws, verifier: &mut FrameVerifier) -> serde_json::Value {
    loop {
        let WsMessage::Text(text) =
            timeout(Duration::from_secs(10), ws.next()).await.expect("a frame").unwrap().unwrap()
        else {
            continue;
        };
        let frame: SignedFrame = serde_json::from_str(&text).unwrap();
        assert_eq!(verifier.verify(&frame), Ok(()), "frame {}", frame.n);
        let msg: serde_json::Value = serde_json::from_str(&frame.msg).unwrap();
        if msg["type"] == "quote" {
            return msg;
        }
        assert_ne!(msg["type"], "quote_error", "{msg}");
    }
}

/// Two venues at `mid`, stamped now.
fn live_prices(mid: i64) -> Command {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64;
    let quotes = ["a", "b"]
        .map(|source| SourceQuote { source: source.into(), mid: Fixed::from_int(mid), ts: now, contract: None })
        .to_vec();
    Command::Prices { market: BTC, quotes }
}

fn order(side: Side, qty: Fixed, quoted: Fixed, max_slippage_bps: i64) -> Command {
    Command::PlaceOrder(OrderRequest {
        account: TRADER,
        client_id: 1,
        market: BTC,
        side,
        qty,
        kind: OrderKind::Market { quoted, max_slippage_bps: Some(max_slippage_bps) },
        reduce_only: false,
        auth: None,
    })
}
