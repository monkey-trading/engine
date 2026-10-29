//! Deterministic signing and canonical-state RPC for submission regressions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use alloy::primitives::{B256, U256};
use alloy::rpc::types::Block;
use alloy::sol_types::SolCall;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::checkpoints::rpc::{
    balanceOfCall, checkpointEpochAtCall, checkpointRootsCall, checkpointSignerAtCall, checkpointSignerCall,
    depositCountCall, depositedThroughCall, distributionsPaidCall, hasCheckpointAtCall, hasCheckpointCall,
    latestRootCall, latestSeqCall, signerAtEpochCall, signerEpochCall, totalDepositedCall, totalWithdrawnCall,
    usdcCall, usedNoncesCall, withdrawalsPausedCall,
};
use crate::log::Checkpoint;
use crate::tee::EnclaveKey;

pub struct RpcState {
    pub chain_id: u64,
    pub latest_nonce: u64,
    pub pending_nonce: u64,
    pub sent: Vec<Value>,
    pub known: bool,
    pub lookup_override: Option<Value>,
    pub lose_response: bool,
    pub block_hash_store: bool,
    pub reject_send: bool,
    pub resurface_on_send: Option<Value>,
    pub signer: alloy::primitives::Address,
    pub epoch: u64,
    pub finalized_epoch: Option<u64>,
    pub epoch_signers: BTreeMap<u64, alloy::primitives::Address>,
    pub paid: U256,
    pub finalized_paid: Option<U256>,
    pub withdrawals_paused: bool,
    pub balance: U256,
    /// The vault's deposit count, and its running deposit total after each deposit id.
    pub deposits: u64,
    pub deposited_through: BTreeMap<u64, U256>,
    pub claimed: BTreeSet<(alloy::primitives::Address, u64)>,
    pub accepted: BTreeMap<u64, (Checkpoint, u64)>,
    pub calls: Vec<(String, B256)>,
    pub finalized_signer: Option<alloy::primitives::Address>,
    pub consumer: Option<Value>,
    pub consumed_at: u64,
    pub orphan: Option<B256>,
    pub fail_method: Option<&'static str>,
    pub wrong_consuming_hash: bool,
    pub latest: Option<Checkpoint>,
    pub finalized: Option<Checkpoint>,
    pub hash: B256,
    path: PathBuf,
}

pub struct Server {
    pub url: String,
    pub state: Arc<Mutex<RpcState>>,
    task: JoinHandle<()>,
}

