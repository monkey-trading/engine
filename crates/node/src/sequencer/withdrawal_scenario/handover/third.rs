//! A third rotation-set member, C, which follows from before B's election: once A has the keys back,
//! an election names C, whose start takes the whole log from A onto a copy its own writer cut, and a
//! re-election hands the keys back to A, whose copy is its own writer's too. Each whole-log take
//! sets the member's copy aside first, so the holder's segments land on an empty directory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy::primitives::{Address, B256, U256, keccak256};
use anyhow::{Context, ensure};
use k256::ecdsa::SigningKey;
use serde_json::json;
use sha2::Digest as _;
use tokio::time::{sleep, timeout};

use crate::log::MANIFEST;
use crate::sequencer::withdrawal_scenario::client::Client;
use crate::sequencer::withdrawal_scenario::flows::last_seq;
use crate::sequencer::withdrawal_scenario::handover::{child_checker, monkey, spawn_release, wait_serving};
use crate::sequencer::withdrawal_scenario::node::{ChildProcess, Node, free_port, write_config};
use crate::sequencer::withdrawal_scenario::standby::rotation_and_staking;
use crate::test_support::LOADED_WAIT;

/// C's admin key and stand-in launch measurement.
const C_ADMIN: [u8; 32] = [0x64; 32];
const C_DIGEST: [u8; 48] = [0x72; 48];
/// The line a member logs once it hands its keys over and follows its successor.
const HANDED: &str = "handed the keys over: following the successor";

/// C's running node.
pub(super) struct Third {
    child: ChildProcess,
    dir: PathBuf,
    client: Client,
    candidate: Address,
    release: B256,
}

impl Third {
    /// Stops C's node, following the operator in effect.
    /// Error cases:
    /// - the node does not drain
    pub(super) async fn stop(mut self) -> anyhow::Result<()> {
        self.child.stop().await.context("C, following, did not drain")
    }
}

/// Registers C as a candidate with its handover endpoint and starts its node, which, as a
/// rotation-set member, follows the operator in effect into a copy its own writer cuts.
/// Error cases:
/// - a chain step, C's config, or its start fails
pub(super) async fn follows(node: &Node, staking: &Address, staking_start: i64) -> anyhow::Result<Third> {
    let admin = SigningKey::from_slice(&C_ADMIN)?;
    let accounts: Vec<Address> = node.chain.request("eth_accounts", json!([])).await?;
    let candidate = accounts[6];
    let dir = node.dir.parent().context("the scenario's artifact directory")?.join("c");
    std::fs::create_dir_all(&dir)?;
    let (port, handover) = (free_port()?, free_port()?);
    let config = dir.join("c.toml");
    write_config(&config, &dir, port, free_port()?, &node.chain, &node.services, |cfg| {
        cfg["operator"]["public_key"] = hex::encode(admin.verifying_key().to_sec1_point(false).as_bytes()).into();
        cfg["operator"]["rotation_enabled"] = true.into();
        cfg["tee"]["use_published_mock_key"] = false.into();
        cfg["tee"]["sealing"] = json!({
            "dir": dir.join("keys").to_string_lossy(),
            "handover": { "listen": format!("127.0.0.1:{handover}"), "danger_accept_invalid_certs": true },
        });
        rotation_and_staking(cfg, &node.chain, staking, &dir, staking_start, true)
    })?;
    let config_hash: [u8; 32] = sha2::Sha256::digest(std::fs::read(&config)?).into();
    let release = keccak256([C_DIGEST.as_slice(), &config_hash].concat());
    let (api, endpoint) = (format!("127.0.0.1:{port}"), format!("127.0.0.1:{handover}"));
    let bond = U256::from(10_000);
    node.chain.register_candidate(candidate, &admin, bond, C_DIGEST, config_hash, candidate, &api, &endpoint).await?;
    node.chain.delegate_from(candidate, candidate, bond).await?;
    let mut child = spawn_release(&dir, &config, node, C_DIGEST, None)?;
    wait_text(&dir.join("node-c.log"), "following the operator in effect", 0, &mut child_checker(&mut child)).await?;
    let client = Client::new(port, node.chain.vault)?;
    Ok(Third { child, dir, client, candidate, release })
}

