//! The LogHeads chain access's tests: the pending bound, the rebroadcast's exact bytes, the
//! replacement record's consistency, the call durable before its broadcast, the post's send, wait,
//! resend, and rebroadcast branches against a loopback endpoint, and the bounded store IO.

use std::path::PathBuf;
use std::sync::Mutex;

use alloy::primitives::{Address, B256};
use axum::extract::State;
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};

use super::*;

#[test]
fn a_pending_send_waits_out_an_in_memory_bound_only() {
    let bound = Duration::from_secs(120);
    // A reservation this process never sent — persisted or planted — reconciles immediately,
    // however innocent the file's timestamp looks.
    assert!(pending_due(None, bound));
    // A send this process made waits out the bound from then, on the monotonic clock.
    let sent_at = std::time::Instant::now();
    assert!(!pending_due(Some(sent_at), bound));
    let elapsed = std::time::Instant::now() - Duration::from_secs(121);
    assert!(pending_due(Some(elapsed), bound));
}

#[test]
fn a_reconciliation_rebroadcasts_the_reserved_heads_bytes() {
    // A pending reserved head and a newer released head (seq 9): the rebroadcast carries the
    // reserved head's call at the reserved nonce — never the newer head at the old nonce.
    let dir = crate::test_support::scratch_dir("log-head-rebroadcast");
    std::fs::create_dir_all(&dir).unwrap();
    let store =
        super::super::submission::SubmissionStore::pending(&dir, Address::from([2; 20]), Address::from([1; 20]), 31337)
            .unwrap();
    let reserved = Head { seq: 3, link: B256::from([3; 32]) };
    let signature = format!("0x{}", hex::encode([7u8; 65]));
    let mut reservation = match store.reserve(9, 1, reserved, 1_000).unwrap() {
        super::super::submission::Reserve::Nonce(reservation) => reservation,
        other => panic!("a fresh store reserves a nonce: {other:?}"),
    };
    reservation.nonce = 9;
    reservation.tx_hash = Some(B256::from([8; 32]));
    reservation.signature = Some(signature.clone());
    store.update(&reservation).unwrap();
    let loaded = match store.reserve(9, 1, Head { seq: 9, link: B256::from([9; 32]) }, 2_000).unwrap() {
        super::super::submission::Reserve::Pending(pending) => pending,
        other => panic!("the stored send is pending: {other:?}"),
    };
    let tx = rebroadcast_call(Address::from([1; 20]), Address::from([2; 20]), 31337, &loaded).unwrap();
    assert_eq!(tx.nonce, Some(9), "the reserved nonce, not a fresh one");
    let expected: Vec<u8> = postCall {
        seq: 3,
        link: reserved.link,
        parentSeq: 0,
        parentLink: B256::ZERO,
        signature: signature.parse().unwrap(),
    }
    .abi_encode();
    assert_eq!(
        tx.input.into_input().unwrap().as_ref(),
        expected.as_slice(),
        "the reserved head's exact bytes, not the newer head"
    );
}

#[test]
fn a_replacement_reservation_records_the_call_being_sent() {
    // The dropped-H1 → sent-H2 path: the record stores H2's head, parent, and signature
    // together, so a later reconciliation rebroadcasts exactly the sent call.
    let dir = crate::test_support::scratch_dir("log-head-replacement");
    std::fs::create_dir_all(&dir).unwrap();
    let store =
        super::super::submission::SubmissionStore::pending(&dir, Address::from([2; 20]), Address::from([1; 20]), 31337)
            .unwrap();
    let first = Head { seq: 1, link: B256::from([1; 32]) };
    let mut reservation = match store.reserve(7, 1, first, 1_000).unwrap() {
        super::super::submission::Reserve::Nonce(reservation) => reservation,
        other => panic!("a fresh store reserves a nonce: {other:?}"),
    };
    reservation.tx_hash = Some(B256::from([8; 32]));
    reservation.signature = Some(format!("0x{}", hex::encode([7u8; 65])));
    store.update(&reservation).unwrap();
    // H1 dropped: the endpoint knows no transaction at the stored hash. The send path —
    // `HeadsRpc::post`, modelled here — rewrites the record to the replacement call before
    // sending, and stores the hash with the rewritten record; the restart then sees a
    // pending replacement, not an unsent one.
    let second = Head { seq: 2, link: B256::from([2; 32]) };
    let second_signature = format!("0x{}", hex::encode([9u8; 65]));
    let parent = Head { seq: 1, link: B256::from([1; 32]) };
    let mut replacement = match store.reserve(7, 1, second, 2_000).unwrap() {
        super::super::submission::Reserve::Pending(pending) => pending,
        other => panic!("the outstanding send is pending: {other:?}"),
    };
    replacement.record_send(1, second, Some(parent));
    replacement.tx_hash = Some(B256::from([10; 32]));
    replacement.signature = Some(second_signature.clone());
    store.update(&replacement).unwrap();
    // A restart sees the pending replacement; reconciliation rebuilds its exact bytes.
    let reloaded = match store.reserve(7, 1, second, 3_000).unwrap() {
        super::super::submission::Reserve::Pending(pending) => pending,
        other => panic!("the replacement is pending after restart: {other:?}"),
    };
    assert_eq!(reloaded.head, Some((1, second)), "the record carries the sent head, not the dropped one");
    assert_eq!(reloaded.signature.as_deref(), Some(second_signature.as_str()), "and its signature, paired");
    let tx = rebroadcast_call(Address::from([1; 20]), Address::from([2; 20]), 31337, &reloaded).unwrap();
    let expected: Vec<u8> = postCall {
        seq: 2,
        link: second.link,
        parentSeq: 1,
        parentLink: parent.link,
        signature: second_signature.parse().unwrap(),
    }
    .abi_encode();
    assert_eq!(tx.input.into_input().unwrap().as_ref(), expected.as_slice());
}

