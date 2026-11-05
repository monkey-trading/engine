use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::routing::post;
use serde_json::{Value, json};

use super::*;

#[tokio::test]
async fn a_strict_majority_answers_alike_and_anything_less_is_refused() {
    let (a, b) = (format!("0x{}", "11".repeat(32)), format!("0x{}", "22".repeat(32)));
    let providers = [
        Stub::answering([("eth_call", ok(&a))]).await,
        Stub::answering([("eth_call", ok(&a))]).await,
        Stub::answering([("eth_call", ok(&b))]).await,
    ];
    let ethereum = client(&providers, &Stub::answering([]).await);
    assert_eq!(ethereum.beacon_root(1).await.unwrap(), Some(a.parse().unwrap()));
    // One answer and two failures is no majority, and the refusal names each provider.
    let providers = [
        Stub::answering([("eth_call", ok(&a))]).await,
        Stub::answering([("eth_call", failure(-32005, "limit exceeded"))]).await,
        Stub::answering([("eth_call", failure(-32005, "limit exceeded"))]).await,
    ];
    let error = ethereum_err(client(&providers, &Stub::answering([]).await).beacon_root(1).await);
    assert!(error.contains("no 2 of 3") && error.contains("p1: ") && error.contains("limit exceeded"), "{error}");
}

#[tokio::test]
async fn a_revert_reads_as_no_beacon_root_and_any_other_error_as_a_failure() {
    let reverted = || failure(3, "execution reverted");
    let providers = [
        Stub::answering([("eth_call", reverted())]).await,
        Stub::answering([("eth_call", reverted())]).await,
        Stub::answering([("eth_call", reverted())]).await,
    ];
    assert_eq!(client(&providers, &Stub::answering([]).await).beacon_root(1).await.unwrap(), None);
    let limited = || failure(-32005, "limit exceeded");
    let providers = [
        Stub::answering([("eth_call", limited())]).await,
        Stub::answering([("eth_call", limited())]).await,
        Stub::answering([("eth_call", limited())]).await,
    ];
    assert!(client(&providers, &Stub::answering([]).await).beacon_root(1).await.is_err());
}

#[tokio::test]
async fn every_send_names_its_nonce_and_a_replacement_outbids_the_fees_of_the_pending_one() {
    let pending = json!({
        "from": "0x2222222222222222222222222222222222222222",
        "to": "0x4444444444444444444444444444444444444444",
        "nonce": "0x7", "gas": "0x30000", "maxFeePerGas": "0x64", "maxPriorityFeePerGas": "0x14",
        "value": "0x0", "input": "0x", "type": "0x2", "chainId": "0x1",
    });
    let signer = Stub::answering([
        ("eth_maxPriorityFeePerGas", ok("0x2")),
        ("eth_getBlockByNumber", Ok(json!({"baseFeePerGas": "0xa"}))),
        ("eth_getTransactionByHash", Ok(pending)),
        ("eth_estimateGas", ok("0x5208")),
        ("eth_sendTransaction", ok(&format!("0x{}", "33".repeat(32)))),
    ])
    .await;
    let providers = [Stub::answering([]).await, Stub::answering([]).await, Stub::answering([]).await];
    let ethereum = client(&providers, &signer);
    // A fresh send names its nonce and offers twice the base fee plus the tip.
    ethereum.send(vec![1], U256::from(5), 7, None).await.unwrap();
    let sent = signer.last("eth_sendTransaction");
    assert_eq!(sent["nonce"].as_str(), Some("0x7"));
    assert_eq!((sent["maxFeePerGas"].as_str(), sent["maxPriorityFeePerGas"].as_str()), (Some("0x16"), Some("0x2")));
    // A replacement goes out at the nonce it is given and pays a quarter more than the pending one.
    ethereum.send(vec![1], U256::from(5), 7, Some(B256::repeat_byte(9))).await.unwrap();
    let sent = signer.last("eth_sendTransaction");
    assert_eq!(sent["nonce"].as_str(), Some("0x7"));
    assert_eq!((sent["maxFeePerGas"].as_str(), sent["maxPriorityFeePerGas"].as_str()), (Some("0x7e"), Some("0x1a")));
}

// ── Helpers ──

type Answer = Result<Value, Value>;

/// A JSON-RPC endpoint answering each method with a fixed result or error, recording each request's
/// first parameter.
struct Stub {
    url: String,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Stub {
    async fn answering<const N: usize>(answers: [(&'static str, Answer); N]) -> Self {
        let answers: Arc<BTreeMap<&'static str, Answer>> = Arc::new(answers.into_iter().collect());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = (answers, seen.clone());
        let app = Router::new().route("/", post(answer)).with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, seen }
    }

    /// The first parameter of the last `method` request.
    fn last(&self, method: &str) -> Value {
        let seen = self.seen.lock().unwrap();
        seen.iter().rev().find(|(name, _)| name == method).map(|(_, params)| params.clone()).unwrap()
    }
}

type StubState = (Arc<BTreeMap<&'static str, Answer>>, Arc<Mutex<Vec<(String, Value)>>>);

async fn answer(State((answers, seen)): State<StubState>, axum::Json(request): axum::Json<Value>) -> axum::Json<Value> {
    let method = request["method"].as_str().unwrap_or_default().to_owned();
    seen.lock().unwrap().push((method.clone(), request["params"][0].clone()));
    let id = request["id"].clone();
    axum::Json(match answers.get(method.as_str()) {
        Some(Ok(result)) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Some(Err(error)) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        None => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no such method"}}),
    })
}

fn ok(result: &str) -> Answer {
    Ok(json!(result))
}

fn failure(code: i64, message: &str) -> Answer {
    Err(json!({"code": code, "message": message}))
}

/// The client over three providers and a signing endpoint, all stubs.
fn client(providers: &[Stub; 3], signer: &Stub) -> Ethereum {
    let list: String = providers
        .iter()
        .enumerate()
        .map(|(i, stub)| format!("[[providers]]\nname = 'p{i}'\nrpc_url = '{}'\n", stub.url))
        .collect();
    let cfg: SeedRelaySection = toml::from_str(&format!(
        "chain_id = 1\nrpc_url = '{}'\nrelayer = '0x2222222222222222222222222222222222222222'\n\
         beacon_api = 'http://127.0.0.1:5052'\nrelay = '0x4444444444444444444444444444444444444444'\n\
         receiver = '0x5555555555555555555555555555555555555555'\npoll_interval = '60s'\nrpc_timeout = '5s'\n\
         state_timeout = '30s'\nresend_after = '1h'\nl2_max_fee_per_gas = '0.1'\nmin_balance = '0.05'\n{list}",
        signer.url
    ))
    .unwrap();
    Ethereum::new(&cfg).unwrap()
}

fn ethereum_err<T: std::fmt::Debug>(result: anyhow::Result<T>) -> String {
    format!("{:#}", result.unwrap_err())
}
