//! Local Anvil scenario over the real sequencer, vault RPC, checkpoint worker, and deposit adapter.

use crate::checkpoints::Health;
mod source;

use std::path::Path;
use std::process::{Child, Command as Process, Stdio};
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, U64, U256};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use engine_core::Fixed;
use engine_core::access::AccessReject;
use engine_core::command::Command;
use engine_core::types::AccountId;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::{sleep, timeout};

use crate::checkpoints::{CheckpointChain, CheckpointRpc, Poster, View};
use crate::config::{CheckpointSection, CustodyProvider, FinalityTag, RpcProviderSection};
use crate::custody::Custody;
use crate::log::{Checkpoint, CheckpointDomain, LogEntry, Manifest};
use crate::sequencer::{Request, Sequencer, replay_path};
use crate::tee::EnclaveKey;
use crate::test_support::{
    access, capture_alerts, dev_config, log_entries, scratch_dir, sequencer_config, trusted_now,
};

sol! {
    function mint(address to, uint256 amount) external;
    function approve(address spender, uint256 amount) external returns (bool);
    function depositToHouse(uint256 amount) external returns (uint256);
    function depositFor(address account, uint256 amount) external returns (uint256);
    function transfer(address to, uint256 amount) external returns (bool);
    function bookSurplus() external returns (uint256);
}

