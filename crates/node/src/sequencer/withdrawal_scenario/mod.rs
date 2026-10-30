//! Opt-in local Anvil scenario over `node::run`; no production configuration enables these hooks.

mod client;
mod exit;
mod flows;
mod guardian;
mod keys;
mod node;
mod rpc;
mod services;

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, ensure};
use engine_core::command::{Command, Sequenced};
use engine_core::event::Event;
use engine_core::types::AccountId;
use serde::{Deserialize, Serialize};
use tokio::time::timeout;

use crate::config::{CustodyProvider, FeedProvider, NodeConfig, TeeProvider};

const CHILD_CONFIG: &str = "ENGINE_WITHDRAWAL_SCENARIO_CHILD_CONFIG";
const CHILD_FEED: &str = "ENGINE_WITHDRAWAL_SCENARIO_CHILD_FEED";
const CHILD_ALERTS: &str = "ENGINE_WITHDRAWAL_SCENARIO_CHILD_ALERTS";
/// `new-key`, or `predecessor=<host:port>`: the child's one-off choice of keys.
const CHILD_KEYS: &str = "ENGINE_WITHDRAWAL_SCENARIO_CHILD_KEYS";
/// The child's stand-in launch measurement, as `--mock-launch-digest` takes it.
const CHILD_LAUNCH_DIGEST: &str = "ENGINE_WITHDRAWAL_SCENARIO_CHILD_LAUNCH_DIGEST";
const CHILD_TEST: &str = "sequencer::withdrawal_scenario::node_child";

#[derive(Deserialize, Serialize)]
struct Barrier {
    account: AccountId,
    nonce: u64,
}

/// Blocks the armed child after RAM approval and before the first command log append.
/// Error cases:
/// - the test barrier cannot be read or its reached marker cannot be synced
pub(crate) fn before_approval_log(sequenced: &Sequenced, events: &[Event]) -> anyhow::Result<()> {
    let request = match &sequenced.cmd {
        Command::Withdraw { account, auth: Some(auth), .. } => Some((*account, auth.nonce)),
        Command::AccessWithdraw { request, .. } => request.auth.as_ref().map(|auth| (request.account, auth.nonce)),
        _ => None,
    };
    let Some((account, nonce)) = request else { return Ok(()) };
    if !events.iter().any(|event| matches!(event, Event::WithdrawalApproved { .. })) {
        return Ok(());
    }
    let Some(config) = std::env::var_os(CHILD_CONFIG) else { return Ok(()) };
    let config = PathBuf::from(config);
    let dir = config.parent().context("scenario config parent")?;
    let arm = dir.join("barrier.arm");
    let bytes = match std::fs::read(&arm) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let expected: Barrier = serde_json::from_slice(&bytes)?;
    if expected.account != account || expected.nonce != nonce {
        return Ok(());
    }
    let mut reached = OpenOptions::new().write(true).create_new(true).open(dir.join("barrier.reached"))?;
    reached.write_all(&serde_json::to_vec(sequenced)?)?;
    reached.sync_all()?;
    // The parent must SIGKILL this actual node process; the approval has not entered its log.
    loop {
        std::thread::park_timeout(Duration::from_secs(1));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "local Anvil end-to-end; scripts/dev-withdrawals.sh runs the parent only"]
async fn dev_scenario_signed_withdrawals_and_fifo() -> anyhow::Result<()> {
    timeout(Duration::from_secs(900), async {
        flows::payments_and_crashes().await?;
        flows::shortfall_and_fifo().await?;
        guardian::pause_and_retirement().await
    })
    .await
    .context("whole withdrawal scenario exceeded fifteen minutes")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "local Anvil end-to-end; scripts/dev-key-lifecycle.sh runs the parent only"]
async fn dev_scenario_key_lifecycle() -> anyhow::Result<()> {
    timeout(Duration::from_secs(900), keys::key_lifecycle())
        .await
        .context("the key lifecycle scenario exceeded fifteen minutes")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "local Anvil end-to-end; scripts/dev-exit.sh runs the parent only"]
async fn dev_scenario_exit_against_the_last_posted_root() -> anyhow::Result<()> {
    timeout(Duration::from_secs(600), exit::exit_against_the_last_posted_root())
        .await
        .context("the exit scenario exceeded ten minutes")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "child entry point; launched only by the local withdrawal scenario"]
async fn node_child() -> anyhow::Result<()> {
    let path = PathBuf::from(std::env::var_os(CHILD_CONFIG).context("scenario child config is required")?);
    let mut cfg = NodeConfig::load(&path)?;
    ensure!(
        cfg.tee.provider == TeeProvider::Mock && (cfg.tee.use_published_mock_key || cfg.tee.sealing.is_some()),
        "local mock signer only: the published key, or the stand-in for sealing"
    );
    let mock_launch_digest = match std::env::var(CHILD_LAUNCH_DIGEST) {
        Ok(text) => Some(crate::parse_launch_digest(&text).map_err(anyhow::Error::msg)?),
        Err(_) => None,
    };
    let keys = match std::env::var(CHILD_KEYS).ok().as_deref() {
        None => crate::KeyStart { mock_launch_digest, ..Default::default() },
        Some("new-key") => crate::KeyStart { new_key: true, predecessor: None, mock_launch_digest },
        Some(other) => {
            let address = other.strip_prefix("predecessor=").context("unknown scenario key start")?;
            crate::KeyStart { new_key: false, predecessor: Some(address.to_owned()), mock_launch_digest }
        }
    };
    ensure!(cfg.custody.provider == CustodyProvider::Evm && cfg.custody.chain_id == 31337, "local Anvil only");
    let endpoint = std::env::var(CHILD_FEED).context("scenario feed endpoint")?;
    ensure!(endpoint.starts_with("ws://127.0.0.1:"), "scenario feed is loopback only");
    let alerts = std::env::var(CHILD_ALERTS).context("scenario alert webhook")?;
    ensure!(alerts.starts_with("http://127.0.0.1:"), "scenario alert webhook is loopback only");
    // Production config still enforces venue-owned WSS domains. Only this test child substitutes
    // a loopback Bitstamp server after validation, keeping the real feed parser and node wiring.
    cfg.feeds.provider = FeedProvider::Venues;
    cfg.feeds.venues = Some(toml::from_str(&format!(
        "timeout='5s'\nreconnect_min='40ms'\nreconnect_max='200ms'\nstale_after='3s'\n\
         [[connections]]\nvenue='bitstamp'\nurl={endpoint:?}\nsymbols=[{{market=1,symbol='btcusd'}}]"
    ))?);
    let telemetry =
        crate::telemetry::init_telemetry(&cfg, |name| (name == "ENGINE_ALERTS_CRITICAL_URL").then(|| alerts.clone()))?;
    crate::node::run_with_cpu(cfg, telemetry, None, keys).await
}
