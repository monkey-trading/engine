//! A standby takes over from a failed operator: A runs with rotation on and payouts paused, B
//! follows A's real stream into a copy, the liveness rule promotes B, the timelock installs only
//! B's registered enclave, B resumes from A's last vault-accepted checkpoint, and the durable
//! authorization reissued under B's signer epoch pays once.

use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::sol_types::SolCall;
use anyhow::{Context, ensure};
use engine_core::event::Event;
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use serde_json::json;
use sha2::Digest as _;
use tokio::time::{sleep, timeout};

use crate::clock::now_ms;
use crate::log::{LogEntry, SegmentedLog};
use crate::sequencer::SignedWithdrawal;
use crate::sequencer::replay_path_with_attested;
use crate::sequencer::withdrawal_scenario::client::{Client, Stream, User};
use crate::sequencer::withdrawal_scenario::flows::{entries, last_seq};
use crate::sequencer::withdrawal_scenario::node::{ChildProcess, Node, free_port, write_config};
use crate::sequencer::withdrawal_scenario::rpc::{
    Chain, currentReleaseCall, executeCall, latestRootCall, latestSeqCall, operatorInEffectCall, pauseWithdrawalsCall,
    pendingElectionCall, promoteCall, scheduleCall, setEnclaveCall, setPausesCall, setSignerCall,
};
use crate::sequencer::withdrawal_scenario::{
    CHILD_ALERTS, CHILD_CONFIG, CHILD_FEED, CHILD_KEYS, CHILD_LAUNCH_DIGEST, CHILD_STANDBY, CHILD_TEST,
};
use crate::test_support::{ANVIL_CONFIG, LOADED_WAIT};

/// The liveness timeout the registry deploys with: long enough for B to follow and short enough to
/// wait out. A test value, not an approved production parameter (#360).
const LIVENESS_SECS: u64 = 30;
const B_DIGEST: [u8; 48] = [0x71; 48];
const B_ADMIN: [u8; 32] = [0x63; 32];

