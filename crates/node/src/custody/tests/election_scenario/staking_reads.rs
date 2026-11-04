//! The staking reads dev scenario on a local Anvil: forty candidates register under a cap of 100 and
//! the timelock approves thirty-five, whom stakers delegate to; the node lists the approved ones
//! through the registry lens at a final block, with the votes the chain shows; three distribution
//! epochs close and `GET /v1/monkey/distributions` lists them; stakers delegate again, the list
//! follows, and a restart of the node keeps the epoch history. Every answer is verified as
//! `engine verify-response` does, and saved for the script that runs the command itself when
//! `STAKING_READS_ANSWERS` names a directory.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::sol_types::SolCall;
use axum::http::StatusCode;
use engine_core::event::Event;
use engine_core::staking::UsdcUnits;
use engine_core::types::{AccountId, Millis, Side};
use k256::ecdsa::SigningKey;
use serde_json::Value;

use super::chain::{Chain, delegateCall, setParametersCall, votesCall};
use super::node::Node;
use super::operator_share::{FLOOR_UNITS, TRADER_KEY, allocated_of, closed_epoch, declare, next_epoch, round_trip};
use super::{DIGEST_A, address, dev_parameters, monkey};
use crate::tee::EnclaveKey;
use crate::verify::{Expected, SentRequest, verify_response_files};

/// Candidates registered, and approved by the timelock, under a cap raised to the registry's limit.
const REGISTERED: usize = 40;
const APPROVED: usize = 35;
/// The chain the scenario runs on, which the node signs its answers for.
const CHAIN_ID: u64 = 31337;
/// What each candidate stakes, and bonds of it.
const STAKE: u64 = 2_000;
const BOND: u64 = 1_000;

