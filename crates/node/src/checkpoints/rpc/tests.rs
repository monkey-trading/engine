//! A tag from one fork cannot lend confirmation depth to a replacement fork.

use std::sync::{Arc, Mutex};

use super::*;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};

fn block(number: u64, hash: u8, parent: u8) -> Value {
    let mut block = alloy::rpc::types::Block::<alloy::rpc::types::Transaction>::default();
    block.header.number = number;
    block.header.hash = alloy::primitives::B256::repeat_byte(hash);
    block.header.parent_hash = alloy::primitives::B256::repeat_byte(parent);
    serde_json::to_value(block).unwrap()
}

async fn fork_switch(Json(request): Json<Value>) -> Json<Value> {
    let method = request["method"].as_str().unwrap();
    let old_98 = format!("{:#x}", alloy::primitives::B256::repeat_byte(98));
    let replacement_98 = format!("{:#x}", alloy::primitives::B256::repeat_byte(198));
    let result = match method {
        "eth_chainId" => json!("0x7a69"),
        // The tag was sampled before an honest reorg. By-number reads now see the replacement
        // tip at 98, which has zero confirmations, while the old tag was at 100.
        "eth_getBlockByNumber" if request["params"][0] == "0x62" => block(98, 198, 197),
        "eth_getBlockByNumber" => block(100, 100, 99),
        "eth_getBlockByHash" if request["params"][0] == old_98 => block(98, 98, 97),
        "eth_getBlockByHash" => block(99, 99, 98),
        "eth_call" if request["params"][1]["blockHash"] == replacement_98 => json!(format!("0x{}", "00".repeat(32))),
        "eth_call" => {
            return Json(
                json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"header is no longer canonical"}}),
            );
        }
        _ => panic!("unexpected method {method}"),
    };
    Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
}

#[tokio::test]
async fn confirmation_depth_cannot_cross_a_reorg_between_tag_and_target_reads() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server =
        tokio::spawn(async move { axum::serve(listener, Router::new().route("/", post(fork_switch))).await.unwrap() });
    let cfg: CheckpointSection = toml::from_str(&format!(
        "rpc_url='http://{address}'\nrelayer='0x{}'\nretry_interval='1ms'\nrpc_timeout='2s'",
        "01".repeat(20)
    ))
    .unwrap();
    let rpc = CheckpointRpc::new(
        &cfg,
        CheckpointDomain { chain_id: 31337, vault: engine_core::types::AccountId([0; 20]) },
        Address::ZERO,
        FinalityTag::Latest,
        2,
        &crate::test_support::scratch_dir("checkpoint-ancestry-rpc"),
        engine_core::types::AccountId(crate::tee::EnclaveKey::test_key().address()),
    )
    .unwrap();
    assert!(rpc.state(View::Finalized).await.is_err(), "a replacement tip must not inherit the orphan tag's depth");
    server.abort();
}

#[tokio::test]
async fn reads_carry_a_user_agent() {
    // A provider can refuse a request without a User-Agent; `rotation_enabled` against a vault
    // with no registry is one read and answers from zero return data.
    let agents = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let agents = agents.clone();
        async move {
            async fn answer(
                State(agents): State<Arc<Mutex<Vec<String>>>>,
                headers: HeaderMap,
                Json(request): Json<Value>,
            ) -> Json<Value> {
                agents.lock().unwrap().push(
                    headers
                        .get(axum::http::header::USER_AGENT)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned(),
                );
                assert_eq!(request["method"], "eth_call");
                Json(json!({"jsonrpc":"2.0","id":request["id"],"result":format!("0x{}", "00".repeat(32))}))
            }
            axum::serve(listener, Router::new().route("/", post(answer)).with_state(agents.clone())).await.unwrap();
        }
    });
    let cfg: CheckpointSection = toml::from_str(&format!(
        "rpc_url='http://{address}'\nrelayer='0x{}'\nretry_interval='1ms'\nrpc_timeout='2s'",
        "01".repeat(20)
    ))
    .unwrap();
    let rpc = CheckpointRpc::read_only(
        &cfg,
        CheckpointDomain { chain_id: 31337, vault: engine_core::types::AccountId([0; 20]) },
        Address::ZERO,
        FinalityTag::Latest,
        2,
    )
    .unwrap();
    assert!(!rpc.rotation_enabled().await.unwrap(), "zero return data names no registry");
    server.abort();
    let _ = server.await;
    assert_eq!(agents.lock().unwrap().as_slice(), [concat!("engine-checkpoints/", env!("CARGO_PKG_VERSION"))],);
}
