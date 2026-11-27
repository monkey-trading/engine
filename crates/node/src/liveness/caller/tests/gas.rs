//! The gas a relayed head is sent with.

use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::SolCall;

use super::*;

/// A relayed head is sent with gas for a turn's first head even when its estimate is from before the
/// turn's boundary; a head's estimate above the floor keeps its quarter, and no other call is floored.
#[tokio::test]
async fn a_relayed_head_carries_gas_for_a_turns_first_head_whatever_its_estimate() {
    let sent = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let recorded = sent.clone();
    let answer = move |axum::Json(request): axum::Json<serde_json::Value>| {
        let recorded = recorded.clone();
        async move {
            let result = match request["method"].as_str().unwrap() {
                "eth_estimateGas" => serde_json::json!(format!("{:#x}", 115_000)),
                _ => {
                    recorded.lock().unwrap().push(request["params"][0].clone());
                    serde_json::json!(B256::ZERO)
                }
            };
            axum::Json(serde_json::json!({"jsonrpc": "2.0", "id": request["id"], "result": result}))
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(axum::routing::post(answer));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let section =
        format!("rpc_url = '{url}'\nrelayer = '{}'\nretry_interval = '1s'\nrpc_timeout = '2s'", Address::ZERO);
    let checkpoints: crate::config::CheckpointSection = toml::from_str(&section).unwrap();
    let liveness = crate::config::LivenessSection {
        caller: Address::repeat_byte(0x22),
        min_balance: U256::from(1),
        read_interval: TEST_POLL,
        fallback_interval: TEST_POLL,
    };
    let endpoint = Endpoint::new(&checkpoints, &liveness, 31_337).unwrap();
    let head = signed(1, 8);
    endpoint.send(DOMAIN.log_heads, head.head.calldata(&head.signature).unwrap()).await.unwrap();
    endpoint.send(DOMAIN.standby.schedule, witnessCall {}.abi_encode()).await.unwrap();
    let gas: Vec<_> = sent.lock().unwrap().iter().map(|tx| tx["gas"].as_str().unwrap().to_string()).collect();
    assert_eq!(gas, [format!("{HEAD_GAS_FLOOR:#x}"), format!("{:#x}", 143_750)], "a head's floor, a witness's quarter");
}
