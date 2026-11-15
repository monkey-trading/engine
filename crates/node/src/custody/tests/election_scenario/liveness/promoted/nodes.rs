//! The promotion leg's real nodes: A, B and C as `node_child` processes, the node binary's own start
//! under the mock TEE, each on a config this module writes for its release, with its standby key
//! sealed for that release before it starts, as a member's first start seals one.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy::primitives::Address;
use anyhow::{Context, ensure};
use engine_core::types::AccountId;
use k256::ecdsa::SigningKey;
use serde_json::json;
use sha2::Digest as _;
use tokio::time::{sleep, timeout};

use crate::custody::tests::election_scenario::chain::Chain;
use crate::sequencer::withdrawal_scenario::client::Client;
use crate::sequencer::withdrawal_scenario::node::ChildProcess;
use crate::sequencer::withdrawal_scenario::services::Services;
use crate::sequencer::withdrawal_scenario::{CHILD_ALERTS, CHILD_CONFIG, CHILD_FEED, CHILD_LAUNCH_DIGEST, CHILD_TEST};
use crate::standby_key::StandbyKeys;
use crate::tee::sealing::Release;
use crate::test_support::{ANVIL_CONFIG, keep_scratch, scratch_dir};

/// How long a node gets to serve its API after it starts.
const SERVE_WAIT: Duration = Duration::from_secs(120);

/// What every member's config names alike: the chain and its contracts, the stand-ins, and the
/// genesis staking start, which each copy of the log must share.
pub(super) struct Deployment<'a> {
    pub chain: &'a Chain,
    pub services: &'a Services,
    pub schedule: Address,
    pub log_heads: Address,
    pub staking_start_ms: i64,
}

/// A member as the leg runs it: its candidate, admin key and release, its node's addresses,
/// directories and config, its standby key, and its process once started.
pub(super) struct Seat {
    pub name: &'static str,
    pub candidate: Address,
    pub admin: SigningKey,
    pub digest: [u8; 48],
    pub dir: PathBuf,
    pub api: SocketAddr,
    pub endpoint: SocketAddr,
    pub config_hash: [u8; 32],
    pub standby: AccountId,
    config: PathBuf,
    child: Option<ChildProcess>,
}

impl Seat {
    /// `name`, the candidate `candidate` with the admin key `admin` running `digest`, its liveness
    /// caller sending from `caller`: writes its config on free ports and seals its standby key.
    /// Error cases:
    /// - the config cannot be written
    pub(super) fn new(
        name: &'static str,
        (candidate, admin, digest): (Address, [u8; 32], [u8; 48]),
        caller: Address,
        deployment: &Deployment<'_>,
    ) -> anyhow::Result<Self> {
        let free = || std::net::TcpListener::bind("127.0.0.1:0").map(|listener| listener.local_addr());
        let (api, metrics, endpoint) = (free()??, free()??, free()??);
        let dir = scratch_dir(&format!("promoted-{name}"));
        std::fs::create_dir_all(dir.join("keys"))?;
        // The node's log and data stay for inspection after the leg.
        keep_scratch(&dir);
        let admin = SigningKey::from_slice(&admin)?;
        let config = dir.join("node.toml");
        write_config(&config, &dir, (api, metrics, endpoint), deployment, caller, &admin)?;
        let config_hash: [u8; 32] = sha2::Sha256::digest(std::fs::read(&config)?).into();
        let standby = StandbyKeys::sealed_for_test(&dir.join("keys"), digest, config_hash).address();
        Ok(Self { name, candidate, admin, digest, dir, api, endpoint, config_hash, standby, config, child: None })
    }

    /// Its release: its digest and its config file's hash.
    pub(super) fn release(&self) -> Release {
        Release { launch_digest: self.digest, config_sha256: self.config_hash }
    }

    /// Where its API serves, as the registry records its hostname.
    pub(super) fn hostname(&self) -> String {
        self.api.to_string()
    }

    /// A client of its API for the vault `vault`.
    /// Error cases:
    /// - the HTTP client cannot be built
    pub(super) fn client(&self, vault: Address) -> anyhow::Result<Client> {
        Client::new(self.api.port(), vault)
    }

