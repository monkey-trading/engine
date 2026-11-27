//! A carried genesis's start reads the move's contracts through the custody quorum, here over a JSON-RPC stand-in
//! serving them as `contracts/src` answers after a done move, and carries only under the staking contract the
//! registry records as the one it deployed, not under one that holds the same token and answers `registry()` as that
//! one does.

use std::path::Path;

use alloy::primitives::{Address, B256, Bytes};
use alloy::rpc::types::{Block, Transaction};
use alloy::sol;
use alloy::sol_types::SolCall;
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::net::TcpListener;

use super::boot::{Expected, check_kept, hold, unmet};
use super::chain::{
    self, MoveContracts, checkpointRootsCall, checkpointSignerCall, hasCheckpointCall, latestSeqCall, monkeyCall,
    operatorInEffectCall, operatorInEffectReturn, registryCall, rotationSwitchCall, stakingCall, vaultCall,
};
use super::*;
use crate::config::{NodeConfig, RpcProviderSection};
use crate::custody::ContractQuorum;
use crate::test_support::{ANVIL_CONFIG, scratch_dir};

sol! {
    function usdc() external view returns (address);
}

/// The node's reference log whose last checkpoint is a source that carries.
const CARRIED: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/reference/v90-carried.jsonl");
const SOURCE: Address = Address::repeat_byte(0xa1);
const REGISTRY: Address = Address::repeat_byte(0xa2);
const MINTER: Address = Address::repeat_byte(0xa3);
const TOKEN: Address = Address::repeat_byte(0xa4);
/// The staking contract the registry deployed, which the source log mirrored.
const DEPLOYED: Address = Address::repeat_byte(0xa5);
/// A contract the registry did not deploy, answering `monkey()` and `registry()` as `DEPLOYED` does.
const IMPOSTOR: Address = Address::repeat_byte(0xfa);
const OPERATOR: Address = Address::repeat_byte(0xa6);
const ROOT: B256 = B256::repeat_byte(0x44);

/// The chain after a done move, as the contracts in `contracts/src` answer, with `IMPOSTOR` beside them.
#[derive(Clone, Copy)]
struct Chain {
    vault: Address,
    usdc: Address,
}

impl Chain {
    /// The answer of the contract `to` to the call `input`.
    fn call(&self, to: Address, input: &[u8]) -> Bytes {
        let selector: [u8; 4] = input[..4].try_into().unwrap();
        let address = |a: Address| Bytes::from(vaultCall::abi_encode_returns(&a));
        match (to, selector) {
            (to, s) if to == self.vault && s == usdcCall::SELECTOR => address(self.usdc),
            (SOURCE, s) if s == checkpointSignerCall::SELECTOR => address(Address::ZERO),
            (SOURCE, s) if s == hasCheckpointCall::SELECTOR => hasCheckpointCall::abi_encode_returns(&true).into(),
            (SOURCE, s) if s == latestSeqCall::SELECTOR => latestSeqCall::abi_encode_returns(&70).into(),
            (SOURCE, s) if s == checkpointRootsCall::SELECTOR => checkpointRootsCall::abi_encode_returns(&ROOT).into(),
            (SOURCE, s) if s == registryCall::SELECTOR => address(REGISTRY),
            (REGISTRY, s) if s == vaultCall::SELECTOR => address(self.vault),
            (REGISTRY, s) if s == rotationSwitchCall::SELECTOR => rotationSwitchCall::abi_encode_returns(&false).into(),
            (REGISTRY, s) if s == operatorInEffectCall::SELECTOR => {
                let operator = operatorInEffectReturn {
                    candidate: Address::repeat_byte(0xc1),
                    adminKey: OPERATOR,
                    release: B256::ZERO,
                    since: 0,
                };
                operatorInEffectCall::abi_encode_returns(&operator).into()
            }
            (REGISTRY, s) if s == stakingCall::SELECTOR => address(DEPLOYED),
            (MINTER, s) if s == vaultCall::SELECTOR => address(self.vault),
            (MINTER, s) if s == monkeyCall::SELECTOR => address(TOKEN),
            (DEPLOYED | IMPOSTOR, s) if s == monkeyCall::SELECTOR => address(TOKEN),
            (DEPLOYED | IMPOSTOR, s) if s == registryCall::SELECTOR => address(REGISTRY),
            other => panic!("unexpected call {other:?}"),
        }
    }
}

/// Answers a JSON-RPC request, or a batch of them, from `chain`.
async fn answer(State(chain): State<Chain>, Json(request): Json<Value>) -> Json<Value> {
    let one = |request: &Value| {
        let result = match request["method"].as_str().unwrap() {
            "eth_chainId" => json!("0x7a69"),
            "eth_call" => {
                let call = &request["params"][0];
                let to: Address = serde_json::from_value(call["to"].clone()).unwrap();
                let input: Bytes = serde_json::from_value(call["input"].clone()).unwrap();
                json!(chain.call(to, &input))
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

/// The Anvil config with its three custody providers on a stand-in serving `Chain`.
async fn served() -> NodeConfig {
    let mut cfg = NodeConfig::load(Path::new(ANVIL_CONFIG)).expect("the Anvil config loads");
    let vault: Address = cfg.custody.vault.parse().unwrap();
    let evm = cfg.custody.evm.as_mut().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(post(answer)).with_state(Chain { vault, usdc: evm.usdc });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for (provider, path) in evm.providers.iter_mut().zip(["a", "b", "c"]) {
        let section = format!("name = '{path}'\nrpc_url = '{rpc}/{path}'");
        provider.rpc_url = toml::from_str::<RpcProviderSection>(&section).unwrap().rpc_url;
    }
    cfg
}

#[tokio::test]
async fn a_carried_start_refuses_a_staking_contract_the_registry_did_not_deploy() {
    let cfg = served().await;
    let evm = cfg.custody.evm.as_ref().unwrap();
    let vault: Address = cfg.custody.vault.parse().unwrap();
    let expected = Expected { chain_id: cfg.custody.chain_id, vault, source: SOURCE, operator: OPERATOR };
    let contracts = |staking| MoveContracts { source: SOURCE, registry: REGISTRY, minter: MINTER, staking };
    let quorum = ContractQuorum::connect(&cfg.custody, "carry").unwrap();
    let read = |staking| chain::read(quorum.quorum(), contracts(staking), (expected.chain_id, evm.usdc));
    let dir = scratch_dir("carry-staking-binding");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("carry.json");
    let built = build(Path::new(CARRIED), None).unwrap();
    write(&built, &path).unwrap();

    // The registry's own staking contract: the move reads as done and kept, and the start carries.
    let state = read(DEPLOYED).await.expect("the stand-in chain reads");
    assert!(unmet(&state, &expected).is_empty(), "{:?}", unmet(&state, &expected));
    assert_eq!(state.registry_staking, DEPLOYED);
    assert!(check_kept(&state, &contracts(DEPLOYED)).is_ok());
    let (_, file) =
        hold(|| read(DEPLOYED), (&expected, &contracts(DEPLOYED)), &path, || async { Ok(()) }).await.unwrap();
    assert_eq!(file.source, built.source);

    // Another contract holding the same token and naming the same registry: the start refuses it at once, naming
    // the registry's.
    let refused =
        hold(|| read(IMPOSTOR), (&expected, &contracts(IMPOSTOR)), &path, || async { Ok(()) }).await.unwrap_err();
    let refused = format!("{refused:#}");
    assert!(refused.contains(&format!("the registry deployed staking contract {DEPLOYED}")), "{refused}");
    assert!(refused.contains(&format!("custody.evm.staking.contract is {IMPOSTOR}")), "{refused}");
}
