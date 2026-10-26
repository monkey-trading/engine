//! The custody tests' harness: a fake chain, a sequencer over a scratch log, captured log lines,
//! and deposit fixtures.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use anyhow::bail;
use engine_core::auth::{ClientAuth, Operator};
use engine_core::command::{Command, Sequenced};
use engine_core::types::AccountId;
use engine_core::{Engine, Fixed};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tracing::subscriber::DefaultGuard;

use crate::clock::now_ms;
use crate::config::EvmCustodySection;
use crate::custody::chain::BlockLink;
use crate::custody::cursor::Cursor;
use crate::custody::evm::{Beneficiary, BlockRef, DepositWatcher, Finality, VaultDeposit, VaultIdentity, VaultReader};
use crate::log::{LogEntry, Manifest};
use crate::sequencer::{Request, Sequencer, SequencerConfig, SignedWithdrawal};
use crate::tee::EnclaveKey;
use crate::test_support::{log_entries, sequencer_config, trusted_now};

pub(super) const CHAIN_ID: u64 = 42161;
pub(super) const VAULT: Address = Address::repeat_byte(0x7a);
pub(super) const USDC_TOKEN: Address = Address::repeat_byte(0xc0);
pub(super) const TRADER: AccountId = AccountId([0xaa; 20]);
/// One USDC in base units.
pub(super) const USDC: u64 = 1_000_000;

/// A chain whose final block, deposits, and failures a test sets.
#[derive(Clone, Default)]
pub(super) struct FakeChain(pub(super) Arc<Mutex<ChainState>>);

/// Successful header payload faults, distinct from a failed transport.
#[derive(Clone, Copy)]
pub(super) enum BadHeaders {
    Missing,
    Empty,
    WrongHeight,
    Extra,
}

#[derive(Default)]
pub(super) struct ChainState {
    /// What the endpoint reports; the configured chain and token when `None`.
    pub(super) identity: Option<VaultIdentity>,
    pub(super) head: u64,
    /// The head block's timestamp; now when `None`.
    pub(super) head_time: Option<u64>,
    /// The block the finality tag names; with no confirmations, the final block.
    pub(super) final_block: Option<u64>,
    /// Independent release tag for confirmed-deposit tests.
    pub(super) withdrawal_block: Option<u64>,
    pub(super) withdrawal_failing: bool,
    /// The hash the finality tag reports, when it differs from the chain's.
    pub(super) tag_hash: Option<B256>,
    /// Header reads to fail before they answer again.
    pub(super) header_failures: u32,
    pub(super) bad_headers: Option<BadHeaders>,
    /// Refuse header batches of more blocks than this, like a provider's batch limit.
    pub(super) header_batch_cap: Option<u64>,
    /// Header reads to fail, by their order among all header reads.
    pub(super) fail_header_reads: Vec<u32>,
    /// Header reads to answer as a missing block, by their order among all header reads.
    pub(super) bad_header_reads: Vec<u32>,
    pub(super) header_calls: u32,
    /// Header ranges answered.
    pub(super) header_reads: Vec<(u64, u64)>,
    /// A block whose header reports a parent off this chain.
    pub(super) broken_link: Option<u64>,
    pub(super) deposits: Vec<VaultDeposit>,
    /// Answer `deposits` with nothing, like a node that lags the one counting.
    pub(super) hide_logs: bool,
    /// Block hashes that differ from `canonical_hash`, as after a reorg.
    pub(super) forks: BTreeMap<u64, B256>,
    /// How long a log read takes, in the runtime's time.
    pub(super) latency: Option<Duration>,
    /// How long a header read takes, in the runtime's time.
    pub(super) header_latency: Option<Duration>,
    /// Refuse log reads over more blocks than this, like a provider's result cap.
    pub(super) log_range_cap: Option<u64>,
    pub(super) failing: bool,
    /// Requests that start a poll: identity checks and final-block reads.
    pub(super) polls: u64,
    pub(super) reads: Vec<(u64, u64)>,
}

/// The sequencer over a log in a test directory.
pub(super) struct Node {
    pub(super) tx: mpsc::Sender<Request>,
    pub(super) operator: Operator,
    sequencer: JoinHandle<anyhow::Result<()>>,
    _withdrawals: mpsc::UnboundedReceiver<SignedWithdrawal>,
}

/// Log lines captured from this thread's tasks.
#[derive(Clone, Default)]
pub(super) struct Logs(Arc<Mutex<Vec<u8>>>);