    /// Starts its node as the node binary starts, with the scenario's feed and alert webhook.
    /// Error cases:
    /// - it already runs, or the child cannot spawn
    pub(super) fn start(&mut self, services: &Services) -> anyhow::Result<()> {
        ensure!(self.child.is_none(), "{} already runs", self.name);
        let digest = hex::encode(self.digest);
        let config = self.config.to_str().context("the config path is UTF-8")?;
        let env = [
            (CHILD_CONFIG, config),
            (CHILD_FEED, services.feed.as_str()),
            (CHILD_ALERTS, services.alerts.as_str()),
            (CHILD_LAUNCH_DIGEST, digest.as_str()),
        ];
        let args = ["--ignored", "--exact", CHILD_TEST, "--nocapture", "--test-threads=1"];
        self.child = Some(ChildProcess::spawn(std::env::current_exe()?, &args, &self.log(), &env, None)?);
        Ok(())
    }

    /// Kills its node at once, as a crash does.
    /// Error cases:
    /// - it does not run, or the kill fails
    pub(super) async fn kill(&mut self) -> anyhow::Result<()> {
        self.child.take().context("not running")?.kill().await
    }

    /// Stops its node by the ordinary drain.
    /// Error cases:
    /// - it does not run, or the drain fails
    pub(super) async fn stop(&mut self) -> anyhow::Result<()> {
        self.child.take().context("not running")?.stop().await
    }

    /// Fails if its node has exited.
    /// Error cases:
    /// - the child exited
    pub(super) fn check(&mut self) -> anyhow::Result<()> {
        match self.child.as_mut() {
            Some(child) => {
                child.check().with_context(|| format!("{}'s node exited: {}", self.name, self.log().display()))
            }
            None => Ok(()),
        }
    }

