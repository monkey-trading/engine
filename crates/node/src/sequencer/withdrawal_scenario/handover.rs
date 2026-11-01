//! An elected operator's enclave takes the keys from the incumbent's: A runs as the operator in
//! effect with rotation on; A authorizes a withdrawal and it pays; an election names B and, past
//! its effective time, B reads A's handover endpoint from the registry, fetches the published log,
//! takes the keys and the final log segment, and continues at the seq after A's final checkpoint;
//! A exits; A's stale sealed copy refuses to open; a re-election of A hands the keys back, and
//! A's old sealed copy cannot reopen under the new release generation.
//!
//! The vault reads chain time while an authorization's 24-hour lifetime is issued at the engine's
//! sequenced time; the election's seven-day notice is crossed by warping the chain alone, so the
//! scenario pays the incumbent's authorization before the warp: no authorization issued at engine
//! time could be claimed after it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy::primitives::{Address, U256, keccak256};
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};
use engine_core::event::Event;
use k256::ecdsa::SigningKey;
use serde_json::json;
use sha2::Digest as _;
use tokio::time::{sleep, timeout};

use crate::clock::now_ms;
use crate::sequencer::withdrawal_scenario::client::{Client, User};
use crate::sequencer::withdrawal_scenario::flows::last_seq;
use crate::sequencer::withdrawal_scenario::node::{ChildProcess, Node, free_port, write_config};
use crate::sequencer::withdrawal_scenario::rpc::{
    Chain, latestSeqCall, rotationEnabledCall, setEnclaveCall, setRotationCall, setSignerCall,
};
use crate::sequencer::withdrawal_scenario::standby::{pay_b, rotation_and_staking};
use crate::sequencer::withdrawal_scenario::{CHILD_ALERTS, CHILD_CONFIG, CHILD_FEED, CHILD_LAUNCH_DIGEST, CHILD_TEST};
use crate::test_support::LOADED_WAIT;

/// The deployer's operator key, which `config/anvil.toml` names: the initial operator in effect.
const OPERATOR: [u8; 32] = [0x42; 32];
/// B's admin key, the candidate the election names.
const B_ADMIN: [u8; 32] = [0x63; 32];
/// The candidates' stand-in launch measurements; the releases differ through the config hashes.
const A_DIGEST: [u8; 48] = [0x5a; 48];
const B_DIGEST: [u8; 48] = [0x71; 48];
/// A release the deployment names before A's config exists; superseded before A seals keys.
const PLACEHOLDER_CONFIG: [u8; 32] = [0x99; 32];
/// The registry's seven-day notice and its finality margin, both from the scenario's deployment.
const NOTICE_SECS: u64 = 7 * 86_400;
const FINALITY_MARGIN_SECS: u64 = 60;

