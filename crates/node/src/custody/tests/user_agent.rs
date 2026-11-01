//! The RPC adapter names itself: a provider can refuse a request without a User-Agent, so every
//! JSON-RPC request carries one.

use std::sync::{Arc, Mutex};

use alloy::primitives::B256;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::config::EvmCustodySection;
use crate::custody::evm::VaultReader;
use crate::custody::rpc::VaultRpc;
use crate::custody::tests::fakes::{VAULT, evm_toml};

#[tokio::test]
async fn rpc_requests_carry_a_user_agent() {
    let agents = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let agents = agents.clone();
        async move {
            axum::serve(listener, Router::new().route("/", post(answer)).with_state(agents)).await.unwrap();
        }
    });

    let toml = evm_toml(&std::env::temp_dir(), 1_000)
        .replace("rpc_url = \"http://127.0.0.1:1\"", &format!("rpc_url = \"http://{addr}\""))
        .replace("rpc_timeout = \"1s\"", "rpc_timeout = \"200ms\"");
    let cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
    let rpc = VaultRpc::new(&cfg, &cfg.providers[0].rpc_url, VAULT).unwrap();

    let finality = rpc.finality().await.unwrap();
    assert_eq!(finality.head, 7);
    server.abort();
    let _ = server.await;
    let agents = agents.lock().unwrap();
    assert!(!agents.is_empty(), "the finality read issues at least one request");
    assert!(
        agents.iter().all(|agent| agent == concat!("engine-custody/", env!("CARGO_PKG_VERSION"))),
        "every request names the engine: {agents:?}",
    );
}

async fn answer(
    State(agents): State<Arc<Mutex<Vec<String>>>>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Json<Value> {
    agents
        .lock()
        .unwrap()
        .push(headers.get(axum::http::header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned());
    let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
    block.header.number = 7;
    block.header.timestamp = 1007;
    block.header.hash = B256::repeat_byte(7);
    Json(json!({"jsonrpc":"2.0","id":request["id"],"result":serde_json::to_value(block).unwrap()}))
}