/// The dev scenario: see the module docs.
/// Error cases:
/// - any real node, chain, or stream step or behavioral assertion fails
pub(super) async fn standby_takes_over() -> anyhow::Result<()> {
    let root = std::env::var_os("SCENARIO_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("engine-standby-{}", std::process::id())));
    let dir = root.join("standby-takeover");
    std::fs::create_dir_all(&dir)?;
    ensure!(!dir.join("a.toml").exists(), "scenario needs a fresh artifact directory: {}", dir.display());
    let mut chain = Chain::start(&dir).await?;
    let domain = engine_core::eip712::domain(31337, AccountId(chain.vault.into()));
    let services = super::services::Services::start(&chain.url, chain.mining.clone(), domain).await?;

    // The operator in effect is the template config's key, as the registry's initial operator.
    let template: toml::Value = toml::from_str(&std::fs::read_to_string(ANVIL_CONFIG)?)?;
    let operator_key = template["operator"]["public_key"].as_str().context("operator key")?.to_owned();
    let operator_key =
        k256::ecdsa::VerifyingKey::from_sec1_bytes(&hex::decode(operator_key.trim_start_matches("0x"))?)?;
    let operator_address = Address::from(engine_core::eip712::address(&operator_key).0);
    let staking = chain.deploy_registry(operator_address, [0x11; 48], [0x22; 32], LIVENESS_SECS).await?;
    // A and B's genesis must match exactly, staking start included.
    let staking_start = now_ms() + 30_000;

    // With no eligible standby, promotion reverts and the operator in effect continues.
    chain.lapse_liveness(LIVENESS_SECS + 60).await?;
    chain.refuses_call(chain.registry, promoteCall {}.abi_encode()).await?;
    let in_effect = chain.read(chain.registry, operatorInEffectCall {}).await?;
    ensure!(in_effect.candidate == operator_address, "the failed promotion moved the operator");

    // B registers as the standby: self-bond, approved release, self-delegation. Its release is the
    // normal config's hash, which must exist before the registration names it.
    let b_admin = SigningKey::from_slice(&B_ADMIN)?;
    let b_admin_address = Address::from(engine_core::eip712::address(b_admin.verifying_key()).0);
    let b_candidate = chain.funder;
    let b_dir = dir.join("b");
    std::fs::create_dir_all(&b_dir)?;
    let b_port = free_port()?;
    let b_metrics = free_port()?;
    let b_config_path = b_dir.join("normal.toml");
    write_config(&b_config_path, &b_dir, b_port, b_metrics, &chain, &services, |cfg| {
        cfg["tee"]["use_published_mock_key"] = false.into();
        cfg["tee"]["sealing"] =
            json!({ "dir": b_dir.join("keys").to_string_lossy(), "handover": { "listen": "127.0.0.1:0" } });
        rotation_and_staking(cfg, &chain, &staking, &b_dir, staking_start, true)
    })?;
    let b_config_hash: [u8; 32] = sha2::Sha256::digest(std::fs::read(&b_config_path)?).into();
    chain
        .register_candidate(b_candidate, &b_admin, U256::from(10_000), B_DIGEST, b_config_hash, operator_address)
        .await?;
    chain.delegate_self(b_candidate).await?;

    // A boots with rotation on and the staking mirror configured.
    let a_port = free_port()?;
    let a_metrics = free_port()?;
    let a_dir = dir.join("a");
    std::fs::create_dir_all(&a_dir)?;
    let a_config = a_dir.join("node.toml");
    write_config(&a_config, &a_dir, a_port, a_metrics, &chain, &services, |cfg| {
        rotation_and_staking(cfg, &chain, &staking, &a_dir, staking_start, true)
    })?;
    let vault: Address = {
        let cfg: toml::Value = toml::from_str(&std::fs::read_to_string(&a_config)?)?;
        cfg["custody"]["vault"].as_str().context("custody.vault")?.parse().context("custody.vault address")?
    };
    let mut node = Node::uninitialized(a_dir, chain, services, Client::new(a_port, vault)?, a_config, a_metrics);
    node.restart().await?;
    let mut node = node;

    // A client deposits; the guardian pauses payouts, then the withdrawal is approved but no
    // authorization is signed while A runs.
    let trader = User::new(1)?;
    node.chain.deposit(trader.address(), 100, false).await?;
    node.client.declare(&trader).await?;
    node.client.wait_balance(&trader, "100").await?;
    node.client.wait_finalised(&trader).await?;
    node.wait_withdrawals_enabled(last_seq(&node.dir.join("log"))?).await?;
    let request = node.client.withdrawal(&trader, "10").await?;
    let events = node.client.request_withdrawal(&request).await?;
    ensure!(
        events
            .iter()
            .any(|event| matches!(event, Event::WithdrawalApproved { account, .. } if *account == trader.account)),
        "the withdrawal was not approved: {events:?}"
    );
    // A signs the authorization while payouts are open; the pause then holds it unclaimed until
    // the takeover, so the reissue must retire a real outstanding authorization.
    let mut a_stream = node.client.stream().await?;
    let issued = a_stream.payout(trader.account, 10).await?;
    node.chain.send(node.chain.guardian(), node.chain.vault, pauseWithdrawalsCall {}.abi_encode()).await?;
    drop(a_stream);
    // A's checkpoints keep posting while payouts are paused; wait until one covers the approval.
    let approval_seq = last_seq(&node.dir.join("log"))?;
    timeout(LOADED_WAIT, async {
        loop {
            if node.chain.read(node.chain.vault, latestSeqCall {}).await? >= approval_seq {
                return anyhow::Ok(());
            }
            node.chain.check()?;
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("no checkpoint covered the approval before the takeover")??;
    let takeover_seq = node.chain.read(node.chain.vault, latestSeqCall {}).await?;
    let takeover_root: B256 = node.chain.read(node.chain.vault, latestRootCall {}).await?;

    // B follows A's real stream into its copy until the copy holds A's latest checkpoint.
    let b_standby_config = b_dir.join("standby.toml");
    write_config(&b_standby_config, &b_dir, b_port, b_metrics, &node.chain, &node.services, |cfg| {
        cfg["operator"]["public_key"] = hex::encode(b_admin.verifying_key().to_sec1_point(false).as_bytes()).into();
        cfg["tee"]["use_published_mock_key"] = false.into();
        cfg["standby"] = json!({
            "operator": format!("https://127.0.0.1:{a_port}"),
            "enclave": node.chain.signer.to_string(),
            "poll_interval": "1s",
            "danger_accept_invalid_certs": true,
        });
        Ok(())
    })?;
    let mut b_child = spawn_b(&b_dir, &b_standby_config, &node, None, None, true)?;
    let b_log = b_dir.join("log");
    timeout(LOADED_WAIT, async {
        loop {
            if copy_holds_checkpoint(&b_log, takeover_seq, takeover_root)? {
                return anyhow::Ok(());
            }
            b_child.check()?;
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("B's copy never held A's latest checkpoint")??;

    // A fails; the liveness rule promotes B; the vault's release moves to B's at once; B's standby
    // finalizes its copy and exits for a normal start.
    let mut node = node;
    node.crash().await?;
    node.chain.lapse_liveness(LIVENESS_SECS + 60).await?;
    node.chain.promote().await?;
    let pending = node.chain.read(node.chain.registry, pendingElectionCall {}).await?;
    ensure!(pending.adminKey == b_admin_address, "the promotion names another candidate: {}", pending.adminKey);
    ensure!(
        pending.release == keccak256([B_DIGEST.as_slice(), &b_config_hash].concat()),
        "the promotion names another release"
    );
    let in_effect = node.chain.read(node.chain.registry, operatorInEffectCall {}).await?;
    ensure!(in_effect.candidate == operator_address, "the admin key moved before its notice");
    ensure!(
        node.chain.read(node.chain.vault, currentReleaseCall {}).await? == pending.release,
        "the vault's current release did not move to the standby at once"
    );
    let status = b_child.exited(LOADED_WAIT).await?;
    ensure!(status.success(), "B's standby exited with {status}");
    let copy = entries(&b_log)?;
    let a_tip = entries(&node.dir.join("log"))?
        .into_iter()
        .rev()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .context("A's log ends in no checkpoint")?;
    ensure!(
        a_tip.seq >= takeover_seq,
        "A published no checkpoint at or past the vault's latest: tip seq {} < {takeover_seq}",
        a_tip.seq
    );
    ensure!(
        matches!(copy.last(), Some(LogEntry::Checkpoint(checkpoint))
            if checkpoint.seq == a_tip.seq && checkpoint.root == a_tip.root),
        "B's copy does not end at the latest published checkpoint A signed: copy {copy:?}, A's tip {a_tip:?}"
    );

    // The vault names B's release at finalized before a --new-key start reads it there.
    node.chain.wait_finalized_release(pending.release).await?;

    // The timelock's setSigner accepts only the enclave B registered: scheduling the admin key
    // and executing it reverts, through the timelock's own path.
    let salt = alloy::primitives::B256::from(rand::random::<[u8; 32]>());
    let payload = setSignerCall { signer: b_admin_address }.abi_encode();
    node.chain
        .send(
            node.chain.deployer,
            node.chain.timelock,
            scheduleCall {
                target: node.chain.vault,
                value: U256::ZERO,
                data: payload.clone().into(),
                predecessor: alloy::primitives::B256::ZERO,
                salt,
                delay: U256::from(1),
            }
            .abi_encode(),
        )
        .await?;
    node.chain
        .refuses_call(
            node.chain.timelock,
            executeCall {
                target: node.chain.vault,
                value: U256::ZERO,
                payload: payload.into(),
                predecessor: alloy::primitives::B256::ZERO,
                salt,
            }
            .abi_encode(),
        )
        .await?;

    // B starts with fresh keys and serves only its attestation until the timelock installs it.
    let mut b_child = spawn_b(&b_dir, &b_config_path, &node, Some("new-key"), Some(B_DIGEST), false)?;
    let b_client = Client::new(b_port, node.chain.vault)?;
    let b_enclave = timeout(LOADED_WAIT, async {
        loop {
            if let Ok(value) = b_client.get("/v1/attestation").await {
                return anyhow::Ok(value["enclave_address"].as_str().context("attested enclave")?.to_owned());
            }
            b_child.check()?;
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("B never served its attestation")??;
    let b_enclave: Address = b_enclave.parse().context("B's attested enclave")?;
    // For a few seconds B must answer nothing but its attestation.
    for _ in 0..15 {
        ensure!(
            b_client.get("/v1/markets").await.is_err(),
            "B answered an API route before the timelock installed its address"
        );
        b_child.check()?;
        sleep(Duration::from_millis(200)).await;
    }
    node.chain.send(b_candidate, node.chain.registry, setEnclaveCall { enclave: b_enclave }.abi_encode()).await?;
    node.chain.govern(setSignerCall { signer: b_enclave }.abi_encode()).await?;
    timeout(LOADED_WAIT, async {
        loop {
            if b_client.get("/v1/markets").await.is_ok() {
                return anyhow::Ok(());
            }
            b_child.check()?;
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("B never resumed after the timelock installed its address")??;

    // Unpaused, B reissues the durable authorization under its signer epoch and it pays once.
    node.chain.govern(setPausesCall { depositsPaused: false, withdrawalsPaused: false }.abi_encode()).await?;
    // The authorization A signed retired with its epoch: the vault rejects it, and only the
    // reissue under B's epoch pays.
    node.chain.refuses(node.chain.claim(&issued)?.abi_encode()).await?;
    let mut stream = b_client.stream().await?;
    let reissued = payout_at_epoch(&mut stream, trader.account, 10, 2).await?;
    ensure!(
        (reissued.nonce, reissued.approved_seq) == (issued.nonce, issued.approved_seq),
        "the reissue changed the approval"
    );
    pay_b(&node.chain, &reissued).await?;
    let signed: Vec<_> = entries(&b_log)?
        .into_iter()
        .filter_map(|entry| match entry {
            LogEntry::Withdrawal(withdrawal) if withdrawal.nonce == 1 => Some(withdrawal),
            _ => None,
        })
        .collect();
    ensure!(
        signed == [issued.clone(), reissued.clone()],
        "A's issue and B's one reissue are exactly what signed: {signed:?}"
    );
    drop(stream);

    // The log verifies across the change of signer, given each signer.
    let signers: std::collections::BTreeSet<_> = [node.chain.signer, AccountId(b_enclave.0.0)].into_iter().collect();
    let replayed = replay_path_with_attested(&b_log, None, signers.clone())?;
    ensure!(replayed.checkpoint_signers == signers, "the replay did not name both signers");
    println!(
        "PASS: the liveness rule promotes the standby, the limited setSigner installs only the \
         registered enclave, B resumes at the latest published checkpoint the outgoing key signed, \
         the retired authorization reverts and the reissue pays once, and the log replays across \
         the signer change"
    );
    Ok(())
}

/// Sets `operator.rotation_enabled` and the staking mirror on a scenario config.
fn rotation_and_staking(
    cfg: &mut serde_json::Value,
    chain: &Chain,
    staking: &Address,
    dir: &Path,
    staking_start: i64,
    rotation: bool,
) -> anyhow::Result<()> {
    cfg["operator"]["rotation_enabled"] = rotation.into();
    cfg["genesis"]["staking"] = json!({
        "liquidity_floor": "1000000",
        "epoch_ms": 30 * 60 * 1000,
        "start_ms": staking_start,
        "cooldown_ms": 60 * 60 * 1000,
    });
    cfg["custody"]["evm"]["staking"] = json!({
        "contract": format!("{staking:#x}"),
        "cursor_path": dir.join("staking-cursor.json").to_string_lossy(),
        "registry": { "contract": format!("{:#x}", chain.registry) },
    });
    Ok(())
}

/// Whether the copy in `dir` holds the checkpoint at `seq` with `root`.
fn copy_holds_checkpoint(dir: &Path, seq: u64, root: B256) -> anyhow::Result<bool> {
    let mut found = false;
    SegmentedLog::open(dir, |entry| {
        if let LogEntry::Checkpoint(checkpoint) = &entry {
            found |= checkpoint.seq == seq && checkpoint.root == format!("0x{}", hex::encode(root.0));
        }
        Ok(())
    })?;
    Ok(found)
}

/// Spawns B as a child: the standby, or its normal start with `keys`.
fn spawn_b(
    b_dir: &Path,
    config: &Path,
    node: &Node,
    keys: Option<&str>,
    digest: Option<[u8; 48]>,
    standby: bool,
) -> anyhow::Result<ChildProcess> {
    let path = config.to_str().context("B config path is UTF-8")?;
    let feed = node.services.feed.clone();
    let alerts = node.services.alerts.clone();
    let digest_hex = digest.map(hex::encode);
    let mut env: Vec<(&str, &str)> = vec![(CHILD_CONFIG, path), (CHILD_FEED, &feed), (CHILD_ALERTS, &alerts)];
    if let Some(keys) = keys {
        env.push((CHILD_KEYS, keys));
    }
    if let Some(digest) = &digest_hex {
        env.push((CHILD_LAUNCH_DIGEST, digest));
    }
    if standby {
        env.push((CHILD_STANDBY, "1"));
    }
    let log = b_dir.join(format!("node-{}.log", keys.unwrap_or("standby")));
    ChildProcess::spawn(
        std::env::current_exe()?,
        &["--ignored", "--exact", CHILD_TEST, "--nocapture", "--test-threads=1"],
        &log,
        &env,
        Some(b_dir),
    )
}

/// The account's next stream authorization of `amount` signed in `epoch`.
async fn payout_at_epoch(
    stream: &mut Stream,
    account: AccountId,
    amount: i64,
    epoch: u64,
) -> anyhow::Result<SignedWithdrawal> {
    loop {
        let payout = stream.payout(account, amount).await?;
        if payout.authorization.as_ref().is_some_and(|auth| auth.signer_epoch == epoch) {
            return Ok(payout);
        }
    }
}

/// Pays the authorization on the chain and requires the duplicate to revert.
async fn pay_b(chain: &Chain, signed: &SignedWithdrawal) -> anyhow::Result<()> {
    let signer =
        signed.authorization.as_ref().map(|auth| auth.signer).context("the reissue is a typed authorization")?;
    let claim = chain.claim_from(signed, signer)?.abi_encode();
    chain.send(chain.payer, chain.vault, claim.clone()).await?;
    chain.duplicate_reverts(claim).await
}