/// With A the operator in effect: an election names C, whose start takes the whole log from A onto
/// its copy, set aside first, and A follows C; then a re-election names A, whose start takes it back
/// the same way, and C follows A. `operator` is A's candidate account, `a_release` the release the
/// vault names for it, and `staker` the account whose delegation the elections move.
/// Error cases:
/// - a chain step, a start, a handover, or a behavioral assertion fails
pub(super) async fn elect_third_then_back(
    node: &mut Node,
    third: &mut Third,
    (operator, a_release, staker): (Address, B256, Address),
) -> anyhow::Result<()> {
    let a_log = node.dir.join("node.log");
    let c_log = third.dir.join("node-c.log");
    let (a_handed, c_handed) = (count(&a_log, HANDED), count(&c_log, HANDED));

    // The election moves A's stake to C.
    node.chain.redelegate_from(staker, operator, third.candidate, monkey(30_000)).await?;
    elect(node, third.candidate, third.release).await?;
    wait_serving(&third.client, &mut child_checker(&mut third.child)).await?;
    wait_text(&a_log, HANDED, a_handed, &mut || node.check_running()).await?;
    let a_tip = last_seq(&node.dir.join("log"))?;
    wait_past(&third.dir.join("log"), a_tip, &mut child_checker(&mut third.child)).await?;
    ensure!(third.dir.join("log.aside").join(MANIFEST).exists(), "C's whole-log take left its copy in place");
    println!(
        "PASS: an election named C, which had followed since before B's; its start set its copy aside and took the whole log from A, which follows C"
    );

    // The re-election moves it back: A takes the keys and the whole log from C the same way.
    node.chain.redelegate_from(staker, third.candidate, operator, monkey(30_000)).await?;
    elect(node, operator, a_release).await?;
    let client = node.client.clone();
    wait_serving(&client, &mut || node.check_running()).await?;
    wait_text(&c_log, HANDED, c_handed, &mut child_checker(&mut third.child)).await?;
    let c_tip = last_seq(&third.dir.join("log"))?;
    wait_past(&node.dir.join("log"), c_tip, &mut || node.check_running()).await?;
    ensure!(node.dir.join("log.aside").join(MANIFEST).exists(), "A's whole-log take left its copy in place");
    println!(
        "PASS: a re-election named A, whose start set its copy aside and took the whole log back from C, which follows A"
    );
    Ok(())
}

/// Closes an epoch electing `candidate`, past now and the open epoch, and passes its effective time
/// on the chain alone until the vault names `release` at finality.
/// Error cases:
/// - the close elects another, or a chain step fails
async fn elect(node: &mut Node, candidate: Address, release: B256) -> anyhow::Result<()> {
    let block: serde_json::Value = node.chain.request("eth_getBlockByNumber", json!(["latest", false])).await?;
    let latest = u64::from_str_radix(block["timestamp"].as_str().context("timestamp")?.trim_start_matches("0x"), 16)?;
    let close = std::cmp::max(node.chain.open_epoch_start().await? + 3_600, latest + 5);
    let (elected, effective) = node.chain.close_epoch_at(close).await?.context("the close elected no candidate")?;
    ensure!(elected == candidate, "the close elected {elected}, not {candidate}");
    node.chain.warp_to(effective + 120).await?;
    node.chain.sync_release().await?;
    node.chain.wait_finalized_release(release).await
}

/// How many times `text` appears in the log at `path`.
fn count(path: &Path, text: &str) -> usize {
    std::fs::read_to_string(path).map_or(0, |log| log.matches(text).count())
}

/// Waits until the log at `path` holds `text` more than `before` times, `check`ing its process.
async fn wait_text(
    path: &Path,
    text: &str,
    before: usize,
    check: &mut dyn FnMut() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    timeout(LOADED_WAIT, async {
        while count(path, text) <= before {
            check()?;
            sleep(Duration::from_millis(300)).await;
        }
        anyhow::Ok(())
    })
    .await
    .with_context(|| format!("{} never logged {text:?} again", path.display()))?
}

/// Waits until the log in `dir` holds a command past `seq`, `check`ing its process.
async fn wait_past(dir: &Path, seq: u64, check: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
    timeout(LOADED_WAIT, async {
        while last_seq(dir)? <= seq {
            check()?;
            sleep(Duration::from_millis(200)).await;
        }
        anyhow::Ok(())
    })
    .await
    .with_context(|| format!("{} never continued past seq {seq}", dir.display()))?
}