impl VaultReader for FakeChain {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        let mut chain = self.0.lock().unwrap();
        chain.polls += 1;
        if chain.failing {
            bail!("connection refused");
        }
        Ok(chain.identity.unwrap_or(VaultIdentity { chain_id: CHAIN_ID, token: Some(USDC_TOKEN) }))
    }

    async fn finality(&self) -> anyhow::Result<Finality> {
        let mut chain = self.0.lock().unwrap();
        chain.polls += 1;
        if chain.failing {
            bail!("connection refused");
        }
        // Rounded up, so a current head is never a second old by the watchdog's reading.
        let head_time = chain.head_time.unwrap_or_else(|| (now_ms() / 1_000 + 1) as u64);
        let tagged = chain
            .final_block
            .map(|number| BlockRef { number, hash: chain.tag_hash.unwrap_or_else(|| chain.hash_at(number)) });
        Ok(Finality { head: chain.head, head_time, tagged, head_support: None })
    }

    async fn withdrawal_finality(&self) -> anyhow::Result<Finality> {
        let mut finality = self.finality().await?;
        let chain = self.0.lock().unwrap();
        if chain.withdrawal_failing {
            bail!("withdrawal finality unavailable");
        }
        finality.tagged = chain.withdrawal_block.map(|number| BlockRef { number, hash: chain.hash_at(number) });
        Ok(finality)
    }

    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        let latency = self.0.lock().unwrap().header_latency;
        if let Some(latency) = latency {
            sleep(latency).await;
        }
        let mut chain = self.0.lock().unwrap();
        let call = chain.header_calls;
        chain.header_calls += 1;
        let over_cap = chain.header_batch_cap.is_some_and(|cap| to - from + 1 > cap);
        if chain.header_failures > 0 || chain.fail_header_reads.contains(&call) || over_cap {
            chain.header_failures = chain.header_failures.saturating_sub(1);
            bail!("rate limited");
        }
        if chain.bad_header_reads.contains(&call) {
            return crate::custody::rpc::decode_header(None, from).map(|header| vec![header]);
        }
        chain.header_reads.push((from, to));
        let parent = |number: u64| match number {
            _ if chain.broken_link == Some(number) => B256::repeat_byte(0x33),
            0 => B256::ZERO,
            _ => chain.hash_at(number - 1),
        };
        let mut headers: Vec<_> = (from..=to)
            .map(|number| BlockLink { number, hash: chain.hash_at(number), parent_hash: parent(number) })
            .collect();
        match chain.bad_headers {
            Some(BadHeaders::Missing) => return crate::custody::rpc::decode_header(None, from).map(|h| vec![h]),
            Some(BadHeaders::Empty) => headers.clear(),
            Some(BadHeaders::WrongHeight) => {
                if let Some(header) = headers.first_mut() {
                    header.number ^= 1;
                }
            }
            Some(BadHeaders::Extra) => {
                if let Some(header) = headers.first().copied() {
                    headers.push(header);
                }
            }
            None => {}
        }
        Ok(headers)
    }

    async fn deposits(&self, from: u64, to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        let latency = self.0.lock().unwrap().latency;
        if let Some(latency) = latency {
            sleep(latency).await;
        }
        let mut chain = self.0.lock().unwrap();
        if chain.log_range_cap.is_some_and(|cap| to - from + 1 > cap) {
            bail!("query exceeds max results");
        }
        chain.reads.push((from, to));
        let visible = chain.deposits.iter().filter(|d| !chain.hide_logs && (from..=to).contains(&d.block));
        Ok(visible.cloned().collect())
    }

    async fn deposit_count(&self, at: BlockRef) -> anyhow::Result<u64> {
        let chain = self.0.lock().unwrap();
        if chain.hash_at(at.number) != at.hash {
            bail!("block {} is not canonical", at.hash);
        }
        Ok(chain.deposits.iter().filter(|d| d.block <= at.number).count() as u64)
    }
}

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl ChainState {
    /// The canonical hash at `number`, forks included.
    fn hash_at(&self, number: u64) -> B256 {
        self.forks.get(&number).copied().unwrap_or_else(|| canonical_hash(number))
    }
}

impl FakeChain {
    pub(super) fn add(&self, deposit: VaultDeposit) {
        self.0.lock().unwrap().deposits.push(deposit);
    }

    pub(super) fn remove(&self, deposit: &VaultDeposit) {
        self.0.lock().unwrap().deposits.retain(|d| d != deposit);
    }

    pub(super) fn set_final(&self, block: Option<u64>) {
        self.0.lock().unwrap().final_block = block;
    }