pub(super) struct Anvil(pub Child);
impl Drop for Anvil {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[ignore = "requires Anvil and compiled contracts; scripts/dev-checkpoints.sh runs it"]
async fn dev_scenario_posts_replays_refuses_truncation_and_pauses_shortfall() {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let _anvil = Anvil(
        Process::new("anvil")
            .args(["--port", &port.to_string(), "--silent"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("Anvil installed"),
    );
    let url = format!("http://127.0.0.1:{port}");
    let provider = RpcClient::new_http(url.parse().unwrap());
    timeout(Duration::from_secs(10), async {
        while provider.request::<_, U64>("eth_chainId", ()).await.is_err() {
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let accounts: Vec<Address> = provider.request("eth_accounts", ()).await.unwrap();
    let relayer = accounts[0];
    // Checkpoint nonce ownership is independent of deployment, funding, and deposit transactions.
    let checkpoint_relayer = accounts[9];
    let key = Arc::new(EnclaveKey::generate());
    let token = deploy(&provider, relayer, "MockUSDC.sol/MockUSDC.json", &[]).await;
    let governance = deploy(
        &provider,
        relayer,
        "TimelockController.sol/TimelockController.json",
        &(U256::from(1), vec![relayer], vec![Address::ZERO], Address::ZERO).abi_encode_params(),
    )
    .await;
    // The MONKEY period budget, 1 MONKEY, is a development value, not an approved production parameter.
    let args = (token, Address::from(key.address()), governance, U256::from(10).pow(U256::from(18))).abi_encode();
    let vault = deploy(&provider, relayer, "Vault.sol/Vault.json", &args).await;
    let domain = CheckpointDomain { chain_id: 31337, vault: AccountId(vault.into()) };
    let section: CheckpointSection = toml::from_str(&format!(
        "rpc_url = {url:?}\nrelayer = '{checkpoint_relayer:#x}'\nretry_interval = '40ms'\nrpc_timeout = '2s'"
    ))
    .unwrap();
    let dir = scratch_dir("checkpoint-anvil");
    let make_poster = || Poster {
        registry: None,
        chain: CheckpointRpc::new(&section, domain, token, FinalityTag::Latest, 0, &dir, AccountId(key.address()))
            .unwrap(),
        domain,
        signer: AccountId(key.address()),
        retry: section.retry_interval,
        pending_deposits_critical_after: Duration::from_secs(10),
    };
    make_poster().startup(None).await.unwrap();

    let mut cfg = dev_config();
    cfg.custody.provider = CustodyProvider::Evm;
    cfg.custody.vault = format!("{vault:#x}");
    let anvil_cfg: crate::config::NodeConfig =
        toml::from_str(&std::fs::read_to_string(crate::test_support::ANVIL_CONFIG).unwrap()).unwrap();
    cfg.custody.evm = anvil_cfg.custody.evm;
    let evm = cfg.custody.evm.as_mut().unwrap();
    // Three local proxies exercise the production quorum adapter. They deliberately share
    // this isolated Anvil; independent service operation is a production deployment concern.
    let mut proxies = Vec::new();
    evm.providers.clear();
    for index in 0..3 {
        let proxy_port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        proxies.push(Anvil(
            Process::new("python3")
                .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/dev-rpc-proxy.py"))
                .args(["--port", &proxy_port.to_string(), "--target", &url])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("Python3 installed"),
        ));
        timeout(Duration::from_secs(10), async {
            while tokio::net::TcpStream::connect(("127.0.0.1", proxy_port)).await.is_err() {
                sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        evm.providers.push(
            toml::from_str::<RpcProviderSection>(&format!(
                "name = 'scenario-{index}'\nrpc_url = 'http://127.0.0.1:{proxy_port}'"
            ))
            .unwrap(),
        );
    }
    evm.usdc = token;
    evm.finality_tag = FinalityTag::Latest;
    evm.confirmations = 0;
    evm.poll_interval = Duration::from_millis(30);
    evm.cursor_path = dir.join("custody.json");
    cfg.genesis.house_capital = Fixed::ZERO;
    let (access_client, source_task) = source::start(domain).await;
    let (work_tx, work_rx) = mpsc::channel(64);
    let (stream, _) = broadcast::channel(1024);
    let (withdrawals, _signed) = mpsc::unbounded_channel();
    let seq_cfg = crate::sequencer::SequencerConfig {
        access_policy: Some(access::policy()),
        checkpoint_parent: Default::default(),
        checkpoints: Some(work_tx),
        vault: domain.vault,
        operator: cfg.operator().unwrap(),
        ..sequencer_config(&dir, watch::channel(Manifest::default()).0)
    };
    let mut seq = Sequencer::recover(seq_cfg, key.clone(), stream, withdrawals).unwrap();
    let custody = Custody::connect(&cfg.custody, seq.last_seq()).unwrap().with_access(Some(access_client));
    seq.activate_protocol().unwrap();
    seq.genesis(&cfg.genesis).unwrap();
    seq.trust(trusted_now()).unwrap();
    let (tx, rx) = mpsc::channel(64);
    let sequencer = tokio::spawn(seq.run(rx));
    let poster = tokio::spawn(make_poster().run(work_rx, tx.clone()));
    let custody_task = tokio::spawn(custody.run(_signed, tx.clone()));
    send(&provider, relayer, token, mintCall { to: relayer, amount: U256::from(110_000_000) }.abi_encode()).await;
    send(&provider, relayer, token, approveCall { spender: vault, amount: U256::MAX }.abi_encode()).await;
    send(&provider, relayer, vault, depositToHouseCall { amount: U256::from(100_000_000) }.abi_encode()).await;
    timeout(Duration::from_secs(10), async {
        loop {
            submit(&tx, Command::Tick).await;
            if source::house(&tx).await == Fixed::from_int(100) {
                break;
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let (_alert_guard, mut alerts) = capture_alerts();
    send(
        &provider,
        relayer,
        vault,
        depositForCall { account: Address::from(access::account(1).0), amount: U256::from(10_000_000) }.abi_encode(),
    )
    .await;
    source::await_pending(&tx, 1).await;
    // The undeclared account's pending deposit and the credit must reconcile to both real vault deposits.
    for _ in 0..4 {
        submit(&tx, Command::Tick).await;
    }
    timeout(Duration::from_secs(10), async {
        loop {
            let entries = log_entries(&dir);
            let latest = entries
                .iter()
                .rev()
                .find_map(|entry| match entry {
                    LogEntry::Checkpoint(cp) => Some(cp.seq),
                    _ => None,
                })
                .unwrap();
            if make_poster().chain.state(View::Latest).await.unwrap().checkpoint.is_some_and(|(seq, _)| seq >= latest) {
                break;
            }
            sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap();
    let replayed = replay_path(&dir).expect("every checkpoint signature verifies");
    source::assert_pending_reconciliation(&replayed);
    let work = crate::checkpoints::Work::capture(
        replayed.last_signed_checkpoint.unwrap(),
        replayed.accounting,
        &replayed.engine,
    );
    assert_eq!(make_poster().reconcile(&work).await.unwrap(), (Some(work.checkpoint.seq), Health::Healthy));

    // USDC sent to the vault outside a deposit raises a Warning but keeps withdrawals running. A
    // surplus booking names no funding address, so screening holds it as unattributable rather
    // than crediting it under whoever books it: house cash stays put and the hold is public.
    let sequenced = || Command::Withdraw { account: AccountId(relayer.into()), amount: Fixed::from_int(1), auth: None };
    send(&provider, relayer, token, mintCall { to: vault, amount: U256::from(10_000_000) }.abi_encode()).await;
    timeout(Duration::from_secs(10), async {
        while !std::iter::from_fn(|| alerts.try_recv().ok())
            .any(|alert| alert["alert"] == "collateral_surplus" && alert["level"] == "WARN")
        {
            sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap();
    // A sequenced request, though the engine rejects it for want of cash, means custody is healthy.
    assert!(submit(&tx, sequenced()).await.is_some(), "an unbooked surplus keeps withdrawals running");
    send(&provider, relayer, vault, bookSurplusCall {}.abi_encode()).await;
    timeout(Duration::from_secs(10), async {
        while source::house(&tx).await != Fixed::from_int(100)
            || source::held(&tx).await.len() != 1
            || submit(&tx, sequenced()).await.is_none()
        {
            submit(&tx, Command::Tick).await;
            sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(source::held(&tx).await[0].1, AccessReject::PayerMissing, "a surplus is held, never credited");
    while alerts.try_recv().is_ok() {}

    // Anvil impersonation is confined to this local test; no production token has this cheat.
    let client = reqwest::Client::new();
    for (method, params) in [
        ("anvil_impersonateAccount", serde_json::json!([vault])),
        ("anvil_setBalance", serde_json::json!([vault, "0x1000000000000000000"])),
    ] {
        client
            .post(&url)
            .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
    }
    send(&provider, vault, token, transferCall { to: relayer, amount: U256::from(1_000_000) }.abi_encode()).await;
    submit(&tx, Command::Tick).await;
    timeout(Duration::from_secs(10), async {
        loop {
            if std::iter::from_fn(|| alerts.try_recv().ok()).any(|alert| alert["alert"] == "collateral_mismatch") {
                break;
            }
            sleep(Duration::from_millis(40)).await;
        }
    })
    .await
    .unwrap();
    let (reply, request) = oneshot::channel();
    tx.send(Request::Query(Box::new(move |engine| {
        let _ = reply.send(access::withdrawal(engine, 1, Fixed::ONE, true));
    })))
    .await
    .unwrap();
    assert!(
        matches!(
            submit(&tx, request.await.unwrap()).await.unwrap().as_slice(),
            [engine_core::event::Event::WithdrawalRejected {
                reason: engine_core::event::Reject::PayoutUnavailable,
                ..
            }]
        ),
        "signed screened withdrawals pause without an approval"
    );
    assert!(submit(&tx, Command::Tick).await.is_some(), "trading commands continue");
    custody_task.abort();
    poster.abort();
    let _ = custody_task.await;
    let _ = poster.await;
    drop(tx);
    sequencer.await.unwrap().unwrap();
    source_task.abort();
    let _ = source_task.await;

    let entries = log_entries(&dir);
    let state = make_poster().chain.state(View::Latest).await.unwrap();
    let (posted_seq, _) = state.checkpoint.unwrap();
    let anchor = entries
        .iter()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(cp) if cp.seq == posted_seq => Some(cp),
            _ => None,
        })
        .unwrap();
    make_poster().startup(Some(anchor)).await.unwrap();
    let wrong_signer = Poster { signer: AccountId(EnclaveKey::generate().address()), ..make_poster() };
    assert!(wrong_signer.startup(Some(anchor)).await.is_err(), "unrecovered keys fail closed");
    let truncated = scratch_dir("checkpoint-truncated");
    std::fs::create_dir_all(&truncated).unwrap();
    let short: Vec<_> = entries
        .iter()
        .filter(|entry| match entry {
            LogEntry::Command(command) => command.sequenced.seq < posted_seq,
            LogEntry::Checkpoint(cp) => cp.seq < posted_seq,
            LogEntry::Withdrawal(_) => true,
        })
        .cloned()
        .collect();
    write_entries(&truncated.join("00000000000000000000.jsonl"), &short);
    let mut found: Option<Checkpoint> = None;
    Sequencer::read_log(&truncated, |entry| {
        if let LogEntry::Checkpoint(cp) = entry
            && cp.seq == posted_seq
        {
            found = Some(cp.clone());
        }
    })
    .unwrap();
    assert!(make_poster().startup(found.as_ref()).await.is_err(), "truncation refuses startup");
    // A canonical latest inclusion is revocable. Confirmation policy alone gates release.
    let delayed = Poster {
        chain: CheckpointRpc::new(&section, domain, token, FinalityTag::Latest, 2, &dir, AccountId(key.address()))
            .unwrap(),
        ..make_poster()
    };
    // Refill the shortfall, so only the confirmation policy gates this release.
    send(&provider, relayer, token, mintCall { to: vault, amount: U256::from(1_000_000) }.abi_encode()).await;
    local_rpc(&url, "anvil_mine", serde_json::json!(["0x3"])).await;
    let saved = local_rpc(&url, "evm_snapshot", serde_json::json!([])).await;
    let current = delayed.chain.state(View::Latest).await.unwrap();
    let (parent_seq, parent_root) = current.checkpoint.clone().unwrap();
    let parent =
        crate::log::CheckpointParent { has_checkpoint: true, seq: parent_seq, root: parent_root.parse().unwrap() };
    let seq = parent_seq + 100;
    let checkpoint = Checkpoint {
        parent: Some(parent),
        seq,
        root: format!("0x{}", "99".repeat(32)),
        signature: key.sign_checkpoint_v2(domain.chain_id, &domain.vault.0, seq, &[0x99; 32], parent),
        signer: key.address_hex(),
        domain: Some(domain),
        deferred_withdrawals: true,
    };
    let work = crate::checkpoints::Work {
        checkpoint,
        liabilities: current.deposited,
        deposits: current.deposits,
        authorized: Fixed::ZERO,
        coverage: crate::checkpoints::DepositCoverage::Historical,
        distributed: engine_core::UsdcUnits(0),
    };
    include_checkpoint(&delayed, &work.checkpoint).await;
    assert_eq!(delayed.reconcile(&work).await.unwrap(), (None, Health::Unvalidated));
    let nonce = provider.request::<_, U64>("eth_getTransactionCount", (checkpoint_relayer, "latest")).await.unwrap();
    assert_eq!(delayed.reconcile(&work).await.unwrap(), (None, Health::Unvalidated));
    assert_eq!(
        provider.request::<_, U64>("eth_getTransactionCount", (checkpoint_relayer, "latest")).await.unwrap(),
        nonce,
        "pending finality must not resubmit"
    );
    assert_ne!(
        delayed.chain.state(View::Finalized).await.unwrap().checkpoint,
        Some((seq, work.checkpoint.root.clone()))
    );
    assert_eq!(local_rpc(&url, "evm_revert", serde_json::json!([saved])).await, serde_json::json!(true));
    assert_eq!(
        delayed.chain.state(View::Latest).await.unwrap().checkpoint,
        current.checkpoint,
        "reorg removes provisional inclusion"
    );
    include_checkpoint(&delayed, &work.checkpoint).await;
    assert_eq!(
        delayed.reconcile(&work).await.unwrap(),
        (None, Health::Unvalidated),
        "re-inclusion still cannot release before finality"
    );
    local_rpc(&url, "anvil_mine", serde_json::json!(["0x2"])).await;
    assert_eq!(
        delayed.reconcile(&work).await.unwrap(),
        (Some(seq), Health::Healthy),
        "release follows confirmation depth"
    );

    let mut forged = entries;
    let cp = forged
        .iter_mut()
        .find_map(|entry| match entry {
            LogEntry::Checkpoint(cp) => Some(cp),
            _ => None,
        })
        .unwrap();
    super::tests::tamper_signature(&mut cp.signature);
    let tampered = dir.join("tampered.jsonl");
    write_entries(&tampered, &forged);
    assert!(replay_path(&tampered).is_err(), "edited signature fails replay");
}

pub(super) async fn deploy(provider: &RpcClient, from: Address, artifact: &str, args: &[u8]) -> Address {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contracts/out");
    let compiled: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join(artifact)).expect("forge build first")).unwrap();
    let mut input = hex::decode(compiled["bytecode"]["object"].as_str().unwrap().trim_start_matches("0x")).unwrap();
    input.extend_from_slice(args);
    let receipt = transact(provider, TransactionRequest::default().from(from).input(input.into())).await;
    assert!(receipt.status(), "{artifact} deployment reverted");
    receipt.contract_address.unwrap()
}

pub(super) async fn send(provider: &RpcClient, from: Address, to: Address, input: Vec<u8>) {
    let receipt = transact(provider, TransactionRequest::default().from(from).to(to).input(input.into())).await;
    assert!(receipt.status());
}

/// Sends through Anvil's unlocked account and polls its receipt under the scenario deadline.
pub(super) async fn transact(provider: &RpcClient, transaction: TransactionRequest) -> TransactionReceipt {
    let hash: B256 = provider.request("eth_sendTransaction", (transaction,)).await.unwrap();
    timeout(Duration::from_secs(5), async {
        loop {
            if let Some(receipt) =
                provider.request::<_, Option<TransactionReceipt>>("eth_getTransactionReceipt", (hash,)).await.unwrap()
            {
                return receipt;
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap()
}

async fn submit(tx: &mpsc::Sender<Request>, cmd: Command) -> Option<Vec<engine_core::event::Event>> {
    let (reply, rx) = oneshot::channel();
    tx.send(Request::Submit { cmd, reply: Some(reply) }).await.unwrap();
    timeout(Duration::from_secs(3), rx).await.unwrap().ok()
}

fn write_entries(path: &Path, entries: &[LogEntry]) {
    let text: String = entries.iter().map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap())).collect();
    std::fs::write(path, text).unwrap();
}

/// Executes only local Anvil scenario controls and rejects JSON-RPC errors.
async fn local_rpc(url: &str, method: &str, params: serde_json::Value) -> serde_json::Value {
    let response: serde_json::Value = reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    response.get("result").unwrap().clone()
}

/// Awaits canonical inclusion explicitly; transaction RPC acknowledgment is not a mined receipt.
async fn include_checkpoint(poster: &Poster<CheckpointRpc>, checkpoint: &Checkpoint) {
    poster.chain.post(checkpoint).await.unwrap();
    timeout(Duration::from_secs(10), async {
        loop {
            if poster.chain.state(View::Latest).await.unwrap().checkpoint
                == Some((checkpoint.seq, checkpoint.root.clone()))
            {
                break;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("checkpoint is included on local Anvil");
}
