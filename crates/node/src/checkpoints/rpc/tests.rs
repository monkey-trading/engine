//! A tag from one fork cannot lend confirmation depth to a replacement fork.

use std::sync::{Arc, Mutex};

use super::*;
use alloy::primitives::B256;
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

#[tokio::test]
async fn the_registry_operator_is_read_at_the_pinned_finalized_block_only_while_rotation_is_off() {
    const VAULT: Address = Address::repeat_byte(0x11);
    const REGISTRY: Address = Address::repeat_byte(0x22);
    const PINNED: B256 = B256::repeat_byte(0x33);
    /// The registry the vault names, and whether its rotation is on.
    type Registry = Arc<Mutex<(Address, bool)>>;
    async fn answer(State(registry): State<Registry>, Json(request): Json<Value>) -> Json<Value> {
        let (named, rotation) = *registry.lock().unwrap();
        let call = &request["params"][0];
        let input = call["input"].as_str().or(call["data"].as_str()).unwrap();
        let selector: [u8; 4] = hex::decode(&input[2..10]).unwrap().try_into().unwrap();
        let to: Address = call["to"].as_str().unwrap().parse().unwrap();
        // Every read is at the observation's block, by hash.
        if request["params"][1]["blockHash"] != format!("{PINNED:#x}") {
            return Json(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"unpinned read"}}));
        }
        let word = |bytes: &[u8]| format!("{:0>64}", hex::encode(bytes));
        let result = match (to, selector) {
            (VAULT, registryCall::SELECTOR) => word(named.as_slice()),
            (REGISTRY, rotationEnabledCall::SELECTOR) => word(&[u8::from(rotation)]),
            (REGISTRY, operatorInEffectCall::SELECTOR) => {
                [word(&[0xc2; 20]), word(&[0xa2; 20]), word(&[0x44; 32]), word(&1_793_000_000u64.to_be_bytes())]
                    .concat()
            }
            other => panic!("unexpected read {other:?}"),
        };
        Json(json!({"jsonrpc":"2.0","id":request["id"],"result":format!("0x{result}")}))
    }
    let registry: Registry = Arc::new(Mutex::new((REGISTRY, false)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route("/", post(answer)).with_state(registry.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg: CheckpointSection = toml::from_str(&format!(
        "rpc_url='http://{address}'\nrelayer='0x{}'\nretry_interval='1ms'\nrpc_timeout='2s'",
        "01".repeat(20)
    ))
    .unwrap();
    let domain = CheckpointDomain { chain_id: 31337, vault: engine_core::types::AccountId(VAULT.0.0) };
    let rpc = CheckpointRpc::read_only(&cfg, domain, Address::ZERO, FinalityTag::Finalized, 0).unwrap();
    let observation = |view| Observation {
        view,
        state: VaultState {
            height: 100,
            signer: engine_core::types::AccountId([1; 20]),
            checkpoint: None,
            balance: engine_core::Fixed::ZERO,
            deposits: 0,
            deposited: engine_core::Fixed::ZERO,
            distributions_paid: engine_core::Fixed::ZERO,
        },
        lifecycle: Some(LifecycleEvidence {
            height: 100,
            block_hash: PINNED,
            signer: engine_core::types::AccountId([1; 20]),
            signer_epoch: 1,
            withdrawals_paused: false,
            total_withdrawn: engine_core::Fixed::ZERO,
            checkpoint_seq: None,
            checkpoint_root: None,
            approvals: Vec::new(),
        }),
    };
    let finalized = observation(View::Finalized);
    assert_eq!(
        rpc.registry_operator_at(&finalized).await.unwrap(),
        Some(RegistryOperator { admin_key: engine_core::types::AccountId([0xa2; 20]), since: 1_793_000_000_000 })
    );
    // While rotation is on, elections move the key; a vault with no registry has none to name.
    *registry.lock().unwrap() = (REGISTRY, true);
    assert_eq!(rpc.registry_operator_at(&finalized).await.unwrap(), None);
    *registry.lock().unwrap() = (Address::ZERO, false);
    assert_eq!(rpc.registry_operator_at(&finalized).await.unwrap(), None);
    // A view finality has not fixed, or one pinned to no block, is refused before any read.
    *registry.lock().unwrap() = (REGISTRY, false);
    assert!(rpc.registry_operator_at(&observation(View::Latest)).await.is_err());
    let unpinned = Observation { lifecycle: None, ..finalized };
    assert!(rpc.registry_operator_at(&unpinned).await.is_err());
    server.abort();
}

/// The schedule's reassignments over a range of turns, read at the latest block: a turn its draw alone
/// holds is read no further; one a promotion or a skip followed gives each of its assignments, the
/// draw's, its first, marked as none, and skips the positions it never assigned. Each assignment's
/// revision history is longer than the stand-in answers `assignmentOf` for, as an RPC's limits refuse a
/// member's repeated release moves, and the assignments read are the same. A registry with no schedule
/// gives none, and a vault with no registry cannot answer.
#[tokio::test]
async fn reassignments_name_each_assignment_of_the_turns_a_promotion_or_skip_followed() {
    use crate::checkpoints::Assigned;
    use crate::checkpoints::rpc::schedule::{TurnReturn, scheduleCall, turnOfCall};
    use crate::standby_key::chain::{RevisionReturn, assignmentOfCall, revisionOfCall, revisionOfReturn};
    const VAULT: Address = Address::repeat_byte(0x11);
    const REGISTRY: Address = Address::repeat_byte(0x22);
    const SCHEDULE: Address = Address::repeat_byte(0x33);
    let member = |byte: u8| Address::repeat_byte(byte);
    /// The registry and schedule the vault and registry name.
    type Named = Arc<Mutex<(Address, Address)>>;
    async fn answer(State(named): State<Named>, Json(request): Json<Value>) -> Json<Value> {
        let (registry, schedule) = *named.lock().unwrap();
        let call = &request["params"][0];
        let input = hex::decode(&call["input"].as_str().or(call["data"].as_str()).unwrap()[2..]).unwrap();
        let to: Address = call["to"].as_str().unwrap().parse().unwrap();
        assert_eq!(request["params"][1], "latest", "a reassignment is read at the latest block");
        let turn = |candidate, position, reason| TurnReturn {
            candidate,
            position,
            reason,
            setId: 1,
            generation: 7,
            prior: 0,
            seed: B256::ZERO,
            source: Address::ZERO,
        };
        let assigned = |candidate| revisionOfReturn {
            candidate,
            revision: RevisionReturn { release: B256::repeat_byte(1), generation: 7 },
        };
        let result = match (to, <[u8; 4]>::try_from(&input[..4]).unwrap()) {
            (VAULT, registryCall::SELECTOR) => registryCall::abi_encode_returns(&registry),
            (REGISTRY, scheduleCall::SELECTOR) => scheduleCall::abi_encode_returns(&schedule),
            (SCHEDULE, turnOfCall::SELECTOR) => {
                turnOfCall::abi_encode_returns(&match turnOfCall::abi_decode(&input).unwrap().turn {
                    7 => turn(Address::repeat_byte(0xe1), 0, 0),
                    8 => turn(Address::repeat_byte(0xb1), 3, 2),
                    9 => turn(Address::repeat_byte(0xd1), 2, 1),
                    other => panic!("turn {other} read"),
                })
            }
            (SCHEDULE, revisionOfCall::SELECTOR) => {
                let asked = revisionOfCall::abi_decode(&input).unwrap();
                assert_eq!(asked.index, U256::ZERO, "a revision past the first was read");
                let at = match (asked.turn, asked.position) {
                    (8, 1) => assigned(Address::repeat_byte(0xa1)),
                    (8, 3) => assigned(Address::repeat_byte(0xb1)),
                    (9, 0) => assigned(Address::repeat_byte(0xc1)),
                    (9, 2) => assigned(Address::repeat_byte(0xd1)),
                    (7, _) => panic!("a turn its draw alone holds was read further"),
                    _ => assigned(Address::ZERO),
                };
                revisionOfCall::abi_encode_returns(&at)
            }
            (SCHEDULE, assignmentOfCall::SELECTOR) => {
                let error = json!({"code": -32000, "message": "response size exceeded"});
                return Json(json!({"jsonrpc":"2.0","id":request["id"],"error":error}));
            }
            other => panic!("unexpected read {other:?}"),
        };
        Json(json!({"jsonrpc":"2.0","id":request["id"],"result":format!("0x{}", hex::encode(result))}))
    }
    let named: Named = Arc::new(Mutex::new((REGISTRY, SCHEDULE)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route("/", post(answer)).with_state(named.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg: CheckpointSection = toml::from_str(&format!(
        "rpc_url='http://{address}'\nrelayer='0x{}'\nretry_interval='1ms'\nrpc_timeout='2s'",
        "01".repeat(20)
    ))
    .unwrap();
    let domain = CheckpointDomain { chain_id: 31337, vault: engine_core::types::AccountId(VAULT.0.0) };
    let rpc = CheckpointRpc::read_only(&cfg, domain, Address::ZERO, FinalityTag::Finalized, 0).unwrap();
    let at = |turn, position, byte, reassigned| Assigned {
        turn,
        position,
        candidate: engine_core::types::AccountId(member(byte).0.0),
        reassigned,
    };
    let expected = [at(8, 1, 0xa1, false), at(8, 3, 0xb1, true), at(9, 0, 0xc1, false), at(9, 2, 0xd1, true)];
    assert_eq!(rpc.reassignments(7..10).await.unwrap(), expected);
    *named.lock().unwrap() = (REGISTRY, Address::ZERO);
    assert_eq!(rpc.reassignments(7..10).await.unwrap(), [], "a registry with no schedule reassigned none");
    *named.lock().unwrap() = (Address::ZERO, SCHEDULE);
    assert!(rpc.reassignments(7..10).await.is_err(), "a vault with no registry answered");
    server.abort();
}
