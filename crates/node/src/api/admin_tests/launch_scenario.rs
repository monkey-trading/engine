//! The launch deployment's dev scenario on a local Anvil: the vault, its MONKEY token and the staking
//! contract deploy from `ops/runbooks/production-deployment.toml`, at the owner's decided values, and
//! the node's `InitEmissions` and `InitStaking` checks, reading the deployed contracts, accept them
//! and refuse a value that is not the contracts'. Run by `scripts/dev-launch-values.sh`.

use std::process::{Command as Process, Stdio};
use std::time::Duration;

use alloy::primitives::{Address, Bytes, U64, U256};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::TransactionRequest;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use engine_core::Fixed;
use engine_core::emissions::{EmissionSchedule, TokenUnits};
use engine_core::staking::StakingConfig;
use tokio::time::sleep;

use super::launch_values::send;
use super::*;
use crate::checkpoints::scenario::{Anvil, deploy};
use crate::checkpoints::{CheckpointChain, CheckpointRpc};
use crate::config::{CheckpointSection, CustodySection, FinalityTag};
use crate::custody::StakingTerms;
use crate::log::CheckpointDomain;

const INPUTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../ops/runbooks/production-deployment.toml");

sol! {
    function monkey() external view returns (address);
    function exitPeriod() external view returns (uint64);
    function claimWindow() external view returns (uint64);
    function staleAfter() external view returns (uint64);
    function getMinDelay() external view returns (uint256);
    function epoch() external view returns (uint256);
    function cooldown() external view returns (uint256);
}