#[tokio::test]
async fn a_held_store_outruns_its_bound_and_returns() {
    // The watchdog bound, not the operation's length, decides: a host-held filesystem
    // operation lets the poster loop — and the stall alarm running in it — move on with an
    // error the caller logs as log_head_post_failed and retries next tick. The abandoned
    // operation still completes in the background; real time, not the paused test clock.
    let bound = Duration::from_millis(50);
    let started = std::time::Instant::now();
    let held = bounded_store(bound, || {
        std::thread::sleep(Duration::from_secs(5));
        Ok(7)
    })
    .await;
    let error = held.unwrap_err();
    assert!(format!("{error:#}").contains("did not complete within"), "{error:#}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the bound, not the held operation, bounds the wait: {:?}",
        started.elapsed()
    );
    // A store that answers inside the bound returns its value.
    let done = bounded_store(bound, || Ok(7)).await.unwrap();
    assert_eq!(done, 7);
}

#[tokio::test]
async fn the_complete_call_is_durable_before_the_broadcast() {
    // The endpoint fails the broadcast, as a crash after the transaction left looks to the node:
    // the record it reads at send time, and the one left behind, already carry the call that may
    // land — epoch, head, parent, and signature — with no hash yet.
    let dir = crate::test_support::scratch_dir("log-head-persist-before-broadcast");
    std::fs::create_dir_all(&dir).unwrap();
    let record = dir.join("log-head-submission.json");
    let endpoint = Endpoint::start(record.clone()).await;
    endpoint.state.lock().unwrap().refuse_sends = true;
    let rpc = crate::log_heads::tests::heads_rpc(&endpoint.url, &dir);
    let head = Head { seq: 5, link: B256::from([5; 32]) };
    let parent = Head { seq: 3, link: B256::from([3; 32]) };
    let signature = format!("0x{}", hex::encode([7u8; 65]));
    let error = rpc.post(1, head, Some(parent), &signature).await.unwrap_err();
    assert!(format!("{error:#}").contains("log-head RPC failed"), "{error:#}");
    let at_send = endpoint.state.lock().unwrap().at_send.clone().expect("the broadcast reached the endpoint");
    let left: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    for record in [at_send, left] {
        assert_eq!(record["nonce"], 7, "{record}");
        assert_eq!(record["head"], serde_json::to_value((1u64, head)).unwrap(), "{record}");
        assert_eq!(record["parent_seq"], 3, "{record}");
        assert_eq!(record["parent_link"], serde_json::to_value(parent.link).unwrap(), "{record}");
        assert_eq!(record["signature"], signature.as_str(), "the signature is durable before the send: {record}");
        assert!(record.get("tx_hash").is_none(), "no hash before the endpoint answers: {record}");
    }
}

