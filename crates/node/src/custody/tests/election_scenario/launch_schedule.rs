//! The rotation contracts' launch dev scenario on a local Anvil, which `scripts/dev-launch-values.sh`
//! runs: beside the vault and the registry, the log heads, the seed receiver, the operator schedule, the
//! parameter vote and the fault proof deploy with the values `ops/runbooks/production-deployment.toml`
//! holds, and the timelock names the schedule in the registry. The node then reads them back: the
//! liveness rule's read through the custody quorum finds the launch heartbeat rule, which the node does
//! not refuse at the production template's ticker, and the node's custody watcher mirrors past the
//! deployment, each window having found the registry naming the configured schedule, the schedule naming
//! the registry with this build's turn length, the vote contract naming the schedule and the fault proof
//! naming the vault.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, U64};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use engine_core::operator_faults::Penalty;
use engine_core::types::Millis;
use k256::ecdsa::SigningKey;

use super::chain::{Chain, Parameters, setParametersCall};
use super::node::{Node, custody_section};
use super::{address, completed_through, dev_parameters};
use crate::checkpoints::scenario::deploy;
use crate::config::NodeConfig;
use crate::custody::StandbyReader;
use crate::liveness::chain::{LivenessDomain, Rule, view};
use crate::liveness::operator::limits::refused;
use crate::standby_key::chain::{StandbyChain, StandbyDomain};
use crate::tee::EnclaveKey;
use crate::test_support::scratch_dir;

const INPUTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ops/runbooks/production-deployment.toml");
const TEMPLATE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ops/runbooks/production-node.toml");
/// Mainnet's beacon genesis time and a day's turns, as the receiver is deployed in production.
const BEACON_GENESIS: u64 = 1_606_824_023;
const DAY_SECS: u64 = 86_400;
/// The restriction lifetime and the fault proof penalty the node records, as the production template
/// proposes them.
const TTL_MS: Millis = DAY_SECS as Millis * 1000;
const PENALTY: Penalty = Penalty { initial_bps: 10_000, decay_ms: 730 * DAY_SECS as Millis * 1000 };

sol! {
    function setCustody(address vault_, address schedule_) external;
    function setRegistry(address registry_) external;
    function schedule() external view returns (address);
    function logHeads() external view returns (address);
    function seedReceiver() external view returns (address);
    function liveness() external view returns (uint64, uint64, uint16, uint16);
    function livenessMinimums() external view returns (uint64, uint64, uint16, uint16);
    function livenessMaximums() external view returns (uint64, uint64, uint16, uint16);
    function takeWindow() external view returns (uint16, uint16, uint16);
    function switchBudget() external view returns (uint64);
    function blockMillis() external view returns (uint64);
    function normalWindow() external view returns (uint64);
    function fastWindow() external view returns (uint64);
}

/// A heartbeat rule as `OperatorSchedule.Liveness` takes it.
type Liveness = (u64, u64, u16, u16);

/// The production template's `node.tick`.
#[derive(serde::Deserialize)]
struct Ticker {
    #[serde(with = "humantime_serde")]
    tick: Duration,
}

