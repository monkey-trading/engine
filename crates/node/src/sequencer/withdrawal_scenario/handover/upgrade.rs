//! The operator in effect's upgrade while rotation is on, the handover scenario's last step: its
//! candidacy moves to a release the timelock approved for it, and the upgrade takes the keys and
//! the log from the running release over the `--predecessor` handover.

use std::time::Duration;

use alloy::primitives::{Address, keccak256};
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};
use serde_json::json;
use sha2::Digest as _;
use tokio::time::{sleep, timeout};

use crate::sequencer::withdrawal_scenario::client::Client;
use crate::sequencer::withdrawal_scenario::flows::last_seq;
use crate::sequencer::withdrawal_scenario::handover::{
    A_DIGEST, child_checker, spawn_release, wait_latest_seq, wait_log, wait_serving,
};
use crate::sequencer::withdrawal_scenario::node::{Node, free_port, write_config};
use crate::sequencer::withdrawal_scenario::rpc::{setReleaseApprovalCall, setReleaseCall, signerEpochCall};
use crate::sequencer::withdrawal_scenario::standby::rotation_and_staking;
use crate::test_support::LOADED_WAIT;

/// The stand-in measurement of A's upgrade.
const A_UPGRADE_DIGEST: [u8; 48] = [0x5b; 48];

/// The operator in effect's upgrade while rotation is on: see the module docs. `candidate` is its
/// registry identity, `signer` the enclave address the vault names, and `predecessor` the running
/// release's handover port.
/// Error cases:
/// - a chain step, the upgrade's start or handover, or a behavioral assertion fails
pub(super) async fn upgrade_with_rotation_on(
    node: &mut Node,
    candidate: Address,
    signer: Address,
    predecessor: u16,
    staking: &Address,
    staking_start: i64,
) -> anyhow::Result<()> {
    let dir = node.dir.parent().context("the scenario's artifact directory")?.join("a-upgrade");
    std::fs::create_dir_all(&dir)?;
    let (port, handover) = (free_port()?, free_port()?);
    let config = dir.join("a-upgrade.toml");
    write_config(&config, &dir, port, free_port()?, &node.chain, &node.services, |cfg| {
        cfg["tee"]["use_published_mock_key"] = false.into();
        cfg["tee"]["sealing"] = json!({
            "dir": dir.join("keys").to_string_lossy(),
            "handover": { "listen": format!("127.0.0.1:{handover}"), "danger_accept_invalid_certs": true },
        });
        rotation_and_staking(cfg, &node.chain, staking, &dir, staking_start, true)
    })?;
    let config_hash: [u8; 32] = sha2::Sha256::digest(std::fs::read(&config)?).into();
    let digest = A_UPGRADE_DIGEST.to_vec();
    let move_to = setReleaseCall { launchDigest: digest.clone().into(), configHash: config_hash.into() }.abi_encode();
    ensure!(
        node.chain.impersonate_send(candidate, node.chain.registry, move_to.clone()).await.is_err(),
        "A's candidacy moved to a release the timelock never approved"
    );
    let approval = setReleaseApprovalCall {
        candidate,
        launchDigest: digest.into(),
        configHash: config_hash.into(),
        approved: true,
    };
    node.chain.govern_target(node.chain.registry, approval.abi_encode()).await?;
    let epoch = node.chain.read(node.chain.vault, signerEpochCall {}).await?;
    node.chain.impersonate_send(candidate, node.chain.registry, move_to).await?;
    node.chain.sync_release().await?;
    node.chain.wait_finalized_release(keccak256([A_UPGRADE_DIGEST.as_slice(), &config_hash].concat())).await?;

    // The upgrade starts with an empty log and takes the keys and every segment from A.
    let keys = format!("predecessor=127.0.0.1:{predecessor}");
    let mut child = spawn_release(&dir, &config, node, A_UPGRADE_DIGEST, Some(keys.as_str()))?;
    let client = Client::new(port, node.chain.vault)?;
    wait_serving(&client, &mut child_checker(&mut child)).await?;
    // A's old release, a rotation-set member, follows the upgrade once it has handed its keys over.
    wait_log(&node.dir.join("node.log"), "handed the keys over: following the successor", node).await?;
    node.stop().await.context("A's old release, following the upgrade, did not drain")?;
    let tip = last_seq(&node.dir.join("log"))?;
    timeout(LOADED_WAIT, async {
        while last_seq(&dir.join("log"))? <= tip {
            sleep(Duration::from_millis(200)).await;
        }
        anyhow::Ok(())
    })
    .await
    .with_context(|| format!("the upgrade did not continue past A's final checkpoint (seq {tip})"))??;
    wait_latest_seq(&mut node.chain, tip + 1).await?;
    let attested: Address = client.get("/v1/attestation").await?["enclave_address"]
        .as_str()
        .context("attested enclave")?
        .parse()
        .context("attested enclave address")?;
    ensure!(attested == signer, "the upgrade attests {attested}, not the signer the vault names");
    ensure!(
        node.chain.read(node.chain.vault, signerEpochCall {}).await? == epoch,
        "the upgrade changed the signer epoch"
    );
    let signers = [engine_core::types::AccountId(signer.into())].into_iter().collect();
    let replayed = crate::sequencer::replay_path_with_attested(&dir.join("log"), None, signers)?;
    ensure!(
        replayed.last_signed_checkpoint.as_ref().is_some_and(|cp| cp.seq > tip),
        "the upgrade's log has no checkpoint past A's final one"
    );

    // The old release's sealed copy no longer opens: the vault names the upgrade, which the old
    // release, a rotation-set member, follows instead.
    node.follows_without_its_keys(A_DIGEST).await?;
    println!(
        "PASS: the operator in effect upgraded with rotation on: the release the timelock approved took the keys and the log, and continued under the same signer"
    );
    Ok(())
}
