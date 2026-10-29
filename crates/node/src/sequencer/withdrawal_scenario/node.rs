//! Actual node child processes and isolated deployment-specific TOML, with no global env mutation.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, ensure};
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout};

use crate::sequencer::withdrawal_scenario::client::Client;
use crate::sequencer::withdrawal_scenario::rpc::Chain;
use crate::sequencer::withdrawal_scenario::services::Services;
use crate::sequencer::withdrawal_scenario::{CHILD_ALERTS, CHILD_CONFIG, CHILD_FEED, CHILD_TEST};
use crate::test_support::ANVIL_CONFIG;
use crate::test_support::LOADED_WAIT;

pub(super) struct ChildProcess(Child);

pub(super) struct Node {
    child: Option<ChildProcess>,
    pub dir: PathBuf,
    pub chain: Chain,
    pub services: Services,
    pub client: Client,
    config: PathBuf,
    metrics: u16,
}

impl ChildProcess {
    /// Starts a child whose output stays in the scenario artifact directory and dies on drop.
    /// Error cases:
    /// - the log cannot open or the child cannot spawn
    pub fn spawn(program: impl AsRef<OsStr>, args: &[&str], log: &Path, env: &[(&str, &str)]) -> anyhow::Result<Self> {
        let output = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
        let mut command = Command::new(program);
        command
            .args(args)
            .envs(env.iter().copied())
            .kill_on_drop(true)
            .stdout(Stdio::from(output.try_clone()?))
            .stderr(Stdio::from(output));
        Ok(Self(command.spawn()?))
    }

    /// Refuses to proceed if a supervised child has already stopped.
    /// Error cases:
    /// - status polling fails or the child exited
    pub fn check(&mut self) -> anyhow::Result<()> {
        ensure!(self.0.try_wait()?.is_none(), "scenario child exited; inspect its local log");
        Ok(())
    }

    /// Sends actual SIGKILL and reaps the stopped child under a deadline.
    /// Error cases:
    /// - killing/reaping fails or exceeds ten seconds
    pub async fn kill(&mut self) -> anyhow::Result<()> {
        timeout(LOADED_WAIT, self.0.kill()).await.context("SIGKILL deadline")??;
        Ok(())
    }

    /// Uses the ordinary SIGTERM drain path and requires a successful test child exit.
    /// Error cases:
    /// - signaling, draining or the child's final result fails
    pub async fn stop(&mut self) -> anyhow::Result<()> {
        let id = self.0.id().context("node child already reaped")?;
        let signal = timeout(LOADED_WAIT, Command::new("kill").args(["-TERM", &id.to_string()]).status()).await??;
        ensure!(signal.success(), "could not signal local node");
        let status = timeout(LOADED_WAIT, self.0.wait()).await.context("node drain deadline")??;
        ensure!(status.success(), "node child did not drain successfully");
        Ok(())
    }
}

