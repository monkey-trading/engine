//! The daily turns scenario's load and its stream client. The load prices BTC-USD, rests an order
//! and cancels it, each request signed with its funded account's next nonce, on whichever node holds
//! the turn, so every switch's window is measured under sequencing, and counts what was done. The stream client follows the operator in effect from its
//! registered hostname, resuming on each successor from its own copy's seq and verifying every frame
//! against the vault's signer.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, U256};
use axum::Extension;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::Request as HttpRequest;
use engine_core::Engine;
use engine_core::auth::RequestAuth;
use engine_core::command::{CancelRequest, Command, OrderKind, OrderRequest};
use engine_core::eip712;
use engine_core::event::Event;
use engine_core::price::SourceQuote;
use engine_core::types::{AccountId, Side};
use k256::ecdsa::SigningKey;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tower::ServiceExt;

use super::super::chain::Chain;
use super::super::node::Node;
use crate::api::{ApiState, router};
use crate::checkpoints::{CheckpointChain, View};
use crate::clock::Clock;
use crate::clock::test_clock::TestClock;
use crate::config::NodeConfig;
use crate::log::CheckpointDomain;
use crate::sequencer::Request;
use crate::standby::Follower;

/// What the load got done: the orders the holding node accepted and the ones it then cancelled.
#[derive(Default)]
pub(super) struct Load {
    pub(super) placed: AtomicU64,
    pub(super) cancelled: AtomicU64,
    /// What the node answered the last order it did not accept, for a scenario that waits on it.
    pub(super) refused: Mutex<Option<String>>,
}

/// The load's key, whose account the scenario declares and funds before the load runs.
pub(super) fn key() -> SigningKey {
    SigningKey::from_slice(&[0x79; 32]).unwrap()
}

/// The load's account.
pub(super) fn account() -> AccountId {
    eip712::address(key().verifying_key())
}

/// Declares the load's account on `node` and funds it through `chain`'s vault, waiting for the
/// credit: a credit before finality is collateral already, which the load trades on.
pub(super) async fn fund(chain: &Chain, node: &Node) {
    let trader = account();
    super::super::operator_share::declare(node, &key()).await;
    chain.fund(Some(Address::from(trader.0)), U256::from(100_000_000_000u64)).await;
    chain.finalize().await;
    node.wait("the load's deposit to credit", move |e| {
        e.accounts().get(&trader).is_some_and(|account| account.balance > engine_core::Fixed::ZERO)
    })
    .await;
}