#[tokio::test]
#[ignore = "requires Anvil, Python 3, and compiled contracts; scripts/dev-launch-values.sh runs it"]
async fn dev_scenario_the_rotation_contracts_deploy_at_the_launch_values_and_the_node_accepts_them() {
    let inputs: toml::Table = toml::from_str(&std::fs::read_to_string(INPUTS).unwrap()).unwrap();
    let section = |key: &str| inputs[key].as_table().unwrap().clone();
    let int = |table: &toml::Table, key: &str| u64::try_from(table[key].as_integer().unwrap()).unwrap();
    let small = |table: &toml::Table, key: &str| u16::try_from(int(table, key)).unwrap();
    let schedule_inputs = section("schedule");
    let rule = |key: &str| -> Liveness {
        let rule = schedule_inputs[key].as_table().unwrap();
        let (interval, tolerance) = (int(rule, "heartbeat_interval"), int(rule, "head_tolerance"));
        (interval, tolerance, small(rule, "heartbeat_misses"), small(rule, "head_blocks"))
    };
    let (liveness, minimums, maximums) = (rule("liveness"), rule("liveness_minimums"), rule("liveness_maximums"));
    let window = schedule_inputs["take_window"].as_table().unwrap();
    let window = (small(window, "slots"), small(window, "minimum"), small(window, "maximum"));
    let (switch_budget, block_millis) = (int(&schedule_inputs, "switch_budget"), int(&schedule_inputs, "block_millis"));
    let vote_inputs = section("parameter_vote");
    let (normal, fast) = (int(&vote_inputs, "normal_window"), int(&vote_inputs, "fast_window"));

    // 1. The vault, the MONKEY minter and the registry deploy, then the rotation contracts from the inputs.
    let operator = SigningKey::from_slice(&[0x42; 32]).unwrap();
    let enclave = Arc::new(EnclaveKey::generate());
    let mut chain = Chain::start(Address::from(enclave.address()), address(&operator)).await;
    let from = chain.accounts[0];
    let log_heads =
        deploy(&chain.provider, from, "LogHeads.sol/LogHeads.json", &(chain.vault,).abi_encode_params()).await;
    let receiver_args = (chain.accounts[9], BEACON_GENESIS, DAY_SECS).abi_encode_params();
    let receiver = deploy(&chain.provider, from, "SeedReceiver.sol/SeedReceiver.json", &receiver_args).await;
    let args = (chain.registry, receiver, log_heads, liveness, minimums, maximums, window, switch_budget, block_millis)
        .abi_encode_params();
    let schedule = deploy(&chain.provider, from, "OperatorSchedule.sol/OperatorSchedule.json", &args).await;
    let vote_args = (schedule, normal, fast).abi_encode_params();
    let vote = deploy(&chain.provider, from, "ParameterVote.sol/ParameterVote.json", &vote_args).await;
    let fault_proof =
        deploy(&chain.provider, from, "FaultProof.sol/FaultProof.json", &(chain.vault,).abi_encode_params()).await;
    (chain.schedule, chain.vote, chain.fault_proof) =
        (Some(schedule), Some((vote, TTL_MS)), Some((fault_proof, PENALTY)));

    // 2. The timelock lengthens the election epoch to a day, as a schedule needs, names the vault and the
    //    schedule in the registry, and the registry in the vault.
    let daily = Parameters { epochLength: DAY_SECS, ..dev_parameters().0 };
    chain
        .govern_each(vec![
            (chain.registry, setParametersCall { parameters: daily }.abi_encode()),
            (chain.registry, setCustodyCall { vault_: chain.vault, schedule_: schedule }.abi_encode()),
            (chain.vault, setRegistryCall { registry_: chain.registry }.abi_encode()),
        ])
        .await;
    assert_eq!(chain.view(chain.registry, scheduleCall {}).await, schedule, "the registry names the schedule");
    let held = chain.view(schedule, livenessCall {}).await;
    assert_eq!((held._0, held._1, held._2, held._3), liveness);
    let lowest = chain.view(schedule, livenessMinimumsCall {}).await;
    assert_eq!((lowest._0, lowest._1, lowest._2, lowest._3), minimums);
    let highest = chain.view(schedule, livenessMaximumsCall {}).await;
    assert_eq!((highest._0, highest._1, highest._2, highest._3), maximums);
    let taken = chain.view(schedule, takeWindowCall {}).await;
    assert_eq!((taken._0, taken._1, taken._2), window);
    assert_eq!(chain.view(schedule, switchBudgetCall {}).await, switch_budget);
    assert_eq!(chain.view(schedule, blockMillisCall {}).await, block_millis);
    assert_eq!(
        (chain.view(schedule, logHeadsCall {}).await, chain.view(schedule, seedReceiverCall {}).await),
        (log_heads, receiver)
    );
    assert_eq!(
        (chain.view(vote, normalWindowCall {}).await, chain.view(vote, fastWindowCall {}).await),
        (normal, fast)
    );
    println!(
        "PASS: the schedule holds the launch rule {liveness:?} within {minimums:?} and {maximums:?}, the take window \
         {window:?} and a {switch_budget} s switch budget, and the registry names it"
    );

    // 3. The liveness rule's read through the custody quorum finds the launch rule, which the node does
    //    not refuse at the production template's ticker.
    let dir = scratch_dir("launch-schedule");
    let anvil: NodeConfig =
        toml::from_str(&std::fs::read_to_string(crate::test_support::ANVIL_CONFIG).unwrap()).unwrap();
    let mut custody = custody_section(&chain, &dir, anvil.custody);
    custody.vault = format!("{:#x}", chain.vault);
    let reader = StandbyReader::connect_named(&custody, log_heads, "launch").unwrap();
    let standby = StandbyDomain { chain_id: custody.chain_id, vault: chain.vault, registry: chain.registry, schedule };
    let block = reader.latest().await.unwrap();
    let read = view(&block, &LivenessDomain { standby, log_heads }).await.unwrap();
    let (interval, tolerance, misses, blocks) = liveness;
    assert_eq!(read.rule, Rule { interval, tolerance, misses, blocks });
    let template: toml::Table = toml::from_str(&std::fs::read_to_string(TEMPLATE).unwrap()).unwrap();
    let Ticker { tick } = toml::from_str(&format!("tick = {:?}", template["node"]["tick"].as_str().unwrap())).unwrap();
    assert_eq!(refused(read.rule, tick), None, "the node refuses to sequence under the launch rule");
    println!("PASS: the node reads the launch rule through its custody quorum and accepts it at a {tick:?} ticker");

    // 4. The node starts with the contracts configured, and its custody watcher mirrors past the deployment:
    //    each window first checks the registry's schedule, the schedule's registry and turn length, the vote
    //    contract's schedule and the fault proof's vault, and mirrors nothing past a refusal.
    let deployed: U64 = chain.provider.request("eth_blockNumber", ()).await.unwrap();
    let deployed = deployed.to::<u64>();
    let node = Node::start(&chain, enclave).await;
    node.wait("the custody watcher to mirror past the deployment", move |e| completed_through(e) >= Some(deployed))
        .await;
    let governance = node.query(|e| e.governance().config).await.expect("the node records the vote contract");
    assert_eq!((governance.vote.0, governance.restriction_ttl_ms), (vote.0.0, TTL_MS));
    let faults = node.query(|e| e.operator_faults().config).await.expect("the node records the fault proof");
    assert_eq!((faults.contract.0, faults.penalty), (fault_proof.0.0, PENALTY));
    println!(
        "PASS: the node's custody watcher accepted the schedule, its turns, the vote contract and the fault proof, \
         and mirrored past block {deployed}"
    );
    node.stop().await;
}