impl Node {
    /// Starts a fresh Vault deployment and an ordinary node through the ignored child entry point.
    /// Error cases:
    /// - setup, node startup, or the API/genesis deadline fails
    pub async fn start(name: &str) -> anyhow::Result<Self> {
        let root = std::env::var_os("SCENARIO_LOG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join(format!("engine-withdrawals-{}", std::process::id())));
        let dir = root.join(name);
        std::fs::create_dir_all(&dir)?;
        let config = dir.join("node.toml");
        ensure!(!config.exists(), "scenario needs a fresh artifact directory: {}", dir.display());
        let chain = Chain::start(&dir).await?;
        let domain = engine_core::eip712::domain(31337, engine_core::types::AccountId(chain.vault.into()));
        let services = Services::start(&chain.url, chain.mining.clone(), domain).await?;
        let api_port = free_port()?;
        let metrics = free_port()?;
        write_config(&config, &dir, api_port, metrics, &chain, &services)?;
        let client = Client::new(api_port, chain.vault)?;
        let mut node = Self { dir, chain, services, client, config, metrics, child: None };
        node.restart().await?;
        Ok(node)
    }

    /// Starts/restarts from the same log and custody cursor with the published mock signer.
    /// Error cases:
    /// - child startup or the fully booted API deadline fails
    pub async fn restart(&mut self) -> anyhow::Result<()> {
        ensure!(self.child.is_none(), "stop the previous scenario node first");
        let path = self.config.to_str().context("test config path is UTF-8")?;
        self.child = Some(ChildProcess::spawn(
            std::env::current_exe()?,
            &["--ignored", "--exact", CHILD_TEST, "--nocapture", "--test-threads=1"],
            &self.dir.join("node.log"),
            &[(CHILD_CONFIG, path), (CHILD_FEED, &self.services.feed), (CHILD_ALERTS, &self.services.alerts)],
        )?);
        timeout(LOADED_WAIT, async {
            loop {
                self.services.check()?;
                self.chain.check()?;
                self.child.as_mut().context("spawned child")?.check()?;
                if self.client.mark().await.is_ok() {
                    return anyhow::Ok(());
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("actual node API and controlled market startup; verify test bootstrap hook is wired")??;
        Ok(())
    }

    /// Waits until the custody checkpoint gate admits withdrawals on a verdict that covers
    /// sequence `after` or later, so a healthy verdict taken before a credit cannot satisfy it. A
    /// restarted node starts paused, so `0` waits for its own first verdict.
    /// Error cases:
    /// - the metrics endpoint never reports a covering healthy verdict
    pub async fn wait_withdrawals_enabled(&mut self, after: u64) -> anyhow::Result<()> {
        self.wait_metrics(|_, scrape| gate_admits(scrape, after))
            .await
            .context("withdrawals stayed paused; custody never reconciled")
    }

    /// Waits until the node pages the Critical `vault_withdrawals_halted` for `cause` and its gate
    /// refuses withdrawals on a verdict covering sequence `after` or later.
    /// Error cases:
    /// - the page or a covering refusing verdict never arrives
    pub async fn wait_withdrawals_halted(&mut self, cause: &str, after: u64) -> anyhow::Result<()> {
        self.wait_metrics(|services, scrape| {
            services.paged("vault_withdrawals_halted", cause) && gate_refuses(scrape, after)
        })
        .await
        .with_context(|| format!("the node never paged and refused withdrawals for {cause}"))
    }

    /// Waits until the gate refuses withdrawals on a verdict covering sequence `after` or later,
    /// as a node restarted during a halt does without paging again: its episode is still open.
    /// Error cases:
    /// - a covering refusing verdict never arrives
    pub async fn wait_withdrawals_refused(&mut self, after: u64) -> anyhow::Result<()> {
        self.wait_metrics(|_, scrape| gate_refuses(scrape, after))
            .await
            .context("the restarted node never refused withdrawals on a fresh verdict")
    }

    /// Polls the metrics endpoint until `done` holds for the local services and a scrape.
    /// Error cases:
    /// - a scenario server or the chain stopped, the endpoint fails, or the deadline passes
    async fn wait_metrics(&mut self, done: impl Fn(&Services, &str) -> bool) -> anyhow::Result<()> {
        timeout(LOADED_WAIT, async {
            loop {
                self.services.check()?;
                self.chain.check()?;
                let text = reqwest::get(format!("http://127.0.0.1:{}/metrics", self.metrics))
                    .await
                    .context("metrics endpoint")?
                    .text()
                    .await?;
                if done(&self.services, &text) {
                    return anyhow::Ok(());
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await?
    }

    /// Kills the actual running node and preserves its files for restart.
    /// Error cases:
    /// - the child is absent or cannot be killed/reaped
    pub async fn crash(&mut self) -> anyhow::Result<()> {
        self.child.take().context("running node")?.kill().await
    }

    /// Drains the actual node before final replay and log assertions.
    /// Error cases:
    /// - the child is absent or fails ordinary shutdown
    pub async fn stop(&mut self) -> anyhow::Result<()> {
        self.child.take().context("running node")?.stop().await
    }
}

/// Whether a metrics scrape shows withdrawals admitted on a verdict for checkpoint `after` or later.
pub(super) fn gate_admits(metrics: &str, after: u64) -> bool {
    metrics.lines().any(|line| line == "engine_withdrawals_paused 0") && verdict_covers(metrics, after)
}

/// Whether a metrics scrape shows withdrawals refused on a verdict for checkpoint `after` or later.
fn gate_refuses(metrics: &str, after: u64) -> bool {
    metrics.lines().any(|line| line == "engine_withdrawals_paused 1") && verdict_covers(metrics, after)
}

/// Whether the scrape's latest custody verdict is for checkpoint `after` or later.
fn verdict_covers(metrics: &str, after: u64) -> bool {
    metrics
        .lines()
        .find_map(|line| line.strip_prefix("engine_withdrawals_verdict_seq "))
        .and_then(|seq| seq.parse::<u64>().ok())
        .is_some_and(|seq| seq >= after)
}

/// Reserves an unused loopback TCP port for a subsequently spawned process.
/// Error cases:
/// - the operating system cannot allocate a loopback listener
pub(super) fn free_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// Rewrites only a fresh local config; all custody and API paths still use ordinary validation.
/// Error cases:
/// - config parsing/encoding or writing the private scenario file fails
fn write_config(
    path: &Path,
    dir: &Path,
    api: u16,
    metrics: u16,
    chain: &Chain,
    services: &Services,
) -> anyhow::Result<()> {
    let source: toml::Value = toml::from_str(&std::fs::read_to_string(ANVIL_CONFIG)?)?;
    let mut cfg = serde_json::to_value(source)?;
    cfg["node"]["listen"] = format!("127.0.0.1:{api}").into();
    cfg["node"]["log_dir"] = dir.join("log").to_string_lossy().into_owned().into();
    cfg["node"]["checkpoint_every"] = 64_i64.into();
    cfg["node"]["segment_commands"] = 1000_i64.into();
    cfg["node"]["tick"] = "100ms".into();
    cfg["history"]["dir"] = dir.join("history").to_string_lossy().into_owned().into();
    cfg["metrics"]["listen"] = format!("127.0.0.1:{metrics}").into();
    cfg["alerts"]["outbox"]["dir"] = dir.join("alerts").to_string_lossy().into_owned().into();
    cfg["custody"]["vault"] = format!("{:#x}", chain.vault).into();
    cfg["custody"]["evm"]["usdc"] = format!("{:#x}", chain.token).into();
    cfg["custody"]["evm"]["providers"] = serde_json::Value::Array(
        services
            .rpcs
            .iter()
            .enumerate()
            .map(|(i, url)| serde_json::json!({"name":format!("local-{i}"),"rpc_url":url}))
            .collect(),
    );
    cfg["custody"]["evm"]["confirmations"] = 0_i64.into();
    cfg["custody"]["evm"]["poll_interval"] = "80ms".into();
    cfg["custody"]["evm"]["cursor_path"] = dir.join("custody-cursor.json").to_string_lossy().into_owned().into();
    cfg["time"]["interval"] = "500ms".into();
    cfg["time"]["max_drift"] = "5s".into();
    cfg["time"]["servers"] = serde_json::Value::Array(services.time.clone());
    cfg["feeds"]["mock_mids"] = serde_json::Value::Array(Vec::new());
    cfg["feeds"]["interval"] = "80ms".into();
    cfg["genesis"]["markets"].as_array_mut().context("genesis markets")?.truncate(1);
    cfg["genesis"]["markets"][0]["price_rules"]["min_sources"] = 1_i64.into();
    cfg["genesis"]["markets"][0]["price_rules"]["max_age_ms"] = 3000_i64.into();
    cfg["genesis"]["markets"][0]["house_cap_floor"] = "1000000".into();
    cfg["genesis"]["markets"][0]["spread_tiers"] = serde_json::Value::Array(vec![serde_json::json!({"bps":0})]);
    cfg["genesis"]["markets"][0]["financing"]["markup_bps"] = 0_i64.into();
    cfg["genesis"]["benchmarks"][0]["annual_bps"] = 0_i64.into();
    cfg["genesis"]["params"]["profit_fee_bps"] = 0_i64.into();
    cfg["genesis"]["params"]["house_total_cap_floor"] = "1000000".into();
    cfg["genesis"]["params"]["stress_levels"].as_array_mut().context("stress levels")?.truncate(1);
    for tier in cfg["genesis"]["params"]["toxicity"]["tiers"].as_array_mut().context("toxicity tiers")? {
        tier["extra_spread_bps"] = 0_i64.into();
    }
    let checkpoints = serde_json::json!({"rpc_url":services.rpcs[0],"relayer":format!("{:#x}", chain.relayer),"retry_interval":"80ms","rpc_timeout":"3s"});
    cfg.as_object_mut().context("config table")?.insert("checkpoints".into(), checkpoints);
    // Every funded account declares against this synthetic source; house funding and screened
    // client actions clear it.
    cfg["access"] = serde_json::json!({
        "source": engine_core::eip712::address(crate::test_support::access::key(0x77).verifying_key()).to_string(),
        "endpoint": services.access,
        "timeout": "2s",
        "max_age": "30s",
        "max_response_bytes": 4096,
        "max_in_flight": 8,
    });
    std::fs::write(path, toml::to_string(&toml::Value::try_from(cfg)?)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::gate_admits;

    #[test]
    fn a_healthy_verdict_from_before_the_deposit_does_not_admit_withdrawals() {
        let scrape = |paused: u8, seq: &str| {
            format!("engine_withdrawals_paused {paused}\nengine_withdrawals_verdict_seq {seq}\n")
        };
        assert!(!gate_admits(&scrape(0, "3"), 7), "verdict for seq 3 predates a credit at seq 7");
        assert!(!gate_admits("engine_withdrawals_paused 0\n", 7), "no verdict sequence is not a verdict");
        assert!(!gate_admits(&scrape(1, "63"), 7), "a covering verdict that pauses admits nothing");
        assert!(gate_admits(&scrape(0, "7"), 7));
        assert!(gate_admits(&scrape(0, "63"), 7));
        assert!(gate_admits(&scrape(0, "63"), 0));
    }
}