#[tokio::test]
#[ignore = "requires Anvil, Python 3, and compiled contracts; scripts/dev-staking-reads.sh runs it"]
async fn dev_scenario_the_operator_list_and_distribution_history_are_signed_reads() {
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_ansi(false).with_writer(std::io::stderr).finish(),
    );
    let deployer = SigningKey::from_slice(&[0x42; 32]).unwrap();
    let trader_key = SigningKey::from_slice(&hex::decode(TRADER_KEY).unwrap()).unwrap();
    let trader = address(&trader_key);
    let enclave = Arc::new(EnclaveKey::generate());
    let mut chain = Chain::start(Address::from(enclave.address()), address(&deployer)).await;
    chain.deploy_lens().await;
    let signer = AccountId(enclave.address());
    let mut node = Node::start_posting_checkpoints(&chain, enclave).await;
    let mut answers = Answers::new(signer, AccountId(chain.vault.into()));

    // 1. The house holds the floor and a client a million, both final.
    chain.fund(None, U256::from(FLOOR_UNITS)).await;
    chain.fund(Some(trader), U256::from(FLOOR_UNITS)).await;
    declare(&node, &trader_key).await;
    let client = AccountId(trader.into());
    node.wait("the deposits to credit and finalise", move |e| {
        e.house_cash() == engine_core::Fixed::from_int(1_000_000)
            && e.accounts().get(&client).is_some_and(|account| {
                account.balance == engine_core::Fixed::from_int(1_000_000)
                    && account.unfinalised_deposits == engine_core::Fixed::ZERO
            })
    })
    .await;

    // 2. The cap rises to 100, forty candidates stake and register, the timelock approves the first
    //    thirty-five, and every candidate delegates: the approved ones spread their stake over each
    //    other, the five unapproved all to the first.
    let (parameters, ..) = dev_parameters();
    let lifted = super::chain::Parameters { maxCandidates: 100, ..parameters };
    chain.govern(vec![setParametersCall { parameters: lifted }.abi_encode()]).await;
    let mut candidates = Vec::new();
    for index in 0..REGISTERED {
        let account = chain.impersonated(&format!("candidate-{index}")).await;
        let admin = SigningKey::from_slice(&[0x60 + u8::try_from(index).unwrap(); 32]).unwrap();
        chain.stake(account, monkey(STAKE)).await;
        chain.register(account, &admin, monkey(BOND), DIGEST_A).await;
        candidates.push((account, address(&admin)));
    }
    let approvals = candidates[..APPROVED].iter().map(|(account, _)| chain.approval(*account, DIGEST_A)).collect();
    chain.govern(approvals).await;
    for (index, (account, _)) in candidates.iter().enumerate() {
        let to = if index < APPROVED { candidates[(index * 3) % APPROVED].0 } else { candidates[0].0 };
        chain.send(*account, chain.staking, delegateCall { candidate: to }.abi_encode()).await;
    }
    let staked = monkey(STAKE * REGISTERED as u64);
    node.wait("every stake to mirror", move |e| {
        e.staked_balances().values().map(|staked| staked.0).sum::<u128>() == u128::try_from(staked).unwrap()
    })
    .await;

    // 3. The list holds the thirty-five approved candidates in registration order with the votes the
    //    chain shows, at a final block the answer names.
    let head: alloy::primitives::U64 = chain.provider.request("eth_blockNumber", ()).await.unwrap();
    let list = wait_for_list(&node, &mut answers, |list| list["block"]["number"].as_u64() >= Some(head.to())).await;
    check_list(&chain, &candidates, &list, staked).await;
    assert_eq!(list["rotation_enabled"], false);
    assert_eq!(list["parameters"]["max_candidates"], 100);
    assert_eq!(list["pending_election"], Value::Null);
    assert_eq!(list["lens"], format!("{:#x}", chain.lens.unwrap()));

    // 4. Three distribution epochs close, each after the client loses to the house.
    let trader_key = &trader_key;
    let mut paid: BTreeMap<AccountId, u128> = BTreeMap::new();
    let mut closes = Vec::new();
    for _ in 0..3 {
        round_trip(&node, trader_key, Side::Buy, "3", "100000", "99700").await;
        let events = next_epoch(&mut node).await;
        assert!(allocated_of(&events).0 > 0, "{events:?}");
        for event in &events {
            if let Event::DistributionAllocated { account, amount, .. } = event {
                *paid.entry(*account).or_default() += amount.0;
            }
        }
        closes.push(events);
    }
    let epochs = node.get_signed_json(&mut answers, "/v1/monkey/distributions").await;
    let listed: Vec<u64> = epochs["epochs"].as_array().unwrap().iter().map(|e| e["epoch"].as_u64().unwrap()).collect();
    assert_eq!(listed, [2, 1, 0], "{epochs}");
    for (listed, events) in epochs["epochs"].as_array().unwrap().iter().zip(closes.iter().rev()) {
        assert_eq!(listed["epoch"], closed_epoch(events));
        assert_eq!(listed["allocated_usdc_units"], allocated_of(events).to_string());
        let stakers: u128 = events
            .iter()
            .map(|e| match e {
                Event::DistributionAllocated { amount, .. } => amount.0,
                _ => 0,
            })
            .sum();
        assert_eq!(listed["stakers_usdc_units"], stakers.to_string());
        assert_eq!(listed["stakers"], REGISTERED);
        assert_eq!(listed["operator_shares"], serde_json::json!([]));
        assert_eq!(listed["skipped"], Value::Null);
    }
    assert_eq!(epochs["from_genesis"], true);
    // Paging: two at a time, newest first, continuing from `next`.
    let first = node.get_signed_json(&mut answers, "/v1/monkey/distributions?limit=2").await;
    assert_eq!((first["epochs"].as_array().unwrap().len(), first["next"].clone()), (2, 1.into()));
    let second = node.get_signed_json(&mut answers, "/v1/monkey/distributions?limit=2&before=1").await;
    assert_eq!((second["epochs"][0]["epoch"].clone(), second["next"].clone()), (0.into(), Value::Null));

    // 5. What each staker was allocated across the epochs, summed from its `distribution_allocated`
    //    history, is its entitlement in the engine and in the proof the vault pays against.
    assert_eq!(paid.len(), REGISTERED);
    for (account, total) in &paid {
        let account = *account;
        assert_eq!(node.query(move |e| e.distribution(account)).await, UsdcUnits(*total));
        assert_eq!(history_sum(&node, account).await, *total, "{account}'s history");
        let proof = super::operator_share::posted_proof(&node, account, UsdcUnits(*total)).await;
        assert_eq!(proof.entitled.0, *total);
    }

    // 6. Ten stakers delegate again, to the second candidate: a later answer shows the new votes.
    for (account, _) in &candidates[10..20] {
        chain.send(*account, chain.staking, delegateCall { candidate: candidates[1].0 }.abi_encode()).await;
    }
    let target = chain.view(chain.staking, votesCall { candidate: candidates[1].0 }).await;
    let moved = wait_for_list(&node, &mut answers, |list| list["candidates"][1]["votes"] == target.to_string()).await;
    check_list(&chain, &candidates, &moved, staked).await;
    assert!(moved["block"]["number"].as_u64() > list["block"]["number"].as_u64());
    assert_eq!(moved["candidates"][1]["eligible"], true, "the new votes pass the eligibility share");

    // 7. The node restarts: the epoch list is rebuilt from the log and the operator list read again.
    let before = epochs["epochs"].clone();
    let node = node.restart(&chain).await;
    let after = node.get_signed_json(&mut answers, "/v1/monkey/distributions").await;
    assert_eq!(after["epochs"], before, "the history survives the restart");
    assert_eq!(after["from_genesis"], true);
    wait_for_list(&node, &mut answers, |list| list["candidates"][1]["votes"] == target.to_string()).await;
    let (status, _) = node.get("/v1/operators").await;
    assert_eq!(status, StatusCode::OK);
    node.stop().await;
    answers.finish();
}