    /// Waits until its API serves a priced market.
    /// Error cases:
    /// - the node exits, or does not serve within `SERVE_WAIT`
    pub(super) async fn serving(&mut self, vault: Address) -> anyhow::Result<()> {
        let client = self.client(vault)?;
        timeout(SERVE_WAIT, async {
            loop {
                self.check()?;
                if client.mark().await.is_ok() {
                    return anyhow::Ok(());
                }
                sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .with_context(|| format!("{}'s node did not serve: {}", self.name, self.log().display()))?
    }

    /// Its node's log file.
    pub(super) fn log(&self) -> PathBuf {
        self.dir.join("node.log")
    }

    /// Its node's log directory.
    pub(super) fn log_dir(&self) -> PathBuf {
        self.dir.join("log")
    }
}

/// Writes a member's config to `path`: the Anvil template on the scenario's chain, Roughtime, feed and
/// screening stand-ins, one controlled market, rotation on with the staking mirror and schedule,
/// `[log_heads]`, `[liveness]` calling from `caller`, sealing with its handover listener at
/// `endpoint`, and `admin` as its operator key.
/// Error cases:
/// - the template does not parse or the file cannot be written
fn write_config(
    path: &Path,
    dir: &Path,
    (api, metrics, endpoint): (SocketAddr, SocketAddr, SocketAddr),
    deployment: &Deployment<'_>,
    caller: Address,
    admin: &SigningKey,
) -> anyhow::Result<()> {
    let Deployment { chain, services, schedule, log_heads, staking_start_ms } = deployment;
    let path_of = |name: &str| dir.join(name).to_string_lossy().into_owned();
    let source: toml::Value = toml::from_str(&std::fs::read_to_string(ANVIL_CONFIG)?)?;
    let mut cfg = serde_json::to_value(source)?;
    cfg["node"]["listen"] = api.to_string().into();
    cfg["node"]["log_dir"] = path_of("log").into();
    cfg["node"]["checkpoint_every"] = 64_i64.into();
    cfg["node"]["tick"] = "100ms".into();
    cfg["history"]["dir"] = path_of("history").into();
    cfg["metrics"]["listen"] = metrics.to_string().into();
    cfg["alerts"]["outbox"]["dir"] = path_of("alerts").into();
    cfg["custody"]["vault"] = format!("{:#x}", chain.vault).into();
    let evm = &mut cfg["custody"]["evm"];
    evm["usdc"] = format!("{:#x}", chain.usdc).into();
    evm["providers"] = chain
        .proxies
        .iter()
        .enumerate()
        .map(|(index, url)| json!({ "name": format!("scenario-{index}"), "rpc_url": url }))
        .collect();
    evm["finality_tag"] = "latest".into();
    evm["confirmations"] = 0_i64.into();
    evm["poll_interval"] = "80ms".into();
    evm["cursor_path"] = path_of("custody-cursor.json").into();
    evm["staking"] = json!({
        "contract": format!("{:#x}", chain.staking),
        "cursor_path": path_of("staking-cursor.json"),
        "registry": { "contract": format!("{:#x}", chain.registry), "schedule": format!("{schedule:#x}") },
    });
    cfg["time"]["interval"] = "500ms".into();
    cfg["time"]["max_drift"] = "5s".into();
    cfg["time"]["servers"] = serde_json::Value::Array(services.time.clone());
    cfg["feeds"]["mock_mids"] = serde_json::Value::Array(Vec::new());
    cfg["feeds"]["interval"] = "80ms".into();
    one_market(&mut cfg["genesis"])?;
    cfg["genesis"]["staking"] = json!({
        "liquidity_floor": "1000000",
        "epoch_ms": 30 * 60 * 1000,
        "start_ms": staking_start_ms,
        "cooldown_ms": 60 * 60 * 1000,
    });
    let table = cfg.as_object_mut().context("config table")?;
    table.insert(
        "checkpoints".into(),
        json!({ "rpc_url": chain.url, "relayer": format!("{:#x}", chain.accounts[8]),
                "retry_interval": "80ms", "rpc_timeout": "3s" }),
    );
    table.insert(
        "log_heads".into(),
        json!({ "contract": format!("{log_heads:#x}"), "relayer": format!("{:#x}", chain.accounts[5]), "interval": "1h" }),
    );
    table.insert("liveness".into(), json!({ "caller": format!("{caller:#x}"), "min_balance": "0.01" }));
    table.insert(
        "access".into(),
        json!({
            "source": engine_core::eip712::address(crate::test_support::access::key(0x77).verifying_key()).to_string(),
            "endpoint": services.access, "timeout": "2s", "max_age": "30s",
            "max_response_bytes": 4096, "max_in_flight": 8,
        }),
    );
    cfg["operator"]["rotation_enabled"] = true.into();
    let public = admin.verifying_key().to_sec1_point(false);
    cfg["operator"]["public_key"] = format!("0x{}", hex::encode(public.as_bytes())).into();
    cfg["tee"]["use_published_mock_key"] = false.into();
    cfg["tee"]["sealing"] = json!({
        "dir": path_of("keys"),
        "handover": { "listen": endpoint.to_string(), "danger_accept_invalid_certs": true },
    });
    std::fs::write(path, toml::to_string(&toml::Value::try_from(cfg)?)?)?;
    Ok(())
}

/// Keeps the genesis's first market, priced by one source, fee-free and with a house cap any
/// scenario fill fits, as the withdrawal scenarios' configs do.
/// Error cases:
/// - the template's genesis lacks a market or stress levels
fn one_market(genesis: &mut serde_json::Value) -> anyhow::Result<()> {
    genesis["markets"].as_array_mut().context("genesis markets")?.truncate(1);
    let market = &mut genesis["markets"][0];
    market["price_rules"]["min_sources"] = 1_i64.into();
    market["price_rules"]["max_age_ms"] = 3000_i64.into();
    market["house_cap_floor"] = "1000000".into();
    market["spread_tiers"] = json!([{ "tenth_bps": 10 }]);
    market["margin_tiers"][0]["bps"] = 990_i64.into();
    market["financing"]["markup_bps"] = 0_i64.into();
    genesis["benchmarks"][0]["annual_bps"] = 0_i64.into();
    genesis["params"]["profit_fee_bps"] = 0_i64.into();
    genesis["params"]["profit_fee_curve"] = json!([]);
    genesis["params"]["house_total_cap_floor"] = "1000000".into();
    genesis["params"]["stress_levels"].as_array_mut().context("stress levels")?.truncate(1);
    Ok(())
}