#[tokio::test]
async fn post_sends_waits_then_reconciles_a_pending_send_by_its_hash() {
    // A send stores its hash and is accepted; a newer head at the same count waits inside the
    // in-memory bound; a restarted process reconciles at once: an unknown hash is cleared and the
    // newer head re-sent at the same nonce, a known one rebroadcast as the reserved call.
    let dir = crate::test_support::scratch_dir("log-head-post-branches");
    std::fs::create_dir_all(&dir).unwrap();
    let record = dir.join("log-head-submission.json");
    let endpoint = Endpoint::start(record.clone()).await;
    let signature = format!("0x{}", hex::encode([7u8; 65]));
    let (first, second) = (Head { seq: 5, link: B256::from([5; 32]) }, Head { seq: 6, link: B256::from([6; 32]) });
    let rpc = crate::log_heads::tests::heads_rpc(&endpoint.url, &dir);
    assert_eq!(rpc.post(1, first, None, &signature).await.unwrap(), PostOutcome::Accepted);
    let stored: Value = serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    assert_eq!(stored["tx_hash"], serde_json::to_value(B256::repeat_byte(0x33)).unwrap(), "the hash is stored");
    assert_eq!(rpc.post(1, second, None, &signature).await.unwrap(), PostOutcome::Waiting);
    assert_eq!(endpoint.sent().len(), 1, "a pending send inside its bound is not repeated");
    // A restart, the endpoint having dropped the send: the newer head goes out at the same nonce.
    endpoint.state.lock().unwrap().known = false;
    let restarted = crate::log_heads::tests::heads_rpc(&endpoint.url, &dir);
    assert_eq!(restarted.post(1, second, None, &signature).await.unwrap(), PostOutcome::Accepted);
    let sent = endpoint.sent();
    assert_eq!((sent.len(), sent[1]["nonce"].as_str()), (2, Some("0x7")), "re-sent at the same nonce");
    assert_eq!(posted_seq(&sent[1]), 6, "the newer head replaces the dropped one");
    // Another restart, the send still known: the reserved call is rebroadcast, not a newer head.
    endpoint.state.lock().unwrap().known = true;
    let restarted = crate::log_heads::tests::heads_rpc(&endpoint.url, &dir);
    let third = Head { seq: 7, link: B256::from([7; 32]) };
    assert_eq!(restarted.post(1, third, None, &signature).await.unwrap(), PostOutcome::Waiting);
    let sent = endpoint.sent();
    assert_eq!((sent.len(), posted_seq(&sent[2])), (3, 6), "the reserved head's call is rebroadcast");
}

// ── Helpers ──

/// A loopback endpoint for the post path's RPCs, with the relayer's count at seven.
struct Endpoint {
    url: String,
    state: Arc<Mutex<EndpointState>>,
    task: tokio::task::JoinHandle<()>,
}

/// What the endpoint has seen and how it answers.
struct EndpointState {
    record: PathBuf,
    /// The reservation as it stood at the last `eth_sendTransaction`.
    at_send: Option<Value>,
    sent: Vec<Value>,
    /// Fail every broadcast, as a crash after the transaction left looks to the node.
    refuse_sends: bool,
    /// Whether `eth_getTransactionByHash` finds the sent transaction.
    known: bool,
}

impl Endpoint {
    async fn start(record: PathBuf) -> Self {
        let state = Arc::new(Mutex::new(EndpointState {
            record,
            at_send: None,
            sent: Vec::new(),
            refuse_sends: false,
            known: true,
        }));
        let app = Router::new().route("/", post(answer)).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, state, task }
    }

    fn sent(&self) -> Vec<Value> {
        self.state.lock().unwrap().sent.clone()
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn answer(State(state): State<Arc<Mutex<EndpointState>>>, Json(request): Json<Value>) -> Json<Value> {
    let mut state = state.lock().unwrap();
    let result = match request["method"].as_str().unwrap() {
        "eth_getTransactionCount" => json!("0x7"),
        "eth_estimateGas" => json!("0x30d40"),
        "eth_getTransactionByHash" => {
            if state.known {
                state.sent.last().cloned().unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        "eth_sendTransaction" => {
            state.at_send = Some(serde_json::from_slice(&std::fs::read(&state.record).unwrap()).unwrap());
            if state.refuse_sends {
                return Json(
                    json!({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": "no answer"}}),
                );
            }
            state.sent.push(request["params"][0].clone());
            json!(B256::repeat_byte(0x33))
        }
        method => panic!("the post path calls no {method}"),
    };
    Json(json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
}

/// The head seq a sent `post` call names.
fn posted_seq(tx: &Value) -> u64 {
    let input = tx.get("input").or_else(|| tx.get("data")).and_then(Value::as_str).expect("the call's input");
    let bytes = hex::decode(input.trim_start_matches("0x")).unwrap();
    postCall::abi_decode(&bytes).unwrap().seq
}
