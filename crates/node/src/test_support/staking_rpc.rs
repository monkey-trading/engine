//! A JSON-RPC stand-in for the custody providers' chain and the staking contract on it.

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::{Block, Transaction};
use alloy::sol;
use alloy::sol_types::SolCall;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

sol! {
    function usdc() external view returns (address);
    function epoch() external view returns (uint256);
    function cooldown() external view returns (uint256);
}

/// A JSON-RPC endpoint for any path whose chain is at block 12, final at 7, whose chain id is 31337 (the
/// Anvil config's), whose USDC is `usdc` and whose staking contract holds the epoch and cooldown given.
/// Returns its `http://` address.
pub async fn staking_rpc(usdc: Address, epoch: u64, cooldown: u64) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(post(answer)).with_state((usdc, epoch, cooldown));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn answer(State((usdc, epoch, cooldown)): State<(Address, u64, u64)>, Json(request): Json<Value>) -> Json<Value> {
    let one = |request: &Value| {
        let result = match request["method"].as_str().unwrap() {
            "eth_chainId" => json!("0x7a69"),
            "eth_call" => {
                let input: Bytes = serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
                let value = match &input[..4] {
                    s if s == usdcCall::SELECTOR => {
                        return json!({"jsonrpc":"2.0","id":request["id"],"result":
                        Bytes::from(usdcCall::abi_encode_returns(&usdc))});
                    }
                    s if s == epochCall::SELECTOR => epoch,
                    s if s == cooldownCall::SELECTOR => cooldown,
                    other => panic!("unexpected call {other:?}"),
                };
                json!(Bytes::from(epochCall::abi_encode_returns(&U256::from(value))))
            }
            "eth_getBlockByNumber" => {
                let number = match request["params"][0].as_str().unwrap() {
                    "safe" | "finalized" => 7,
                    "latest" => 12,
                    hex => u64::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap(),
                };
                let mut block = Block::<Transaction>::default();
                block.header.number = number;
                block.header.timestamp = 1234;
                block.header.hash = B256::repeat_byte(number as u8);
                serde_json::to_value(block).unwrap()
            }
            method => panic!("unexpected method {method}"),
        };
        json!({"jsonrpc":"2.0","id":request["id"],"result":result})
    };
    Json(match request.as_array() {
        Some(batch) => Value::Array(batch.iter().map(one).collect()),
        None => one(&request),
    })
}
