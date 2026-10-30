//! The enclave's keys across a first boot, a restart, a missing sealed file, a config change, an
//! upgrade, a superseded release, the same release named again, and a second copy, on a local
//! deployment: Anvil, the Vault behind
//! its timelock, and node processes with the mock TEE and its stand-in for sealing. Each config uses
//! paths relative to the node's working directory and ephemeral ports, which the scenario reads
//! from the node's log, so a copy of the data directory runs the same release beside the original.

use std::path::{Path, PathBuf};
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::sol_types::SolCall;
use anyhow::{Context, bail, ensure};
use engine_core::types::AccountId;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use tokio::time::{sleep, timeout};

use crate::log::LogEntry;
use crate::sequencer::withdrawal_scenario::client::{Client, User};
use crate::sequencer::withdrawal_scenario::flows::{entries, last_seq};
use crate::sequencer::withdrawal_scenario::node::{ChildProcess, gate_admits, write_config};
use crate::sequencer::withdrawal_scenario::rpc::{
    Chain, setSignerCall, signerEpochCall, totalWithdrawnCall, usedNoncesCall,
};
use crate::sequencer::withdrawal_scenario::services::Services;
use crate::sequencer::withdrawal_scenario::{
    CHILD_ALERTS, CHILD_CONFIG, CHILD_FEED, CHILD_KEYS, CHILD_LAUNCH_DIGEST, CHILD_TEST,
};
use crate::tee::sealing::{Release, file_for};
use crate::test_support::LOADED_WAIT;

/// The stand-in launch measurements of the running release and its upgrade.
const RELEASE: [u8; 48] = [0xa1; 48];
const UPGRADE: [u8; 48] = [0xb2; 48];
/// What the node logs once each listener is up, with the address it bound.
const API_UP: &str = "api up over TLS";
const ATTESTATION_UP: &str = "serving the attestation while the vault names another signer";
const METRICS_UP: &str = "metrics up";
const HANDOVER_UP: &str = "handover listener up";

/// One release's node process, running `config` in `data` under a stand-in measurement.
struct ReleaseNode {
    /// Where each start's log goes.
    dir: PathBuf,
    /// The working directory, holding the log, history, cursor, alert outbox, and sealed keys.
    data: PathBuf,
    config: PathBuf,
    release: Release,
    feed: String,
    vault: Address,
    client: Option<Client>,
    child: Option<ChildProcess>,
    starts: usize,
}

/// How a release's node obtains its keys.
enum Keys {
    Sealed,
    New,
    From(u16),
}