#[tokio::test]
#[ignore = "requires Anvil and compiled contracts; scripts/dev-launch-values.sh runs it"]
async fn dev_scenario_the_launch_values_deploy_and_the_nodes_initialisations_accept_them() {
    let inputs: toml::Table = toml::from_str(&std::fs::read_to_string(INPUTS).unwrap()).unwrap();
    let int = |table: &str, key: &str| inputs[table][key].as_integer().unwrap() as u64;
    let text = |table: &str, key: &str| inputs[table][key].as_str().unwrap().to_owned();
    let start_ms =
        inputs["start"].as_str().unwrap().parse::<chrono::DateTime<chrono::Utc>>().unwrap().timestamp_millis();
    let (min_delay, epoch, cooldown) =
        (int("timelock", "min_delay"), int("staking", "epoch"), int("staking", "cooldown"));
    let budget: u128 = text("vault", "period_budget").parse().unwrap();
    let floor: Fixed = text("init_staking", "liquidity_floor").parse().unwrap();

    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let _anvil = Anvil(
        Process::new("anvil")
            .args(["--port", &port.to_string(), "--silent"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Anvil installed"),
    );
    let url = format!("http://127.0.0.1:{port}");
    let provider = RpcClient::new_http(url.parse().unwrap());
    crate::test_support::wait_for("Anvil to answer eth_chainId", async {
        while provider.request::<_, U64>("eth_chainId", ()).await.is_err() {
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await;
    let accounts: Vec<Address> = provider.request("eth_accounts", ()).await.unwrap();
    let signer = crate::tee::EnclaveKey::generate();

    // The deployment from the inputs. `Staking` is deployed directly with the two values the registry's
    // constructor passes it, to keep the scenario to the values under test.
    let usdc = deploy(&provider, accounts[0], "MockUSDC.sol/MockUSDC.json", &[]).await;
    let mut governance_args = U256::from(min_delay).to_be_bytes::<32>().to_vec();
    governance_args.extend_from_slice(&U256::from(0x80).to_be_bytes::<32>());
    governance_args.extend_from_slice(&U256::from(0xC0).to_be_bytes::<32>());
    governance_args.extend_from_slice(Address::ZERO.into_word().as_slice());
    for _ in 0..2 {
        governance_args.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
        governance_args.extend_from_slice(accounts[0].into_word().as_slice());
    }
    let governance =
        deploy(&provider, accounts[0], "TimelockController.sol/TimelockController.json", &governance_args).await;
    let vault_args = (
        usdc,
        Address::from(signer.address()),
        governance,
        accounts[0],
        U256::from(budget),
        int("vault", "exit_period"),
        int("vault", "claim_window"),
        int("vault", "stale_after"),
    )
        .abi_encode();
    let vault = deploy(&provider, accounts[0], "Vault.sol/Vault.json", &vault_args).await;
    let call = |to: Address, input: Vec<u8>| {
        let provider = provider.clone();
        async move {
            let request = TransactionRequest::default().to(to).input(input.into());
            provider.request::<_, Bytes>("eth_call", (request, "latest")).await.unwrap()
        }
    };
    let monkey = monkeyCall::abi_decode_returns(&call(vault, monkeyCall {}.abi_encode()).await).unwrap();
    let staking_args = (monkey, U256::from(cooldown), U256::from(epoch)).abi_encode();
    let staking = deploy(&provider, accounts[0], "Staking.sol/Staking.json", &staking_args).await;

    // The chain holds the decided values.
    let read = |out: Bytes| U256::from_be_slice(&out);
    assert_eq!(read(call(vault, exitPeriodCall {}.abi_encode()).await), U256::from(14 * 86_400));
    assert_eq!(read(call(vault, claimWindowCall {}.abi_encode()).await), U256::from(30 * 86_400));
    assert_eq!(read(call(vault, staleAfterCall {}.abi_encode()).await), U256::from(86_400));
    assert_eq!(read(call(governance, getMinDelayCall {}.abi_encode()).await), U256::from(3 * 86_400));
    assert_eq!(read(call(staking, epochCall {}.abi_encode()).await), U256::from(86_400));
    assert_eq!(read(call(staking, cooldownCall {}.abi_encode()).await), U256::from(28 * 86_400));

    // The node reads the vault's budget and, through the custody quorum, the staking contract's terms.
    let dir = scratch_dir("launch-values-chain");
    std::fs::create_dir_all(&dir).unwrap();
    let section: CheckpointSection = toml::from_str(&format!(
        "rpc_url = {url:?}\nrelayer = '{:#x}'\nretry_interval = '40ms'\nrpc_timeout = '2s'",
        accounts[9]
    ))
    .unwrap();
    let domain = CheckpointDomain { chain_id: 31337, vault: engine_core::types::AccountId(vault.into()) };
    let chain = CheckpointRpc::new(
        &section,
        domain,
        usdc,
        FinalityTag::Latest,
        0,
        &dir,
        engine_core::types::AccountId(signer.address()),
    )
    .unwrap();
    let vault_budget = chain.emission_budget().await.unwrap();
    assert_eq!(vault_budget, TokenUnits(budget), "the vault's cap gives the decided budget back");
    let custody: CustodySection = toml::from_str(&format!(
        r#"provider = "evm"
chain_id = 31337
vault = "{vault:#x}"

[evm]
providers = [{{ name = "a", rpc_url = "{url}" }}, {{ name = "b", rpc_url = "{url}" }}, {{ name = "c", rpc_url = "{url}" }}]
usdc = "{usdc:#x}"
finality_tag = "latest"
withdrawal_finality_tag = "finalized"
confirmations = 0
start_block = 0
max_block_range = 1000
poll_interval = "200ms"
rpc_timeout = "5s"
stall_critical_after = "5m"
finality_stalled_after = "5m"
cursor_path = "{dir}/custody-cursor.json"

[evm.staking]
contract = "{staking:#x}"
cursor_path = "{dir}/staking-cursor.json"

[evm.staking.registry]
contract = "0x00000000000000000000000000000000000000ee"
"#,
        dir = dir.display()
    ))
    .unwrap();
    let mut node = Node::start_on("launch-values", true).await;
    node.state.emission_budget = Some(vault_budget);
    node.state.staking_terms = Some(StakingTerms::connect(&custody).unwrap().expect("a staking contract"));
    let emissions =
        |budget| AdminCommand::InitEmissions(EmissionSchedule { start_ms, period_budget: TokenUnits(budget) });
    let staking_config = |epoch_secs: u64, cooldown_secs: u64| {
        AdminCommand::InitStaking(StakingConfig {
            liquidity_floor: floor,
            epoch_ms: (epoch_secs * 1000) as i64,
            start_ms,
            cooldown_ms: (cooldown_secs * 1000) as i64,
        })
    };

    // A budget, an epoch or a cooldown that is not the contracts' is refused and not logged.
    let (status, body) = send(&node, 0, emissions(budget / 10)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("differs from the vault's"), "{body}");
    for (command, wrong) in [
        (staking_config(epoch / 2, cooldown), "epoch 43200000 ms differs from the staking contract's 86400 s"),
        (staking_config(epoch, cooldown / 4), "cooldown 604800000 ms differs from the staking contract's 2419200 s"),
    ] {
        let (status, body) = send(&node, 0, command).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body.to_string().contains(wrong), "{body}");
    }
    assert!(admin_entries(&node.entries()).is_empty(), "no refused command reached the log");

    // The decided values are accepted and applied.
    let applied = serde_json::json!([{ "type": "admin_applied" }]);
    assert_eq!(send(&node, 0, emissions(budget)).await, (StatusCode::OK, applied.clone()));
    assert_eq!(send(&node, 1, staking_config(epoch, cooldown)).await, (StatusCode::OK, applied));
}