/// The dev scenario: see the module docs.
/// Error cases:
/// - any real node, chain, or stream step or behavioral assertion fails
pub(super) async fn elected_operator_takes_over() -> anyhow::Result<()> {
    let root = std::env::var_os("SCENARIO_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("engine-handover-{}", std::process::id())));
    let dir = root.join("operator-handover");
    std::fs::create_dir_all(&dir)?;
    ensure!(!dir.join("a.toml").exists(), "scenario needs a fresh artifact directory: {}", dir.display());
    let mut chain = Chain::start(&dir).await?;
    let domain = engine_core::eip712::domain(31337, engine_core::types::AccountId(chain.vault.into()));
    let services = super::services::Services::start(&chain.url, chain.mining.clone(), domain).await?;

    // The registry deploys with rotation off and a placeholder initial release; A's config lands
    // after, and the timelock names A's real release while rotation is still off.
    let operator_key = SigningKey::from_slice(&OPERATOR)?;
    let operator = Address::from(engine_core::eip712::address(operator_key.verifying_key()).0);
    let staking = chain.deploy_registry(operator, A_DIGEST, PLACEHOLDER_CONFIG, 3_600, false).await?;
    let staking_start = now_ms() + 30_000;
    let a_port = free_port()?;
    let a_handover = free_port()?;
    let a_metrics = free_port()?;
    let a_dir = dir.join("a");
    std::fs::create_dir_all(&a_dir)?;
    let a_config_path = a_dir.join("a.toml");
    write_config(&a_config_path, &a_dir, a_port, a_metrics, &chain, &services, |cfg| {
        cfg["operator"]["rotation_enabled"] = true.into();
        cfg["tee"]["use_published_mock_key"] = false.into();
        cfg["tee"]["sealing"] = json!({
            "dir": a_dir.join("keys").to_string_lossy(),
            "handover": { "listen": format!("127.0.0.1:{a_handover}"), "danger_accept_invalid_certs": true },
        });
        rotation_and_staking(cfg, &chain, &staking, &a_dir, staking_start, true)
    })?;
    let a_config_hash: [u8; 32] = sha2::Sha256::digest(std::fs::read(&a_config_path)?).into();
    chain.set_current_release(&A_DIGEST, a_config_hash).await?;
    let vault: Address = {
        let cfg: toml::Value = toml::from_str(&std::fs::read_to_string(&a_config_path)?)?;
        cfg["custody"]["vault"].as_str().context("custody.vault")?.parse().context("custody.vault address")?
    };
    let mut node =
        Node::uninitialized(a_dir, chain, services, Client::new(a_port, vault)?, a_config_path.clone(), a_metrics);

    // 1. Rotation is off: A's rotation-on config refuses to start.
    match node.restart().await {
        Ok(()) => anyhow::bail!("A started with rotation on while the registry has rotation off"),
        Err(_) => {}
    };
    let log = std::fs::read_to_string(node.dir.join("node.log"))?;
    ensure!(
        log.contains("the vault's registry has rotation off"),
        "A's refusal names the rotation check: {}",
        log.lines().rev().take(20).collect::<Vec<_>>().join("\n")
    );

    // 2. The operator in effect registers as a candidate with its handover endpoint, rotation
    //    turns on, the release change folds, and A seals new keys once the timelock installs the
    //    enclave it attests.
    node.chain
        .register_impersonated_candidate(
            operator,
            &operator_key,
            U256::from(10_000),
            A_DIGEST,
            a_config_hash,
            operator,
            &format!("127.0.0.1:{a_port}"),
            &format!("127.0.0.1:{a_handover}"),
        )
        .await?;
    node.chain.govern_target(node.chain.registry, setRotationCall { enabled: true }.abi_encode()).await?;
    ensure!(node.chain.read(node.chain.registry, rotationEnabledCall {}).await?, "rotation never turned on");
    node.chain.sync_release().await?;
    // A new-key start serves only its attestation until the timelock installs the enclave, so the
    // spawn and the full-API wait are separate.
    node.spawn_keys(Some("new-key"), Some(A_DIGEST)).await?;
    let a_enclave = {
        let client = Client::new(a_port, vault)?;
        wait_attestation(&client, &mut node, "a").await?
    };
    node.chain
        .impersonate_send(operator, node.chain.registry, setEnclaveCall { enclave: a_enclave }.abi_encode())
        .await?;
    node.chain.govern(setSignerCall { signer: a_enclave }.abi_encode()).await?;
    node.wait_api().await?;
    node.wait_withdrawals_enabled(0).await?;

    // A client deposits and asks for a withdrawal; A approves it before the handover.
    let trader = User::new(1)?;
    node.chain.deposit(trader.address(), 100, false).await?;
    node.client.declare(&trader).await?;
    node.client.wait_balance(&trader, "100").await?;
    node.client.wait_finalised(&trader).await?;
    let request = node.client.withdrawal(&trader, "10").await?;
    let events = node.client.request_withdrawal(&request).await?;
    ensure!(
        events
            .iter()
            .any(|event| matches!(event, Event::WithdrawalApproved { account, .. } if *account == trader.account)),
        "the withdrawal was not approved: {events:?}"
    );

    // 3. B registers as a candidate; the stakers delegate to it; a close elects it.
    let b_admin = SigningKey::from_slice(&B_ADMIN)?;
    let b_candidate = node.chain.funder;
    let b_dir = dir.join("b");
    std::fs::create_dir_all(&b_dir)?;
    let b_port = free_port()?;
    let b_handover = free_port()?;
    let b_config_path = b_dir.join("b.toml");
    write_config(&b_config_path, &b_dir, b_port, free_port()?, &node.chain, &node.services, |cfg| {
        cfg["operator"]["public_key"] = hex::encode(b_admin.verifying_key().to_sec1_point(false).as_bytes()).into();
        cfg["operator"]["rotation_enabled"] = true.into();
        cfg["tee"]["use_published_mock_key"] = false.into();
        cfg["tee"]["sealing"] = json!({
            "dir": b_dir.join("keys").to_string_lossy(),
            "handover": { "listen": format!("127.0.0.1:{b_handover}"), "danger_accept_invalid_certs": true },
        });
        rotation_and_staking(cfg, &node.chain, &staking, &b_dir, staking_start, true)
    })?;
    let b_config_hash: [u8; 32] = sha2::Sha256::digest(std::fs::read(&b_config_path)?).into();
    let b_release = keccak256([B_DIGEST.as_slice(), &b_config_hash].concat());
    node.chain
        .register_candidate(
            b_candidate,
            &b_admin,
            U256::from(10_000),
            B_DIGEST,
            b_config_hash,
            b_candidate,
            &format!("127.0.0.1:{b_port}"),
            &format!("127.0.0.1:{b_handover}"),
        )
        .await?;
    node.chain.delegate_self(b_candidate).await?;
    // Stakers must not be the relayer: its nonce reservations back checkpoint posting, and staking
    // transactions would consume them. The stakes stay under the token's cap, 52_560 MONKEY at the
    // vault's one-MONKEY period budget, leaving room for the re-election's delegation below.
    let accounts: Vec<Address> = node.chain.request("eth_accounts", json!([])).await?;
    node.chain.stake_from(accounts[3], monkey(20_000)).await?;
    node.chain.delegate_from(accounts[3], b_candidate).await?;

    // A signs the withdrawal's authorization and it pays while the chain and engine clocks still
    // agree: the vault's 24-hour authorization lifetime runs against chain time, which the warp
    // below jumps seven days past the engine's sequenced time, so no authorization issued at
    // engine time can be claimed after it. The authorization is the incumbent's, signed before
    // the handover either way.
    let mut a_stream = node.client.stream().await?;
    let issued = a_stream.payout(trader.account, 10).await?;
    drop(a_stream);
    let approved = issued.approved_seq.context("the issued authorization names its approval")?;
    wait_latest_seq(&mut node.chain, approved).await?;
    pay_b(&node.chain, &issued).await?;

    // 4. A close elects B; the engine mirrors it; time passes its notice; the fold lands.
    let open_start = node.chain.open_epoch_start().await?;
    let close1 = open_start + 2 * 3_600;
    let (_, effective1) = node.chain.close_epoch_at(close1).await?.context("the close elected no candidate")?;
    // The close lands a few blocks after the warp, so its effective time carries the inclusion
    // delay.
    let notice1 = close1 + NOTICE_SECS + FINALITY_MARGIN_SECS;
    ensure!(
        (notice1..notice1 + 30).contains(&effective1),
        "the effective time is not the close plus the notice: {effective1}"
    );
    // Mirrored: the deployer still signs and the elected key is pending — the engine applies the
    // election only seven days of sequenced time after it logged, which real child processes do
    // not outrun; the handover below needs only the chain-side naming.
    wait_operator(&node.client, || node.chain.check(), Some(&format!("{operator:#x}")), true).await?;
    // The incumbent sees the election coming and raises the Warning that a handover is expected.
    wait_log(&node.dir.join("node.log"), "operator_elected", &mut node).await?;
    // The chain alone passes the effective time; the fold lands; the vault names B's release.
    node.chain.warp_to(effective1 + 120).await?;
    node.chain.sync_release().await?;
    node.chain.wait_finalized_release(b_release).await?;

    // 5. B starts: it resolves A's handover endpoint from the registry, fetches the published log,
    //    takes the keys and the final segment, and continues; A exits; A's authorization pays.
    let mut b_child = spawn_b(&b_dir, &b_config_path, &node, B_DIGEST)?;
    let b_client = Client::new(b_port, node.chain.vault)?;
    let mut b_check = b_child_checker(&mut b_child);
    wait_serving(&b_client, &mut b_check).await?;
    let status = node.exited(LOADED_WAIT).await?;
    ensure!(status.success(), "A exited with {status} after handing the keys over");
    let a_tip = last_seq(&node.dir.join("log"))?;
    timeout(LOADED_WAIT, async {
        loop {
            if last_seq(&b_dir.join("log"))? > a_tip {
                return anyhow::Ok(());
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("B did not continue past A's final checkpoint (seq {a_tip})")??;
    println!(
        "PASS: A's pre-handover authorization paid, and the elected operator's enclave took the keys and the log from the incumbent's and continued at the seq after its final checkpoint"
    );

    // 6. A restart refuses: its sealed copy is under a superseded release generation.
    match node.restart_keys(None, Some(A_DIGEST)).await {
        Ok(()) => anyhow::bail!("A opened its stale sealed keys"),
        Err(_) => {}
    }
    let log = std::fs::read_to_string(node.dir.join("node.log"))?;
    ensure!(
        log.contains("refusing this release's keys"),
        "A's restart refusal names the superseded release: {}",
        log.lines().rev().take(20).collect::<Vec<_>>().join("\n")
    );

    // 7. A re-election: fresh stake delegates to A; a close elects it; past the notice A takes the
    //    keys back from B, and B exits.
    // The re-election's delegation must beat B's by the margin yet keep the total minted under the
    // cap: 30_000 to A against B's 20_000.
    let accounts: Vec<Address> = node.chain.request("eth_accounts", json!([])).await?;
    let extra = accounts[5];
    node.chain.stake_from(extra, monkey(30_000)).await?;
    node.chain.delegate_from(extra, operator).await?;
    // The chain already warped past the election's effective time: close the next epoch at least
    // an epoch past now, not past the registry's pre-warp epoch start.
    let latest: u64 = {
        let block: serde_json::Value = node.chain.request("eth_getBlockByNumber", json!(["latest", false])).await?;
        u64::from_str_radix(block["timestamp"].as_str().context("timestamp")?.trim_start_matches("0x"), 16)?
    };
    let open_start = node.chain.open_epoch_start().await?;
    let close2 = std::cmp::max(open_start + 3_600, latest + 5);
    let (_, effective2) = node.chain.close_epoch_at(close2).await?.context("the re-election elected no candidate")?;
    // The deployer's key still signs on B's node; A's key is pending, as before, until sequenced
    // time passes the notice the scenario does not outrun.
    wait_operator(&b_client, &mut b_check, Some(&format!("{operator:#x}")), true).await?;
    drop(b_check);
    let a_release = keccak256([A_DIGEST.as_slice(), &a_config_hash].concat());
    node.chain.warp_to(effective2 + 120).await?;
    node.chain.sync_release().await?;
    node.chain.wait_finalized_release(a_release).await?;
    node.restart_keys(None, Some(A_DIGEST)).await?;
    let status = b_child.exited(LOADED_WAIT).await?;
    ensure!(status.success(), "B exited with {status} after handing the keys back");
    println!("PASS: the re-elected operator's enclave took the keys back; the stale sealed copy never opened");
    Ok(())
}

/// `amount` whole MONKEY in base units.
fn monkey(amount: u64) -> U256 {
    U256::from(amount) * U256::from(10).pow(U256::from(18))
}

/// Spawns B as a child with the automatic key start: an election names its release, so it takes
/// the keys from the incumbent the registry names.
fn spawn_b(b_dir: &Path, config: &Path, node: &Node, digest: [u8; 48]) -> anyhow::Result<ChildProcess> {
    let path = config.to_str().context("B config path is UTF-8")?;
    let log = b_dir.join("node-b.log");
    ChildProcess::spawn(
        std::env::current_exe()?,
        &["--ignored", "--exact", CHILD_TEST, "--nocapture", "--test-threads=1"],
        &log,
        &[
            (CHILD_CONFIG, path),
            (CHILD_FEED, &node.services.feed),
            (CHILD_ALERTS, &node.services.alerts),
            (CHILD_LAUNCH_DIGEST, &hex::encode(digest)),
        ],
        Some(b_dir),
    )
}

/// Polls the attestation route until the node serves one, returning the enclave address; the
/// one-off `new-key` start serves nothing else until the timelock installs it.
async fn wait_attestation(client: &Client, node: &mut Node, who: &str) -> anyhow::Result<Address> {
    timeout(LOADED_WAIT, async {
        loop {
            if let Ok(value) = client.get("/v1/attestation").await {
                let enclave: Address =
                    value["enclave_address"].as_str().context("attested enclave")?.parse().context("enclave")?;
                return anyhow::Ok(enclave);
            }
            node.services.check()?;
            node.chain.check()?;
            node.check_running()?;
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .with_context(|| format!("{who} never served its attestation"))?
}

/// Waits until the vault has posted a checkpoint at or past `seq`.
async fn wait_latest_seq(chain: &mut Chain, seq: u64) -> anyhow::Result<()> {
    timeout(LOADED_WAIT, async {
        loop {
            if chain.read(chain.vault, latestSeqCall {}).await? >= seq {
                return anyhow::Ok(());
            }
            chain.check()?;
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("no checkpoint covered the approval before the handover")?
}

/// Waits until the node's log contains `text`, requiring the child to stay up.
async fn wait_log(path: &Path, text: &str, node: &mut Node) -> anyhow::Result<()> {
    timeout(LOADED_WAIT, async {
        loop {
            if let Ok(log) = std::fs::read_to_string(path)
                && log.contains(text)
            {
                return anyhow::Ok(());
            }
            node.services.check()?;
            node.chain.check()?;
            node.check_running()?;
            sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .with_context(|| format!("the node's log never contained {text:?}"))?
}

/// Waits until `/v1/operator` shows the operator `want` with a pending rotation when `pending`,
/// or no pending rotation when not.
async fn wait_operator(
    client: &Client,
    mut check: impl FnMut() -> anyhow::Result<()>,
    want: Option<&str>,
    pending: bool,
) -> anyhow::Result<()> {
    timeout(LOADED_WAIT, async {
        loop {
            if let Ok(view) = client.get("/v1/operator").await {
                let operator = view["operator"].as_str().unwrap_or_default();
                let has_pending = !view["pending_rotation"].is_null();
                if want.is_some_and(|want| operator == want) && has_pending == pending {
                    return anyhow::Ok(());
                }
            }
            check()?;
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .with_context(|| format!("/v1/operator never showed {want:?} pending={pending}"))?
}

/// Waits until the API answers the markets route.
async fn wait_serving(client: &Client, check: &mut dyn FnMut() -> anyhow::Result<()>) -> anyhow::Result<()> {
    timeout(LOADED_WAIT, async {
        loop {
            if client.get("/v1/markets").await.is_ok() {
                return anyhow::Ok(());
            }
            check()?;
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("the node never served its API")?
}

/// A check closure over a raw child process.
fn b_child_checker(child: &mut ChildProcess) -> impl FnMut() -> anyhow::Result<()> + '_ {
    move || child.check()
}