/// Runs the scenario and prints its PASS line.
/// Error cases:
/// - any step or check fails
pub(super) async fn key_lifecycle() -> anyhow::Result<()> {
    let root = std::env::var_os("SCENARIO_LOG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join(format!("engine-keys-{}", std::process::id())))
        .join("key-lifecycle");
    ensure!(!root.exists(), "the scenario needs a fresh artifact directory: {}", root.display());
    std::fs::create_dir_all(&root)?;
    let mut chain = Chain::start(&root).await?;
    let domain = engine_core::eip712::domain(31337, AccountId(chain.vault.into()));
    let services = Services::start(&chain.url, chain.mining.clone(), domain).await?;
    let data = root.join("data");
    let config = release_config(&root.join("release-a.toml"), "alerts-a", &chain, &services)?;
    let mut old = ReleaseNode::new(&root, "release-a", &data, &config, RELEASE, &chain, &services)?;

    // Until the timelock makes this release the vault's current one, it seals no keys.
    old.spawn(Keys::New)?;
    ensure!(!old.exited().await?.success(), "a release the vault does not name sealed new keys");
    ensure!(old.logged("refusing this release's keys")?, "the refusal did not say why");
    ensure!(!data.join("keys").exists(), "a refused start sealed keys");
    let first = chain.set_current_release(&RELEASE, old.release.config_sha256).await?;

    // A first boot seals new keys the vault does not name: the node serves only its attestation
    // until the timelock registers them.
    old.spawn(Keys::New)?;
    let signer = old.attested_signer().await?;
    ensure!(signer != chain.signer, "new keys reused the published development key");
    let attestation_only = Client::new(old.logged_port(&[ATTESTATION_UP])?.context("attestation port")?, chain.vault)?;
    ensure!(attestation_only.get("/v1/markets").await.is_err(), "a node the vault does not name served its API");
    chain.govern(setSignerCall { signer: Address::from(signer.0) }.abi_encode()).await?;
    chain.signer = signer;
    old.ready().await?;
    let epoch = chain.read(chain.vault, signerEpochCall {}).await?;

    // An outstanding withdrawal, signed and unclaimed.
    let trader = User::new(1)?;
    chain.deposit(trader.address(), 100, false).await?;
    old.api()?.declare(&trader).await?;
    old.api()?.wait_balance(&trader, "100").await?;
    old.api()?.wait_finalised(&trader).await?;
    old.wait_withdrawals_enabled(last_seq(&data.join("log"))?).await?;
    let mut stream = old.api()?.stream().await?;
    old.api()?.request_withdrawal(&old.api()?.withdrawal(&trader, "10").await?).await?;
    let outstanding = stream.payout(trader.account, 10).await?;
    drop(stream);

    // A restart opens the sealed keys: the same signer, no governance, and the outstanding
    // authorization still the only one, so nothing needed signing again.
    old.stop().await?;
    old.spawn(Keys::Sealed)?;
    old.ready().await?;
    ensure!(old.attested_signer().await? == signer, "a restart changed the attested signer");
    old.wait_withdrawals_enabled(0).await?;
    let signed = entries(&data.join("log"))?
        .into_iter()
        .filter(|entry| matches!(entry, LogEntry::Withdrawal(w) if w.nonce == outstanding.nonce && w.account == trader.account))
        .count();
    ensure!(signed == 1, "the restart signed the outstanding withdrawal again: {signed} authorizations");

    // Without its sealed file, and without --new-key, the release refuses to start.
    old.stop().await?;
    let sealed = file_for(&data.join("keys"), &old.release, first);
    let aside = root.join("sealed-aside.json");
    std::fs::rename(&sealed, &aside)?;
    old.spawn(Keys::Sealed)?;
    ensure!(!old.exited().await?.success(), "a release without sealed keys started");
    ensure!(old.logged("no keys are sealed for this release")?, "the refusal did not say why");
    std::fs::rename(&aside, &sealed)?;

    // The same code under a config governance has not named is another release: it opens no keys,
    // and seals none.
    let edited = root.join("release-a-edited.toml");
    std::fs::write(&edited, std::fs::read_to_string(&config)? + "\n# an edit governance has not approved\n")?;
    let mut changed = ReleaseNode::new(&root, "release-a-edited", &data, &edited, RELEASE, &chain, &services)?;
    changed.spawn(Keys::Sealed)?;
    ensure!(!changed.exited().await?.success(), "a config governance has not named opened the keys");
    ensure!(changed.logged("refusing this release's keys")?, "the refusal did not say why");
    changed.spawn(Keys::New)?;
    ensure!(!changed.exited().await?.success(), "a config governance has not named sealed new keys");
    ensure!(changed.logged("refusing this release's keys")?, "the refusal did not say why");
    old.spawn(Keys::Sealed)?;
    old.ready().await?;

    // An upgrade, with a config of its own, gets no keys until the timelock makes it the current
    // release, and the running release goes on.
    let upgrade_config = release_config(&root.join("release-b.toml"), "alerts-b", &chain, &services)?;
    let mut new = ReleaseNode::new(&root, "release-b", &data, &upgrade_config, UPGRADE, &chain, &services)?;
    new.spawn(Keys::From(old.handover_port()?))?;
    ensure!(!new.exited().await?.success(), "a release the vault does not name took the keys");
    ensure!(
        new.logged("the predecessor keeps its keys: the vault's current release is")?,
        "the refusal did not say why"
    );
    old.ready().await?;

    // Once the timelock makes it the current release, the upgrade takes the keys and the old
    // release exits.
    chain.set_current_release(&UPGRADE, new.release.config_sha256).await?;
    new.spawn(Keys::From(old.handover_port()?))?;
    ensure!(old.exited().await?.success(), "the old release did not exit cleanly once it handed over");
    new.ready().await?;
    ensure!(new.attested_signer().await? == signer, "the upgrade attests another signer");
    ensure!(chain.read(chain.vault, signerEpochCall {}).await? == epoch, "the upgrade changed the signer epoch");
    chain.send(chain.payer, chain.vault, chain.claim(&outstanding)?.abi_encode()).await?;
    let nonce = usedNoncesCall { account: trader.address(), nonce: outstanding.nonce };
    ensure!(chain.read(chain.vault, nonce).await?, "the authorization the old release signed did not pay");

    // The old release's sealed file is still on the disk, but the upgrade superseded it.
    ensure!(sealed.exists(), "the old release's sealed file is gone");
    old.spawn(Keys::Sealed)?;
    ensure!(!old.exited().await?.success(), "the superseded release opened its keys");
    ensure!(old.logged("refusing this release's keys")?, "the refusal did not say why");

    // Governance names the old release again, in a new generation: its sealed copy still does not
    // open, and it takes the keys afresh from the running upgrade, which exits.
    let again = chain.set_current_release(&RELEASE, old.release.config_sha256).await?;
    old.spawn(Keys::Sealed)?;
    ensure!(!old.exited().await?.success(), "a release named again reopened its old sealed copy");
    ensure!(old.logged(&format!("in the vault's generation {again} of it"))?, "the refusal did not say why");
    old.spawn(Keys::From(new.handover_port()?))?;
    ensure!(new.exited().await?.success(), "the upgrade did not exit cleanly once it handed the keys back");
    old.ready().await?;
    ensure!(old.attested_signer().await? == signer, "the release named again attests another signer");
    let resealed = file_for(&data.join("keys"), &old.release, again);
    ensure!(sealed.exists() && resealed.exists(), "the keys were not sealed afresh beside the old copy");

    // A second copy of the release unseals the same keys, and each sequences a history of its own.
    // Both take the same signed withdrawal, unless the conflict has already stopped one; the vault
    // takes one copy's checkpoint, the other stops on the conflict, and the withdrawal pays once.
    old.wait_withdrawals_enabled(0).await?;
    let request = old.api()?.withdrawal(&trader, "20").await?;
    old.stop().await?;
    let copy_data = root.join("data-copy");
    copy_dir(&data, &copy_data)?;
    let mut copy = ReleaseNode::new(&root, "release-a-copy", &copy_data, &config, RELEASE, &chain, &services)?;
    old.spawn(Keys::Sealed)?;
    copy.spawn(Keys::Sealed)?;
    old.ready().await?;
    copy.ready().await?;
    ensure!(copy.attested_signer().await? == signer, "the copy attests another signer");
    let _ = tokio::join!(old.api()?.request_withdrawal(&request), copy.api()?.request_withdrawal(&request));
    let stopped = timeout(LOADED_WAIT, async {
        loop {
            match (old.running(), copy.running()) {
                (true, true) => sleep(Duration::from_millis(100)).await,
                (false, true) => break anyhow::Ok(0),
                (true, false) => break anyhow::Ok(1),
                (false, false) => bail!("both copies stopped"),
            }
        }
    })
    .await
    .context("two copies of one release both kept signing")??;
    let (stopped, survivor) = if stopped == 0 { (&old, 1) } else { (&copy, 0) };
    ensure!(stopped.logged("checkpoint_conflict")?, "the stopped copy did not stop on the checkpoint conflict");
    // The survivor's stream replays its history from the start.
    let survivor = [&old, &copy][survivor];
    let paid = survivor.api()?.stream().await?.payout(trader.account, 20).await?;
    // The stopped copy released no authorization the surviving history does not hold.
    let survivor_entries = entries(&survivor.data.join("log"))?;
    for entry in entries(&stopped.data.join("log"))? {
        if let LogEntry::Withdrawal(signed) = &entry {
            ensure!(survivor_entries.contains(&entry), "the stopped copy signed a withdrawal of its own: {signed:?}");
        }
    }
    let claim = chain.claim(&paid)?.abi_encode();
    chain.send(chain.payer, chain.vault, claim.clone()).await?;
    chain.duplicate_reverts(claim).await?;
    let total = chain.read(chain.vault, totalWithdrawnCall {}).await?;
    ensure!(total == U256::from(30_000_000), "the vault paid {total} base units, not 30 USDC once each");
    println!(
        "PASS: keys are sealed only for the vault's current release and survive a restart; a missing file \
         refuses without --new-key; a config governance has not named opens nothing; an upgrade it names takes \
         the keys, the outstanding withdrawal pays, and the superseded release no longer opens its own, nor does \
         it once named again, when it takes the keys afresh; a second copy stops on the checkpoint conflict"
    );
    Ok(())
}

/// Writes a release's config to `path`, relative to the node's working directory, with ephemeral
/// ports and its alert outbox in `outbox`, and returns `path`.
/// Error cases:
/// - the config cannot be written
fn release_config(path: &Path, outbox: &str, chain: &Chain, services: &Services) -> anyhow::Result<PathBuf> {
    write_config(path, Path::new(""), 0, 0, chain, services, |cfg| {
        cfg["alerts"]["outbox"]["dir"] = outbox.into();
        cfg["tee"]["use_published_mock_key"] = false.into();
        cfg["tee"]["sealing"] = json!({ "dir": "keys", "handover": { "listen": "127.0.0.1:0" } });
        Ok(())
    })?;
    Ok(path.to_owned())
}

impl ReleaseNode {
    /// A release running `config` in `data` under the stand-in measurement `digest`, logging under
    /// `root/name`.
    /// Error cases:
    /// - a directory cannot be created, or the config cannot be read
    fn new(
        root: &Path,
        name: &str,
        data: &Path,
        config: &Path,
        digest: [u8; 48],
        chain: &Chain,
        services: &Services,
    ) -> anyhow::Result<Self> {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(data)?;
        let config_sha256 = Sha256::digest(std::fs::read(config)?).into();
        Ok(Self {
            dir,
            data: data.to_owned(),
            config: config.to_owned(),
            release: Release { launch_digest: digest, config_sha256 },
            feed: services.feed.clone(),
            vault: chain.vault,
            client: None,
            child: None,
            starts: 0,
        })
    }

    /// Starts the node with `keys`, logging to a file of its own for this start.
    /// Error cases:
    /// - the previous process is still running, or the child cannot spawn
    fn spawn(&mut self, keys: Keys) -> anyhow::Result<()> {
        ensure!(self.child.is_none(), "stop the previous process first");
        self.starts += 1;
        self.client = None;
        let start = match keys {
            Keys::Sealed => None,
            Keys::New => Some("new-key".to_owned()),
            Keys::From(port) => Some(format!("predecessor=127.0.0.1:{port}")),
        };
        let config = self.config.to_str().context("config path is UTF-8")?;
        let digest = hex::encode(self.release.launch_digest);
        // Critical alerts read the durable outbox this config sets up, not a webhook sink.
        let mut env = vec![
            (CHILD_CONFIG, config),
            (CHILD_FEED, self.feed.as_str()),
            (CHILD_LAUNCH_DIGEST, &digest),
            (CHILD_ALERTS, "http://127.0.0.1:9/alerts"),
        ];
        if let Some(start) = &start {
            env.push((CHILD_KEYS, start));
        }
        self.child = Some(ChildProcess::spawn(
            std::env::current_exe()?,
            &["--ignored", "--exact", CHILD_TEST, "--nocapture", "--test-threads=1"],
            &self.log(),
            &env,
            Some(&self.data),
        )?);
        Ok(())
    }

    /// This start's log file.
    fn log(&self) -> PathBuf {
        self.dir.join(format!("node-{}.log", self.starts))
    }

    /// Whether this start's log holds `text`.
    /// Error cases:
    /// - the log cannot be read
    fn logged(&self, text: &str) -> anyhow::Result<bool> {
        Ok(std::fs::read_to_string(self.log())?.contains(text))
    }

    /// The loopback port in this start's last log line with one of `messages`, if any.
    /// Error cases:
    /// - the log cannot be read
    fn logged_port(&self, messages: &[&str]) -> anyhow::Result<Option<u16>> {
        let log = std::fs::read_to_string(self.log())?;
        Ok(log.lines().rev().filter(|line| messages.iter().any(|message| line.contains(message))).find_map(|line| {
            let port = line.split_once("listen=127.0.0.1:")?.1;
            port[..port.find(|c: char| !c.is_ascii_digit()).unwrap_or(port.len())].parse().ok()
        }))
    }

    /// The handover listener's port.
    /// Error cases:
    /// - the log cannot be read, or shows no handover listener
    fn handover_port(&self) -> anyhow::Result<u16> {
        self.logged_port(&[HANDOVER_UP])?.context("the node logged no handover listener")
    }

    /// The full API, once `ready` has found it.
    /// Error cases:
    /// - `ready` has not found it since this start
    fn api(&self) -> anyhow::Result<&Client> {
        self.client.as_ref().context("the node's API is not up")
    }

    /// Whether the process is still running.
    fn running(&mut self) -> bool {
        self.child.as_mut().is_some_and(|child| child.check().is_ok())
    }

    /// The address the node's attestation names, once it serves one.
    /// Error cases:
    /// - the process exits, or serves no attestation in time
    async fn attested_signer(&mut self) -> anyhow::Result<AccountId> {
        timeout(LOADED_WAIT, async {
            loop {
                self.child.as_mut().context("no running process")?.check()?;
                if let Some(port) = self.logged_port(&[API_UP, ATTESTATION_UP])?
                    && let Ok(attestation) = Client::new(port, self.vault)?.get("/v1/attestation").await
                {
                    let address = attestation["enclave_address"].as_str().context("attested address")?;
                    return Ok(address.parse::<AccountId>()?);
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("the node never served its attestation")?
    }

    /// Waits until the full API serves a mark.
    /// Error cases:
    /// - the process exits, or the API does not serve in time
    async fn ready(&mut self) -> anyhow::Result<()> {
        timeout(LOADED_WAIT, async {
            loop {
                self.child.as_mut().context("no running process")?.check()?;
                if let Some(port) = self.logged_port(&[API_UP])? {
                    let client = Client::new(port, self.vault)?;
                    if client.mark().await.is_ok() {
                        self.client = Some(client);
                        return anyhow::Ok(());
                    }
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("the node's API never came up")?
    }

    /// Waits until custody admits withdrawals on a verdict covering seq `after`.
    /// Error cases:
    /// - the metrics never show it
    async fn wait_withdrawals_enabled(&mut self, after: u64) -> anyhow::Result<()> {
        timeout(LOADED_WAIT, async {
            loop {
                self.child.as_mut().context("no running process")?.check()?;
                if let Some(port) = self.logged_port(&[METRICS_UP])? {
                    let metrics = reqwest::get(format!("http://127.0.0.1:{port}/metrics")).await?.text().await?;
                    if gate_admits(&metrics, after) {
                        return anyhow::Ok(());
                    }
                }
                sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("withdrawals stayed paused")?
    }

    /// Drains the process with SIGTERM.
    /// Error cases:
    /// - it is not running or does not drain successfully
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.child.take().context("no running process")?.stop().await
    }

    /// The process's exit status, once it exits on its own.
    /// Error cases:
    /// - it is not running, or still runs after `LOADED_WAIT`
    async fn exited(&mut self) -> anyhow::Result<std::process::ExitStatus> {
        self.child.take().context("no running process")?.exited(LOADED_WAIT).await
    }
}

/// Copies the directory tree `from` to `to`.
/// Error cases:
/// - a read or write fails
fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