    pub(super) fn reads(&self) -> Vec<(u64, u64)> {
        self.0.lock().unwrap().reads.clone()
    }
}

impl Node {
    /// Recovers the sequencer from the log in `dir/log/` and runs it.
    pub(super) fn start(dir: &Path) -> Self {
        Self::start_at_protocol(dir, false, CHAIN_ID, TRADER, ClientAuth::Unsigned)
    }

    /// Starts the latest protocol while retaining the real disk-backed sequencer.
    pub(super) fn start_confirmed(dir: &Path) -> Self {
        Self::start_at_protocol(dir, true, CHAIN_ID, TRADER, ClientAuth::Unsigned)
    }

    /// Uses the Anvil fixture's actual signing domain for approval records.
    pub(super) fn start_evm(dir: &Path, chain_id: u64, vault: AccountId) -> Self {
        Self::start_at_protocol(dir, true, chain_id, vault, ClientAuth::Eip712 { chain_id, vault })
    }

    fn start_at_protocol(dir: &Path, latest: bool, chain_id: u64, vault: AccountId, client_auth: ClientAuth) -> Self {
        let log_dir = dir.join("log");
        let base = sequencer_config(&log_dir, watch::channel(Manifest::default()).0);
        let cfg = SequencerConfig {
            segment_commands: 1_000,
            checkpoint_every: 1_000,
            checkpoint_parent: Default::default(),
            checkpoints: None,
            chain_id,
            vault,
            client_auth,
            operator: engine_core::auth::Operator { chain_id, vault, ..base.operator },
            ..base
        };
        let (stream, _) = broadcast::channel(1_024);
        let (withdrawals, received_withdrawals) = mpsc::unbounded_channel();
        let operator = cfg.operator;
        let mut seq = Sequencer::recover(cfg, Arc::new(EnclaveKey::test_key()), stream, withdrawals).unwrap();
        if latest {
            seq.activate_protocol().unwrap();
        }
        seq.trust(trusted_now()).unwrap();
        let (tx, rx) = mpsc::channel(64);
        Node { tx, operator, sequencer: tokio::spawn(seq.run(rx)), _withdrawals: received_withdrawals }
    }

    /// Drains the sequencer and waits for it to stop.
    pub(super) async fn stop(self) {
        drop(self.tx);
        self.sequencer.await.unwrap().unwrap();
    }

    pub(super) async fn read<T: Send + 'static>(&self, read: impl FnOnce(&Engine) -> T + Send + 'static) -> T {
        let (reply, rx) = oneshot::channel();
        let query = Box::new(move |engine: &Engine| {
            let _ = reply.send(read(engine));
        });
        self.tx.send(Request::Query(query)).await.unwrap();
        rx.await.unwrap()
    }

    pub(super) async fn balance(&self, account: AccountId) -> Option<Fixed> {
        self.read(move |e| e.accounts().get(&account).map(|a| a.balance)).await
    }

    /// Sequences `cmd` and returns its events.
    pub(super) async fn submit(&self, cmd: Command) -> Vec<engine_core::event::Event> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
        rx.await.unwrap()
    }

    pub(super) async fn last_seq(&self) -> Option<u64> {
        self.read(|e| e.last_seq()).await
    }
}

impl Logs {
    /// Captures every log line emitted on this thread until the guard drops.
    pub(super) fn capture() -> (Self, DefaultGuard) {
        crate::test_support::keep_callsites_interested();
        let logs = Logs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt().with_ansi(false).with_writer(move || writer.clone()).finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    pub(super) fn count(&self, needle: &str) -> usize {
        String::from_utf8_lossy(&self.0.lock().unwrap()).matches(needle).count()
    }

    /// The captured lines that contain `needle`.
    pub(super) fn lines_with(&self, needle: &str) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains(needle))
            .map(str::to_owned)
            .collect()
    }
}

/// A sequencer over an in-memory engine that answers sync requests only when `answer_syncs`,
/// as when its fsync fails otherwise.
pub(super) fn engine_sequencer(answer_syncs: bool) -> mpsc::Sender<Request> {
    refusing_sequencer(answer_syncs, 0)
}

