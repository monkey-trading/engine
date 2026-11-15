//! `engine admin init-staking` reads the staking contract's epoch and cooldown through the config's
//! custody providers and refuses a different one before it signs anything: the node it would send to
//! is never reached.

use std::fs;

use alloy::primitives::Address;
use clap::Parser;

use crate::admin::{AdminArgs, HexKey, KeyEnv, run_with_clock};
use crate::config::NodeConfig;
use crate::test_support::{ANVIL_CONFIG, scratch_dir, staking_rpc};
use engine_core::Fixed;
use engine_core::staking::StakingConfig;

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
    let rpc = staking_rpc(USDC, 86_400, 2_419_200).await;
    let config = config_for(&rpc, "chain", false);
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

fn staking_config(epoch_ms: i64, cooldown_ms: i64) -> StakingConfig {
    StakingConfig { liquidity_floor: Fixed::from_int(30_500_000), epoch_ms, start_ms: 0, cooldown_ms }
}

#[tokio::test]
async fn providers_that_name_delivered_keys_need_the_secrets_file_and_read_the_contract_with_it() {
    let rpc = staking_rpc(USDC, 86_400, 2_419_200).await;
    let config = config_for(&rpc, "keyed", true);
    let dir = config.parent().unwrap().to_owned();
    let load = || NodeConfig::load(&config).unwrap();
    let held = staking_config(86_400_000, 2_419_200_000);

    let without = format!("{:#}", super::check(load(), None, &held).await.unwrap_err());
    assert!(without.contains("need --secrets-file") && without.contains("key_a"), "{without}");
    let bundle = dir.join("secrets.toml");
    fs::write(&bundle, "version = 1\n[secrets]\nkey_a = \"ka\"\nkey_b = \"kb\"\nkey_c = \"kc\"\n").unwrap();
    super::check(load(), Some(&bundle), &held).await.unwrap();
    // With the keys, a value that is not the contract's is still refused.
    let wrong = format!("{:#}", super::check(load(), Some(&bundle), &staking_config(1, 2)).await.unwrap_err());
    assert!(wrong.contains("differs from the staking contract's"), "{wrong}");
    // A bundle short of a key is no better than none, and is not told to pass the file it passed.
    fs::write(&bundle, "version = 1\n[secrets]\nkey_a = \"ka\"\n").unwrap();
    let short = format!("{:#}", super::check(load(), Some(&bundle), &held).await.unwrap_err());
    assert!(short.contains("key_b") && !short.contains("need --secrets-file"), "{short}");
}

/// The Anvil config with its custody section replaced by three paths of the stand-in at `rpc` and a staking contract,
/// written to a file named for `test`.
fn config_for(rpc: &str, test: &str, keyed: bool) -> std::path::PathBuf {
    let dir = scratch_dir(&format!("init-staking-{test}"));
    fs::create_dir_all(&dir).unwrap();
    let [key_a, key_b, key_c] = if keyed { ["/{key_a}", "/{key_b}", "/{key_c}"] } else { [""; 3] };
    let custody = format!(
        r#"[custody]
provider = "evm"
chain_id = 31337
vault = "0xe7f1725E7734CE288F8367e1Bb143E90bb3F0512"

[custody.evm]
providers = [
  {{ name = "a", rpc_url = "{rpc}/a{key_a}" }},
  {{ name = "b", rpc_url = "{rpc}/b{key_b}" }},
  {{ name = "c", rpc_url = "{rpc}/c{key_c}" }},
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
        dir = dir.display(),
    );
    // Delivered keys need the listener that receives them.
    let custody = if keyed {
        format!(
            "{custody}\n[tee.sealing]\ndir = \"{}/keys\"\n[tee.sealing.secrets]\nlisten = \"127.0.0.1:9472\"\n",
            dir.display()
        )
    } else {
        custody
    };
    let path = dir.join("node.toml");
    let anvil = fs::read_to_string(ANVIL_CONFIG).unwrap();
    let (before, rest) = anvil.split_once("[custody]\n").expect("a custody section");
    let (_, after) = rest.split_once("\n[feeds]").expect("a feeds section after it");
    let text = format!("{before}{custody}\n[feeds]{after}");
    // A sealed key is not the published mock one.
    let text =
        if keyed { text.replace("use_published_mock_key = true", "use_published_mock_key = false") } else { text };
    fs::write(&path, text).unwrap();
    path
}

#[tokio::test]
async fn a_config_with_no_staking_contract_passes_the_check_and_goes_on_to_the_node() {
    let node = {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        format!("https://127.0.0.1:{port}")
    };
    // The dev config runs mock custody, which names no staking contract: nothing is read, so a value
    // no contract holds is not refused, and the run reaches for the node, which is not there.
    let argv = [
        "engine",
        "--config",
        crate::test_support::DEV_CONFIG,
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
        "1800000",
        "--cooldown-ms",
        "1",
    ];
    let args = Cli::try_parse_from(argv).unwrap().args;
    let env = KeyEnv { key_file: None, key: Some(HexKey(DEV_KEY.into())) };
    let sent = format!("{:#}", run_with_clock(args, env, || 1_000).await.unwrap_err());
    assert!(!sent.contains("staking contract") && !sent.contains("refusing to sign"), "{sent}");
    assert!(sent.contains("127.0.0.1"), "the run reached for the node: {sent}");
}
