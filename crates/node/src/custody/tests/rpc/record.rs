//! The schedule's governance record read: a schedule that reverts its getter keeps no record, one with no
//! code or whose record names no vote contract names none yet, and any other failure is an error, never
//! a missing record.
use super::*;
use crate::custody::evm::GovernanceRecord;

use axum::extract::State;
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};

const SCHEDULE: Address = Address::repeat_byte(0x7c);
const RECORD: Address = Address::repeat_byte(0x7e);
const VOTE: Address = Address::repeat_byte(0x7d);
const FAULT_PROOF: Address = Address::repeat_byte(0x7f);

alloy::sol! {
    function governanceRecord() external view returns (address);
    function vote() external view returns (address);
    function faultProof() external view returns (address);
}

/// What the endpoint answers the schedule's `governanceRecord()` with, and the vote contract the record
/// names.
#[derive(Clone)]
struct Answers {
    schedule: Value,
    vote: Address,
}

#[tokio::test]
async fn a_schedule_that_reverts_the_getter_keeps_no_record() {
    let revert = json!({ "error": { "code": -32000, "message": "execution reverted" } });
    assert_eq!(read(revert, VOTE).await.unwrap(), GovernanceRecord::Absent);
    let revert = json!({ "error": { "code": 3, "message": "execution reverted", "data": "0x" } });
    assert_eq!(read(revert, VOTE).await.unwrap(), GovernanceRecord::Absent);
}

#[tokio::test]
async fn a_schedule_with_no_code_or_a_record_naming_no_vote_contract_names_none_yet() {
    assert_eq!(read(json!({ "result": "0x" }), VOTE).await.unwrap(), GovernanceRecord::Unset);
    assert_eq!(read(recorded(), Address::ZERO).await.unwrap(), GovernanceRecord::Unset);
}

#[tokio::test]
async fn a_filled_record_names_its_vote_and_fault_proof_contracts() {
    assert_eq!(read(recorded(), VOTE).await.unwrap(), GovernanceRecord::Set { vote: VOTE, fault_proof: FAULT_PROOF });
}

#[tokio::test]
async fn an_endpoint_failure_is_an_error_not_a_missing_record() {
    let failure = json!({ "error": { "code": -32000, "message": "header not found" } });
    let error = read(failure, VOTE).await.unwrap_err();
    assert!(format!("{error:#}").contains("reading the schedule's governance record"), "{error:#}");
}

/// The schedule's answer naming the record.
fn recorded() -> Value {
    json!({ "result": Bytes::from(governanceRecordCall::abi_encode_returns(&RECORD)) })
}

/// The record a reader configured with the schedule reads from an endpoint answering as `schedule` and
/// `vote` say.
async fn read(schedule: Value, vote: Address) -> anyhow::Result<GovernanceRecord> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().route("/", post(answer)).with_state(Answers { schedule, vote });
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut cfg: EvmCustodySection = toml::from_str(
        &evm_toml(&std::env::temp_dir(), 1_000)
            .replace("rpc_url = \"http://127.0.0.1:1\"", &format!("rpc_url = \"{url}\"")),
    )
    .unwrap();
    let section = format!(
        "contract = \"{}\"\ncursor_path = \"unused-staking-cursor\"\n[registry]\ncontract = \"{}\"\nschedule = \"{SCHEDULE}\"",
        Address::repeat_byte(0x5a),
        Address::repeat_byte(0x6b)
    );
    cfg.staking = Some(toml::from_str(&section).unwrap());
    let rpc = VaultRpc::new(&cfg, &cfg.providers[0].rpc_url, VAULT).unwrap();
    let record = rpc.governance_record(BlockRef { number: 7, hash: B256::repeat_byte(7) }).await;
    server.abort();
    record
}

/// Answers an `eth_call` to the schedule with its scripted answer and to the record by selector.
async fn answer(State(answers): State<Answers>, Json(request): Json<Value>) -> Json<Value> {
    assert_eq!(request["method"], "eth_call");
    let call = &request["params"][0];
    let to: Address = call["to"].as_str().unwrap().parse().unwrap();
    let input: Bytes = call["input"].as_str().or(call["data"].as_str()).unwrap().parse().unwrap();
    let mut reply = match to {
        SCHEDULE => answers.schedule.clone(),
        RECORD if input[..4] == voteCall::SELECTOR => {
            json!({ "result": Bytes::from(voteCall::abi_encode_returns(&answers.vote)) })
        }
        RECORD if input[..4] == faultProofCall::SELECTOR => {
            json!({ "result": Bytes::from(faultProofCall::abi_encode_returns(&FAULT_PROOF)) })
        }
        other => panic!("an unexpected call to {other}"),
    };
    reply["jsonrpc"] = json!("2.0");
    reply["id"] = request["id"].clone();
    Json(reply)
}
