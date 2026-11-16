use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{B256, U256};
use axum::extract::State;
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};

use super::*;
use crate::checkpoints::rpc::CandidateReturn;
use crate::checkpoints::{CheckpointChain, LifecycleEvidence, VaultState};
use crate::config::{CheckpointSection, FinalityTag};
use crate::log::CheckpointDomain;
use crate::standby_key::chain::{RevisionReturn, assignmentOfCall, revisionOfReturn};

const VAULT: Address = Address::repeat_byte(0x11);
const REGISTRY: Address = Address::repeat_byte(0x22);
const SCHEDULE: Address = Address::repeat_byte(0x33);
const PINNED: B256 = B256::repeat_byte(0x44);

/// The chain a stand-in node answers for: the registry and schedule the vault and registry name, and
/// the most assignment reads it has had in flight at once.
struct Chain {
    named: Mutex<(Address, Address)>,
    in_flight: AtomicUsize,
    most: AtomicUsize,
}

/// The members of a turn's assignments before its latest, read at the pinned finalized block: turn 8 was
/// drawn for A at position 0, skipped past an unassigned position to B at 2, then to C at 3, its latest,
/// which is not read; turn 7, its draw alone at position 2, is read no further. Each assignment's
/// revision history is longer than the stand-in answers `assignmentOf` for, as an RPC's limits refuse a
/// member's repeated release moves, and the members read are the same. A registry with no schedule, or a
/// vault with no registry, gives none, and an observation finality has not fixed, or one pinned to no
/// block, is refused.
#[tokio::test]
async fn the_members_a_turns_latest_assignment_superseded_are_read_in_the_order_made() {
    let (rpc, chain, server) = serve().await;
    let finalized = observation(View::Finalized, Some(PINNED));
    let members = rpc.superseded_members(&finalized, 8).await.unwrap();
    assert_eq!(members, [registered(0xa1), registered(0xb1)]);
    assert_eq!(rpc.superseded_members(&finalized, 7).await.unwrap(), []);
    *chain.named.lock().unwrap() = (REGISTRY, Address::ZERO);
    assert_eq!(rpc.superseded_members(&finalized, 8).await.unwrap(), [], "a registry with no schedule");
    *chain.named.lock().unwrap() = (Address::ZERO, SCHEDULE);
    assert_eq!(rpc.superseded_members(&finalized, 8).await.unwrap(), [], "a vault with no registry");
    *chain.named.lock().unwrap() = (REGISTRY, SCHEDULE);
    assert!(rpc.superseded_members(&observation(View::Latest, Some(PINNED)), 8).await.is_err());
    assert!(rpc.superseded_members(&observation(View::Finalized, None), 8).await.is_err());
    server.abort();
}

/// A skip to a late position leaves a long assignment history: its positions are read several at a
/// time, and the members still come in the order made, though here the earlier positions, and the
/// earlier member's registration, take longer to answer.
#[tokio::test]
async fn a_late_skips_assignment_history_is_read_several_positions_at_a_time() {
    let (rpc, chain, server) = serve().await;
    let members = rpc.superseded_members(&observation(View::Finalized, Some(PINNED)), 9).await.unwrap();
    assert_eq!(members, [registered(0x91), registered(0x92)]);
    assert!(chain.most.load(Ordering::SeqCst) > 1, "the assignments were read one at a time");
    server.abort();
}

// ── Helpers ──