impl Server {
    /// Starts an isolated local RPC, with seven consumed account nonces and no pending work.
    pub async fn start(dir: &Path) -> Self {
        let state = Arc::new(Mutex::new(RpcState {
            chain_id: 31337,
            latest_nonce: 7,
            pending_nonce: 7,
            sent: Vec::new(),
            known: true,
            lookup_override: None,
            lose_response: false,
            block_hash_store: false,
            reject_send: false,
            resurface_on_send: None,
            signer: alloy::primitives::Address::from(EnclaveKey::test_key().address()),
            finalized_signer: None,
            epoch: 1,
            finalized_epoch: None,
            epoch_signers: BTreeMap::from([(1, alloy::primitives::Address::from(EnclaveKey::test_key().address()))]),
            paid: U256::ZERO,
            finalized_paid: None,
            withdrawals_paused: false,
            balance: U256::ZERO,
            deposits: 0,
            deposited_through: BTreeMap::new(),
            claimed: Default::default(),
            accepted: Default::default(),
            calls: Vec::new(),
            consumer: None,
            consumed_at: 6,
            orphan: None,
            fail_method: None,
            wrong_consuming_hash: false,
            latest: None,
            finalized: None,
            hash: B256::repeat_byte(0x33),
            path: dir.join("checkpoint-submission.json"),
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/", post(answer)).with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, state, task }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Answers only the RPC methods the real adapter needs, checking durable ownership at send time.
async fn answer(State(state): State<Arc<Mutex<RpcState>>>, Json(request): Json<Value>) -> Response {
    let mut state = state.lock().unwrap();
    let params = &request["params"];
    let method = request["method"].as_str().unwrap();
    if state.fail_method == Some(method) {
        return rpc_error(&request, "injected RPC failure");
    }
    let result = match method {
        "eth_chainId" => json!(format!("{:#x}", state.chain_id)),
        "eth_getTransactionCount" => {
            assert_eq!(params.as_array().unwrap().len(), 2);
            assert_eq!(params[0], serde_json::to_value(alloy::primitives::Address::repeat_byte(3)).unwrap());
            let nonce = if params[1].is_object() {
                assert_eq!(params[1]["requireCanonical"], true);
                let hash: B256 = serde_json::from_value(params[1]["blockHash"].clone()).unwrap();
                if state.orphan == Some(hash) {
                    return rpc_error(&request, "header is no longer canonical");
                }
                if hash.as_slice()[0] as u64 >= state.consumed_at && state.consumer.is_some() { 8 } else { 7 }
            } else if params[1] == "pending" {
                state.pending_nonce
            } else {
                assert_eq!(params[1], "latest");
                state.latest_nonce
            };
            json!(format!("{nonce:#x}"))
        }
        "eth_sendTransaction" => {
            let transaction = params[0].clone();
            let record: Value = serde_json::from_slice(&std::fs::read(&state.path).unwrap()).unwrap();
            let nonce =
                u64::from_str_radix(transaction["nonce"].as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
            assert_eq!(record["nonce"], nonce, "nonce ownership must exist before send");
            assert!(record["checkpoint"]["signature"].as_str().unwrap().starts_with("0x"));
            state.sent.push(transaction);
            if state.reject_send {
                return rpc_error(&request, "rejected before acceptance");
            }
            if let Some(consumer) = state.resurface_on_send.take() {
                state.consumer = Some(consumer);
                state.latest_nonce = nonce + 1;
                state.pending_nonce = nonce + 1;
                return rpc_error(&request, "obsolete predecessor consumed nonce before acceptance");
            }
            state.pending_nonce = state.pending_nonce.max(nonce + 1);
            if state.block_hash_store {
                std::fs::create_dir(state.path.with_extension("json.tmp")).unwrap();
            }
            if std::mem::take(&mut state.lose_response) {
                return "".into_response();
            }
            json!(state.hash)
        }
        "eth_getTransactionByHash" => {
            if state.known {
                state.lookup_override.clone().or_else(|| state.sent.last().cloned()).unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        "eth_getBlockByNumber" => {
            assert_eq!(params[1], false);
            assert_eq!(params.as_array().unwrap().len(), 2);
            let number = match params[0].as_str().unwrap() {
                "finalized" => 8,
                "latest" => 10,
                value => u64::from_str_radix(value.trim_start_matches("0x"), 16).unwrap(),
            };
            block(number)
        }
        "eth_getBlockByHash" => {
            assert!(params[1].is_boolean());
            assert_eq!(params.as_array().unwrap().len(), 2);
            let hash: B256 = serde_json::from_value(params[0].clone()).unwrap();
            let number = hash.as_slice()[0] as u64;
            let mut result = block(number);
            if params[1] == true {
                let transactions = state
                    .consumer
                    .iter()
                    .cloned()
                    .map(|mut tx| {
                        tx.as_object_mut().unwrap().entry("value").or_insert(json!("0x0"));
                        tx
                    })
                    .collect::<Vec<_>>();
                result["transactions"] = json!(transactions);
                if state.wrong_consuming_hash {
                    result["hash"] = json!(B256::repeat_byte(0xff));
                }
            }
            result
        }
        "eth_call" => {
            assert_eq!(params.as_array().unwrap().len(), 2);
            assert_eq!(params[1]["requireCanonical"], true);
            assert!(params[1]["blockHash"].as_str().is_some());
            let hash: B256 = serde_json::from_value(params[1]["blockHash"].clone()).unwrap();
            if state.orphan == Some(hash) {
                return rpc_error(&request, "header is no longer canonical");
            }
            let finalized = params[1]["blockHash"] == json!(B256::repeat_byte(8));
            let input = params[0].get("input").or_else(|| params[0].get("data")).unwrap().as_str().unwrap();
            let selector = &input[2..10];
            state.calls.push((selector.to_owned(), hash));
            let checkpoint = if finalized { &state.finalized } else { &state.latest };
            let bytes = if selector == hex::encode(usdcCall::SELECTOR) {
                usdcCall::abi_encode_returns(&alloy::primitives::Address::repeat_byte(2))
            } else if selector == hex::encode(checkpointSignerCall::SELECTOR) {
                let signer = if finalized { state.finalized_signer.unwrap_or(state.signer) } else { state.signer };
                checkpointSignerCall::abi_encode_returns(&signer)
            } else if selector == hex::encode(signerEpochCall::SELECTOR) {
                signerEpochCall::abi_encode_returns(&if finalized {
                    state.finalized_epoch.unwrap_or(state.epoch)
                } else {
                    state.epoch
                })
            } else if selector == hex::encode(signerAtEpochCall::SELECTOR) {
                let epoch = signerAtEpochCall::abi_decode(&hex::decode(&input[2..]).unwrap()).unwrap().epoch;
                signerAtEpochCall::abi_encode_returns(
                    &state.epoch_signers.get(&epoch).copied().unwrap_or(alloy::primitives::Address::ZERO),
                )
            } else if selector == hex::encode(totalWithdrawnCall::SELECTOR) {
                totalWithdrawnCall::abi_encode_returns(&if finalized {
                    state.finalized_paid.unwrap_or(state.paid)
                } else {
                    state.paid
                })
            } else if selector == hex::encode(usedNoncesCall::SELECTOR) {
                {
                    let query = usedNoncesCall::abi_decode(&hex::decode(&input[2..]).unwrap()).unwrap();
                    usedNoncesCall::abi_encode_returns(&state.claimed.contains(&(query.account, query.nonce)))
                }
            } else if selector == hex::encode(hasCheckpointAtCall::SELECTOR) {
                let seq = hasCheckpointAtCall::abi_decode(&hex::decode(&input[2..]).unwrap()).unwrap().seq;
                hasCheckpointAtCall::abi_encode_returns(
                    &(state.accepted.contains_key(&seq) || checkpoint.as_ref().is_some_and(|cp| cp.seq == seq)),
                )
            } else if selector == hex::encode(checkpointRootsCall::SELECTOR) {
                let seq = checkpointRootsCall::abi_decode(&hex::decode(&input[2..]).unwrap()).unwrap().seq;
                let cp = state.accepted.get(&seq).map(|(cp, _)| cp).or(checkpoint.as_ref()).unwrap();
                checkpointRootsCall::abi_encode_returns(&cp.root.parse().unwrap())
            } else if selector == hex::encode(checkpointSignerAtCall::SELECTOR) {
                let seq = checkpointSignerAtCall::abi_decode(&hex::decode(&input[2..]).unwrap()).unwrap().seq;
                let cp = state.accepted.get(&seq).map(|(cp, _)| cp).or(checkpoint.as_ref()).unwrap();
                checkpointSignerAtCall::abi_encode_returns(&alloy::primitives::Address::from(
                    cp.signer.parse::<engine_core::types::AccountId>().unwrap().0,
                ))
            } else if selector == hex::encode(checkpointEpochAtCall::SELECTOR) {
                let seq = checkpointEpochAtCall::abi_decode(&hex::decode(&input[2..]).unwrap()).unwrap().seq;
                checkpointEpochAtCall::abi_encode_returns(&state.accepted.get(&seq).map_or(1, |(_, epoch)| *epoch))
            } else if selector == hex::encode(hasCheckpointCall::SELECTOR) {
                hasCheckpointCall::abi_encode_returns(&checkpoint.is_some())
            } else if selector == hex::encode(latestSeqCall::SELECTOR) {
                latestSeqCall::abi_encode_returns(&checkpoint.as_ref().unwrap().seq)
            } else if selector == hex::encode(latestRootCall::SELECTOR) {
                latestRootCall::abi_encode_returns(&checkpoint.as_ref().unwrap().root.parse().unwrap())
            } else if selector == hex::encode(balanceOfCall::SELECTOR) {
                balanceOfCall::abi_encode_returns(&state.balance)
            } else if selector == hex::encode(totalDepositedCall::SELECTOR) {
                // The vault holds exactly its deposits less its payments.
                let paid = if finalized { state.finalized_paid.unwrap_or(state.paid) } else { state.paid };
                totalDepositedCall::abi_encode_returns(&(state.balance + paid))
            } else if selector == hex::encode(depositedThroughCall::SELECTOR) {
                let id = depositedThroughCall::abi_decode(&hex::decode(&input[2..]).unwrap()).unwrap().id;
                depositedThroughCall::abi_encode_returns(
                    &state.deposited_through.get(&id.to::<u64>()).copied().unwrap_or(U256::ZERO),
                )
            } else if selector == hex::encode(distributionsPaidCall::SELECTOR) {
                distributionsPaidCall::abi_encode_returns(&U256::ZERO)
            } else if selector == hex::encode(withdrawalsPausedCall::SELECTOR) {
                withdrawalsPausedCall::abi_encode_returns(&state.withdrawals_paused)
            } else {
                assert_eq!(selector, hex::encode(depositCountCall::SELECTOR));
                depositCountCall::abi_encode_returns(&U256::from(state.deposits))
            };
            json!(format!("0x{}", hex::encode(bytes)))
        }
        other => panic!("unexpected RPC method {other}"),
    };
    Json(json!({"jsonrpc": "2.0", "id": request["id"], "result": result})).into_response()
}

/// Returns a canonical block on the fixture's immutable hash-linked chain.
fn block(number: u64) -> Value {
    let mut block: Block = Block::default();
    block.header.number = number;
    block.header.hash = B256::repeat_byte(number as u8);
    block.header.parent_hash = B256::repeat_byte(number.saturating_sub(1) as u8);
    serde_json::to_value(block).unwrap()
}

/// Returns an ordinary JSON-RPC refusal without changing the fixture's chain state.
fn rpc_error(request: &Value, message: &str) -> Response {
    Json(json!({"jsonrpc":"2.0", "id":request["id"], "error":{"code":-32000,"message":message}})).into_response()
}