/// The answers a scenario saves for `engine verify-response`.
struct Answers {
    n: u32,
    dir: Option<PathBuf>,
    /// The deployment the node signs for: its enclave address, and the vault on the scenario's chain.
    signer: AccountId,
    vault: AccountId,
}

impl Answers {
    fn new(signer: AccountId, vault: AccountId) -> Self {
        let dir = std::env::var_os("STAKING_READS_ANSWERS").map(PathBuf::from);
        if let Some(dir) = &dir {
            std::fs::create_dir_all(dir).unwrap();
        }
        Self { n: 0, dir, signer, vault }
    }

    /// Writes what `engine verify-response` needs about the deployment.
    fn finish(&self) {
        let Some(dir) = &self.dir else { return };
        std::fs::write(dir.join("deployment"), format!("{} {CHAIN_ID} {}\n", self.signer, self.vault)).unwrap();
    }
}

impl Node {
    /// GETs `path` signed under a fresh challenge, verifies the answer as `engine verify-response`
    /// does, saves it, and returns its body.
    async fn get_signed_json(&self, answers: &mut Answers, path: &str) -> Value {
        answers.n += 1;
        let challenge = format!("{:064x}", answers.n);
        let (status, headers, body) = self.get_signed(path, &challenge).await;
        assert_eq!(status, StatusCode::OK, "{path}: {}", String::from_utf8_lossy(&body));
        let mut saved = format!("HTTP/1.1 {status}\r\n");
        for (name, value) in &headers {
            saved.push_str(&format!("{name}: {}\r\n", value.to_str().unwrap()));
        }
        saved.push_str("\r\n");
        let dir = answers.dir.clone().unwrap_or_else(|| crate::test_support::scratch_dir("staking-reads-answers"));
        let (header_file, body_file) =
            (dir.join(format!("{}.headers", answers.n)), dir.join(format!("{}.body", answers.n)));
        std::fs::write(&header_file, saved).unwrap();
        std::fs::write(&body_file, &body).unwrap();
        let expected = Expected {
            signer: answers.signer,
            chain_id: CHAIN_ID,
            vault: answers.vault,
            challenge: crate::verify::parse_challenge_arg(&challenge).unwrap(),
        };
        let request = SentRequest { method: "GET", path_and_query: path, body: &[] };
        verify_response_files(&expected, &request, &header_file, &body_file).expect("the answer verifies");
        if answers.dir.is_some() {
            let index = std::fs::read_to_string(dir.join("answers")).unwrap_or_default();
            std::fs::write(dir.join("answers"), format!("{index}{}\t{challenge}\t{path}\n", answers.n)).unwrap();
        }
        serde_json::from_slice(&body).unwrap()
    }
}

/// Reads `/v1/operators` until `done` holds, verifying every answer it takes.
async fn wait_for_list(node: &Node, answers: &mut Answers, done: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..200 {
        let (status, ..) = node.get("/v1/operators").await;
        if status == StatusCode::OK {
            let list = node.get_signed_json(answers, "/v1/operators").await;
            if done(&list) {
                return list;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the operator list never reached the expected state");
}

/// The list is the approved candidates, by registration, with the chain's votes, and names a block.
async fn check_list(chain: &Chain, candidates: &[(Address, Address)], list: &Value, staked: U256) {
    let rows = list["candidates"].as_array().unwrap();
    assert_eq!(rows.len(), APPROVED, "only the approved candidates are listed");
    for (index, row) in rows.iter().enumerate() {
        let (account, admin) = candidates[index];
        assert_eq!(row["address"], format!("{account:#x}"));
        assert_eq!(row["admin_key"], format!("{admin:#x}"));
        assert_eq!(row["registration"], index);
        assert_eq!(row["self_bond"], monkey(BOND).to_string());
        assert_eq!(row["role"], "candidate");
        let votes = chain.view(chain.staking, votesCall { candidate: account }).await;
        assert_eq!(row["votes"], votes.to_string(), "candidate {index}'s votes are the chain's");
    }
    assert_eq!(list["total_staked"], staked.to_string());
    let number = list["block"]["number"].as_u64().unwrap();
    let block: Value = chain.provider.request("eth_getBlockByNumber", (format!("{number:#x}"), false)).await.unwrap();
    assert_eq!(list["block"]["hash"], block["hash"]);
    let time = u64::from_str_radix(block["timestamp"].as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
    assert_eq!(list["block"]["time_ms"].as_i64(), Some(time as Millis * 1000));
}

/// The sum of `account`'s `distribution_allocated` events in its history, once the index has them.
async fn history_sum(node: &Node, account: AccountId) -> u128 {
    let path = format!("/v1/accounts/{account}/history?kind=distribution_allocated&limit=200");
    for _ in 0..200 {
        let (status, page) = node.get(&path).await;
        if status == StatusCode::OK {
            assert!(page["next"].is_null(), "a staker has few enough allocations for one page");
            return page["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["amount"].as_str().unwrap().parse::<u128>().unwrap())
                .sum();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the history index never caught up");
}
