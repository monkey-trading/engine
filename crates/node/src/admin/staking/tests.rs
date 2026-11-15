//! `engine admin init-staking` reads the staking contract's epoch and cooldown through the config's
//! custody providers and refuses a different one before it signs anything: the node it would send to
//! is never reached.

use std::fs;

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::{Block, Transaction};
use alloy::sol;
use alloy::sol_types::SolCall;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use clap::Parser;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::admin::{AdminArgs, HexKey, KeyEnv, run_with_clock};
use crate::test_support::{ANVIL_CONFIG, scratch_dir};

// The stand-in contract's reads.
sol! {
    function usdc() external view returns (address);
    function epoch() external view returns (uint256);
    function cooldown() external view returns (uint256);
}

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    args: AdminArgs,
}

const DEV_KEY: &str = "0x4242424242424242424242424242424242424242424242424242424242424242";
const ZERO_PIN: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";
const USDC: Address = Address::repeat_byte(0x55);

#[tokio::test]
async fn a_cooldown_or_epoch_that_is_not_the_contracts_is_refused_before_anything_is_signed() {
    // Nothing listens at the node's port, so a command that was signed and sent would fail to connect.
    let node = {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        format!("https://127.0.0.1:{port}")
    };
    let rpc = rpc_stand_in(86_400, 2_419_200).await;
    let config = config_for(&rpc, "chain");
    let run = |epoch_ms: &str, cooldown_ms: &str| {
        let argv = [
            "engine",
            "--config",
            config.to_str().unwrap(),
            "--node",
            &node,
            "--pin",
            ZERO_PIN,
            "init-staking",
            "--start",
            "2027-01-01T00:00:00Z",
            "--liquidity-floor",
            "30500000",
            "--epoch-ms",
            epoch_ms,
            "--cooldown-ms",
            cooldown_ms,
        ];
        let args = Cli::try_parse_from(argv).unwrap().args;
        let env = KeyEnv { key_file: None, key: Some(HexKey(DEV_KEY.into())) };
        async move { format!("{:#}", run_with_clock(args, env, || 1_000).await.unwrap_err()) }
    };

    let epoch = run("1800000", "2419200000").await;
    assert!(epoch.contains("epoch 1800000 ms differs from the staking contract's 86400 s"), "{epoch}");
    let cooldown = run("86400000", "86400000").await;
    assert!(cooldown.contains("cooldown 86400000 ms differs from the staking contract's 2419200 s"), "{cooldown}");
    // The contract's own values pass the check, and the run goes on to the node, which is not there.
    let sent = run("86400000", "2419200000").await;
    assert!(!sent.contains("staking contract"), "{sent}");
    assert!(sent.contains("127.0.0.1"), "the run reached for the node: {sent}");
}

/// The Anvil config with its custody section replaced by three paths of the stand-in at `rpc` and a staking contract,
/// written to a file named for `test`.
fn config_for(rpc: &str, test: &str) -> std::path::PathBuf {
    let dir = scratch_dir(&format!("init-staking-{test}"));
    fs::create_dir_all(&dir).unwrap();
    let custody = format!(
        r#"[custody]
provider = "evm"
chain_id = 31337
vault = "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512"

[custody.evm]
providers = [
  {{ name = "a", rpc_url = "{rpc}/a" }},
  {{ name = "b", rpc_url = "{rpc}/b" }},
  {{ name = "c", rpc_url = "{rpc}/c" }},
]
usdc = "{USDC:#x}"
finality_tag = "safe"
withdrawal_finality_tag = "finalized"
confirmations = 2
start_block = 0
max_block_range = 1000
poll_interval = "200ms"
rpc_timeout = "5s"
stall_critical_after = "5m"
finality_stalled_after = "5m"
cursor_path = "{dir}/custody-cursor.json"

[custody.evm.staking]
contract = "0x00000000000000000000000000000000000000dd"
cursor_path = "{dir}/staking-cursor.json"

[custody.evm.staking.registry]
contract = "0x00000000000000000000000000000000000000ee"
"#,
        dir = dir.display()
    );
    let path = dir.join("node.toml");
    let anvil = fs::read_to_string(ANVIL_CONFIG).unwrap();
    let (before, rest) = anvil.split_once("[custody]\n").expect("a custody section");
    let (_, after) = rest.split_once("\n[feeds]").expect("a feeds section after it");
    fs::write(&path, format!("{before}{custody}\n[feeds]{after}")).unwrap();
    path
}

/// A JSON-RPC endpoint for any path whose chain is at block 12, final at 7, whose USDC is `USDC` and
/// whose staking contract holds the epoch and cooldown given. Returns its `http://` address.
async fn rpc_stand_in(epoch: u64, cooldown: u64) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(post(answer)).with_state((epoch, cooldown));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn answer(State(terms): State<(u64, u64)>, Json(request): Json<Value>) -> Json<Value> {
    let one = |request: &Value| {
        let result = match request["method"].as_str().unwrap() {
            "eth_chainId" => json!("0x7a69"),
            "eth_call" => {
                let input: Bytes = serde_json::from_value(request["params"][0]["input"].clone()).unwrap();
                let value = match &input[..4] {
                    s if s == usdcCall::SELECTOR => {
                        return json!({"jsonrpc":"2.0","id":request["id"],"result":
                        Bytes::from(usdcCall::abi_encode_returns(&USDC))});
                    }
                    s if s == epochCall::SELECTOR => terms.0,
                    s if s == cooldownCall::SELECTOR => terms.1,
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