/// A stand-in node answering for `Chain`, and a reader of the vault on it.
async fn serve() -> (CheckpointRpc, Arc<Chain>, tokio::task::JoinHandle<()>) {
    let chain = Arc::new(Chain {
        named: Mutex::new((REGISTRY, SCHEDULE)),
        in_flight: AtomicUsize::new(0),
        most: AtomicUsize::new(0),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new().route("/", post(answer)).with_state(chain.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cfg: CheckpointSection = toml::from_str(&format!(
        "rpc_url='http://{address}'\nrelayer='0x{}'\nretry_interval='1ms'\nrpc_timeout='2s'",
        "01".repeat(20)
    ))
    .unwrap();
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId(VAULT.0.0) };
    let rpc = CheckpointRpc::read_only(&cfg, domain, Address::ZERO, FinalityTag::Finalized, 0).unwrap();
    (rpc, chain, server)
}

/// Answers the vault's, registry's and schedule's reads at the pinned block, panicking on any other,
/// and refuses every `assignmentOf` as over its response limit; an assignment's member of turn 9 takes
/// 10 ms for each position below 10, and member 0x91's registration 60 ms.
async fn answer(State(chain): State<Arc<Chain>>, Json(request): Json<Value>) -> Json<Value> {
    let (registry, schedule) = *chain.named.lock().unwrap();
    let call = &request["params"][0];
    let input = hex::decode(&call["input"].as_str().or(call["data"].as_str()).unwrap()[2..]).unwrap();
    let to: Address = call["to"].as_str().unwrap().parse().unwrap();
    assert_eq!(request["params"][1]["blockHash"], format!("{PINNED:#x}"), "every read is at the pinned block");
    let result = match (to, <[u8; 4]>::try_from(&input[..4]).unwrap()) {
        (VAULT, registryCall::SELECTOR) => registryCall::abi_encode_returns(&registry),
        (REGISTRY, scheduleCall::SELECTOR) => scheduleCall::abi_encode_returns(&schedule),
        (REGISTRY, candidateOfCall::SELECTOR) => {
            let byte = candidateOfCall::abi_decode(&input).unwrap().candidate.0[0];
            if byte == 0x91 {
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
            candidateOfCall::abi_encode_returns(&CandidateReturn {
                adminKey: Address::repeat_byte(byte + 1),
                enclave: Address::ZERO,
                bond: Default::default(),
                release: B256::repeat_byte(byte),
                registration: 1,
                apiHostname: format!("{byte:x}.test"),
                handoverEndpoint: format!("{byte:x}.test:7443"),
            })
        }
        (SCHEDULE, turnOfCall::SELECTOR) => {
            turnOfCall::abi_encode_returns(&match turnOfCall::abi_decode(&input).unwrap().turn {
                7 => turn(0xe1, 2, DRAWN),
                8 => turn(0xc1, 3, 1),
                9 => turn(0x93, 9, 1),
                other => panic!("turn {other} read"),
            })
        }
        (SCHEDULE, revisionOfCall::SELECTOR) => {
            let asked = revisionOfCall::abi_decode(&input).unwrap();
            assert_eq!(asked.index, U256::ZERO, "a revision past the first was read");
            if asked.turn == 9 {
                let now = chain.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                chain.most.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10 * u64::from(10 - asked.position))).await;
                chain.in_flight.fetch_sub(1, Ordering::SeqCst);
            }
            let member = match (asked.turn, asked.position) {
                (8, 0) => 0xa1,
                (8, 1) => 0,
                (8, 2) => 0xb1,
                (8, 3) | (9, 9) => panic!("the latest assignment was read"),
                (7, _) => panic!("a turn its draw alone holds was read further"),
                (9, 0) => 0x91,
                (9, 6) => 0x92,
                (9, _) => 0,
                other => panic!("assignment {other:?} read"),
            };
            revisionOfCall::abi_encode_returns(&first_revision(member))
        }
        (SCHEDULE, assignmentOfCall::SELECTOR) => {
            let error = json!({"code": -32000, "message": "response size exceeded"});
            return Json(json!({"jsonrpc":"2.0","id":request["id"],"error":error}));
        }
        other => panic!("unexpected read {other:?}"),
    };
    Json(json!({"jsonrpc":"2.0","id":request["id"],"result":format!("0x{}", hex::encode(result))}))
}

/// A turn whose latest assignment names member `byte` at `position` for `reason`.
fn turn(byte: u8, position: u16, reason: u8) -> TurnReturn {
    TurnReturn {
        candidate: Address::repeat_byte(byte),
        position,
        reason,
        setId: 1,
        generation: 7,
        prior: 0,
        seed: B256::ZERO,
        source: Address::ZERO,
    }
}

/// The first revision of an assignment of member `byte`, the zero address and no revision for `0`.
fn first_revision(byte: u8) -> revisionOfReturn {
    let generation = u64::from(byte != 0);
    revisionOfReturn {
        candidate: Address::repeat_byte(byte),
        revision: RevisionReturn { release: B256::repeat_byte(byte), generation },
    }
}

/// Member `byte`'s handover data as `answer` registers it.
fn registered(byte: u8) -> HandoverSource {
    HandoverSource {
        candidate: AccountId([byte; 20]),
        release: [byte; 32],
        api_hostname: format!("{byte:x}.test"),
        handover_endpoint: format!("{byte:x}.test:7443"),
    }
}

/// An observation in `view`, pinned to block `pinned` when there is one.
fn observation(view: View, pinned: Option<B256>) -> Observation {
    let zero = engine_core::Fixed::ZERO;
    Observation {
        view,
        state: VaultState {
            height: 100,
            signer: AccountId([1; 20]),
            checkpoint: None,
            balance: zero,
            deposits: 0,
            deposited: zero,
            distributions_paid: zero,
        },
        lifecycle: pinned.map(|block_hash| LifecycleEvidence {
            height: 100,
            block_hash,
            signer: AccountId([1; 20]),
            signer_epoch: 1,
            withdrawals_paused: false,
            total_withdrawn: zero,
            checkpoint_seq: None,
            checkpoint_root: None,
            approvals: Vec::new(),
        }),
    }
}