/// `engine_sequencer` that also drops its first `refusals` commands unapplied, with their
/// replies, and keeps running, as the node's sequencer does while trusted time refuses stamps.
pub(super) fn refusing_sequencer(answer_syncs: bool, refusals: usize) -> mpsc::Sender<Request> {
    let (tx, mut rx) = mpsc::channel::<Request>(64);
    tokio::spawn(async move {
        let (mut engine, mut seq, mut refusals) = (Engine::new(), 0, refusals);
        while let Some(request) = rx.recv().await {
            match request {
                Request::Submit { .. } if refusals > 0 => refusals -= 1,
                Request::Submit { cmd, reply } => {
                    let events = engine.apply(&Sequenced { seq, ts: 0, cmd }).unwrap();
                    seq += 1;
                    if let Some(reply) = reply {
                        let _ = reply.send(events);
                    }
                }
                Request::Query(query) => query(&engine),
                Request::Sync(reply) if answer_syncs => {
                    let _ = reply.send(engine.last_seq());
                }
                Request::Sync(_) => {}
                Request::CheckpointStatus { .. } => unreachable!("no checkpoint worker in this test"),
                Request::WatchBenchmarkSchedule(_) => unreachable!("only the API watches benchmark schedules"),
                Request::TimeSample(_) => unreachable!("custody sends no time samples"),
            }
        }
    });
    tx
}

/// A watcher over `chain` with its cursor in `dir`, after a log ending at `log_seq`.
pub(super) fn watcher(
    chain: &FakeChain,
    dir: &Path,
    max_block_range: u64,
    log_seq: Option<u64>,
) -> DepositWatcher<FakeChain> {
    watcher_with_confirmations(chain, dir, max_block_range, log_seq, 0)
}

/// A watcher whose final block sits `confirmations` below the tagged block.
pub(super) fn watcher_with_confirmations(
    chain: &FakeChain,
    dir: &Path,
    max_block_range: u64,
    log_seq: Option<u64>,
    confirmations: u64,
) -> DepositWatcher<FakeChain> {
    let toml = evm_toml(dir, max_block_range).replace("confirmations = 0", &format!("confirmations = {confirmations}"));
    let cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
    DepositWatcher::start_legacy(chain.clone(), &cfg, CHAIN_ID, VAULT, log_seq).unwrap()
}

/// The cursor stored in `dir`, read without the checks `Cursor::load` makes.
pub(super) fn stored_cursor(dir: &Path) -> Cursor {
    serde_json::from_str(&std::fs::read_to_string(dir.join("cursor.json")).unwrap()).unwrap()
}

/// A `[custody.evm]` section with its cursor in `dir`.
pub(super) fn evm_toml(dir: &Path, max_block_range: u64) -> String {
    format!(
        r#"
        providers = [
          {{ name = "a", rpc_url = "http://127.0.0.1:1" }},
          {{ name = "b", rpc_url = "http://127.0.0.1:2" }},
          {{ name = "c", rpc_url = "http://127.0.0.1:3" }},
        ]
        usdc = "0xc0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0"
        finality_tag = "safe"
        withdrawal_finality_tag = "finalized"
        confirmations = 0
        start_block = 0
        max_block_range = {max_block_range}
        poll_interval = "10ms"
        rpc_timeout = "1s"
        stall_critical_after = "1s"
        finality_stalled_after = "1s"
        cursor_path = "{}"
        "#,
        dir.join("cursor.json").display()
    )
}

/// Vault deposit `id` in canonical `block`, from a transaction named after the id.
pub(super) fn deposit(id: u64, block: u64, beneficiary: Beneficiary, units: u64) -> VaultDeposit {
    let tx_hash = B256::left_padding_from(&id.to_be_bytes());
    let block_hash = canonical_hash(block);
    VaultDeposit { block, block_hash, tx_hash, log_index: 1, id, beneficiary, units: U256::from(units) }
}

/// The fake chain's hash for `block` before any reorg.
pub(super) fn canonical_hash(block: u64) -> B256 {
    B256::from(U256::from(block))
}

pub(super) fn usdc(amount: &str) -> Fixed {
    amount.parse().unwrap()
}

/// How many deposit and house-capital commands the log in `dir` holds.
pub(super) fn deposit_commands(dir: &Path) -> usize {
    let deposit = |cmd: &Command| matches!(cmd, Command::Deposit { .. } | Command::HouseCapital { .. });
    let entries = log_entries(&dir.join("log"));
    entries.iter().filter(|e| matches!(e, LogEntry::Command(c) if deposit(&c.sequenced.cmd))).count()
}

/// Waits, in paused time, until the watcher has started `polls` polls in all.
pub(super) async fn wait_for_polls(chain: &FakeChain, polls: u64) {
    while chain.0.lock().unwrap().polls < polls {
        sleep(Duration::from_millis(5)).await;
    }
}

/// A fresh, empty directory for one test.
pub(super) fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("engine-custody-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