/// Prices BTC-USD at 100,000, then rests a BTC-USD order on the holding node's API away from that
/// price and cancels it by its id, every few milliseconds, each request signed for the vault `vault`
/// with the account's next nonce and screened as the API screens it, until aborted, counting what
/// the node accepted and cancelled.
pub(super) fn generate(
    holding: watch::Receiver<Option<ApiState>>,
    vault: AccountId,
    clock: Arc<TestClock>,
) -> (Arc<Load>, JoinHandle<()>) {
    let load = Arc::new(Load::default());
    let counted = load.clone();
    let task = tokio::spawn(async move {
        let (key, account) = (key(), account());
        let domain = eip712::domain(31_337, vault);
        for client_id in 1..=u64::MAX {
            tokio::time::sleep(Duration::from_millis(5)).await;
            let Some(mut api) = holding.borrow().clone() else { continue };
            // The API judges expiries on the scenario's clock, which the node's own copy trails.
            api.clock = clock.clone();
            let tx = api.tx.clone();
            let now = clock.now_ms();
            let quote = |source: &str| SourceQuote {
                source: source.into(),
                mid: "100000".parse().unwrap(),
                ts: now,
                contract: None,
            };
            submit(&tx, Command::Prices { market: 1, quotes: vec![quote("a"), quote("b")] }).await;
            let expires_at = now + 60_000;
            let Some(nonce) = next_nonce(&tx, account).await else { continue };
            let (side, price) = if client_id % 2 == 0 { (Side::Buy, "99000") } else { (Side::Sell, "101000") };
            let mut order = OrderRequest {
                account,
                client_id,
                market: 1,
                side,
                qty: "0.001".parse().unwrap(),
                kind: OrderKind::Limit { price: price.parse().unwrap() },
                reduce_only: false,
                auth: None,
            };
            let signature = eip712::sign(&key, &eip712::order_hash(&domain, &order, nonce, expires_at));
            order.auth = Some(RequestAuth { nonce, expires_at, signature });
            let events = post(&api, "/v1/orders", &order).await;
            let accepted = events.iter().find_map(|event| match event {
                Event::OrderAccepted { order_id, .. } => Some(*order_id),
                _ => None,
            });
            let Some(order_id) = accepted else {
                *counted.refused.lock().unwrap() = Some(format!("{events:?}"));
                continue;
            };
            counted.placed.fetch_add(1, Ordering::Relaxed);
            let Some(nonce) = next_nonce(&tx, account).await else { continue };
            let signature = eip712::sign(&key, &eip712::cancel_hash(&domain, account, order_id, nonce, expires_at));
            let auth = Some(RequestAuth { nonce, expires_at, signature });
            let cancelled = post(&api, "/v1/orders/cancel", &CancelRequest { account, order_id, auth }).await;
            if cancelled.iter().any(|event| matches!(event, Event::OrderCancelled { .. })) {
                counted.cancelled.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    (load, task)
}

/// The events the API of the node behind `api` answers a POST of `body` to `path` with; none when it
/// refuses or does not answer.
async fn post(api: &ApiState, path: &str, body: &impl serde::Serialize) -> Vec<Event> {
    let request = HttpRequest::post(path)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let app = router(api.clone()).layer(Extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 9)))));
    let Ok(response) = app.oneshot(request).await;
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap_or_default();
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// `account`'s next request nonce on the node behind `tx`, if it answers.
async fn next_nonce(tx: &mpsc::Sender<Request>, account: AccountId) -> Option<u64> {
    let (reply, answered) = oneshot::channel();
    let query = Box::new(move |engine: &Engine| {
        let _ = reply.send(engine.next_request_nonce(account));
    });
    tx.send(Request::Query(query)).await.ok()?;
    answered.await.ok()
}

/// Sequences `cmd` on the node behind `tx`: its events, or none if the node does not answer.
async fn submit(tx: &mpsc::Sender<Request>, cmd: Command) -> Vec<Event> {
    let (reply, applied) = oneshot::channel();
    if tx.send(Request::Submit { cmd, reply: Some(reply) }).await.is_err() {
        return Vec::new();
    }
    applied.await.unwrap_or_default()
}

/// A stream client: a copy of the log in `cfg`'s log directory, bootstrapped from the operator in
/// effect's archive, following whichever operator the registry names at `https://` its hostname,
/// every frame verified against the vault's signer. Each operator it follows,
/// in order, is recorded with the enclave its frames verify against, and its copy's last seq.
pub(super) struct Client {
    pub(super) followed: Arc<Mutex<Vec<(String, AccountId)>>>,
    pub(super) seq: Arc<AtomicU64>,
    task: JoinHandle<()>,
}

impl Client {
    /// Starts the client under `cfg`, which must outlive it.
    pub(super) fn start(cfg: &'static NodeConfig) -> Self {
        let (followed, seq) = (Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicU64::new(0)));
        let (recorded, reached) = (followed.clone(), seq.clone());
        let task = tokio::task::spawn_local(async move {
            let vault = crate::handover::vault_reader(cfg).unwrap().expect("the client reads the vault");
            let operator = crate::standby::resolve(cfg, &vault).await.unwrap();
            let client = crate::standby::archive::archive_client(true).unwrap();
            crate::standby::archive::bootstrap_archive(&client, &operator.origins(), &cfg.node.log_dir).await.unwrap();
            let at = vault.observe(View::Finalized, &[]).await.unwrap();
            let domain = CheckpointDomain { chain_id: 31_337, vault: cfg.custody.vault.parse().unwrap() };
            let signers = [at.state.signer].into();
            let segment_commands = cfg.node.segment_commands;
            let mut copy =
                Follower::open(&cfg.node.log_dir, segment_commands, domain, at.state.signer, signers, None, |_| {})
                    .unwrap();
            loop {
                let Ok(operator) = crate::standby::resolve(cfg, &vault).await else {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                };
                {
                    let mut followed = recorded.lock().unwrap();
                    if followed.last().is_none_or(|(origin, _)| *origin != operator.origin) {
                        followed.push((operator.origin.clone(), operator.enclave));
                    }
                }
                if copy.stint(cfg, &operator, Duration::from_secs(2)).await.is_err() {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                reached.store(copy.replica().last_seq().unwrap_or(0), Ordering::Relaxed);
            }
        });
        Self { followed, seq, task }
    }

    /// Stops the client.
    pub(super) fn stop(&self) {
        self.task.abort();
    }
}
