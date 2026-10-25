//! Anvil receipts through the RPC quorum, custody watcher and durable sequencer.

use std::path::Path;
use std::process::{Command as Process, Stdio};
use std::time::Duration;

use engine_core::auth::RequestAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::eip712::{self, Eip712Domain};
use engine_core::event::{Event, Reject};
use engine_core::price::SourceQuote;
use engine_core::types::AccountId;
use engine_core::types::Side;
use k256::ecdsa::SigningKey;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{Instant, sleep};

use crate::config::{FinalityTag, NodeConfig};
use crate::custody::evm::{DepositWatcher, VaultReader};
use crate::custody::quorum::Quorum;
use crate::custody::tests::fakes::{Logs, Node, usdc};
use crate::sequencer::Request;

#[tokio::test]
#[ignore = "run by scripts/dev-deposit-finality.sh with its private Anvil fixture"]
async fn anvil_confirmation_finality_scenario() {
    let path = std::env::var("ENGINE_FINALITY_FIXTURE").expect("Anvil fixture path");
    let rpc = std::env::var("ENGINE_FINALITY_RPC").expect("local Anvil RPC");
    let key = std::env::var("ENGINE_FINALITY_TEST_KEY").expect("published Anvil trader key");
    let signing_key = SigningKey::from_slice(&hex::decode(key.trim_start_matches("0x")).unwrap()).unwrap();
    let trader = eip712::address(signing_key.verifying_key());
    assert!(rpc.starts_with("http://127.0.0.1:") && rpc.rsplit(':').next().unwrap().parse::<u16>().is_ok());
    let cfg = NodeConfig::load(Path::new(&path)).unwrap();
    assert_eq!(cfg.custody.chain_id, 31337, "scenario requires the private Anvil chain");
    let (logs, _guard) = Logs::capture();
    let dir = Path::new(&path).parent().unwrap();
    let node = Node::start_evm(dir, cfg.custody.chain_id, cfg.custody.vault.parse().unwrap());
    assert!(node.read(|e| e.deposit_finality_enabled()).await, "this build activates deposit finality");
    let client = Trader {
        account: trader,
        key: signing_key,
        domain: eip712::domain(cfg.custody.chain_id, cfg.custody.vault.parse().unwrap()),
    };
    let mut evm = cfg.custody.evm.unwrap();
    evm.finality_tag = FinalityTag::Latest;
    evm.withdrawal_finality_tag = FinalityTag::Finalized;
    evm.confirmations = 0;
    for provider in &evm.providers {
        let url = provider.rpc_url.expose().unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("127.0.0.1"));
    }
    let vault = cfg.custody.vault.parse().unwrap();
    let quorum = Quorum::connect(&evm, vault).unwrap();
    let mut watcher = DepositWatcher::start(quorum, &evm, cfg.custody.chain_id, vault, None).unwrap();
    let dev = crate::test_support::dev_config();
    node.submit(Command::Admin(AdminCommand::SetParams(dev.genesis.params))).await;
    node.submit(Command::Admin(AdminCommand::UpsertMarket(dev.genesis.markets[0].clone()))).await;
    cast(&rpc, &["send", &cfg.custody.vault, "depositToHouse(uint256)", "1000000000", "--private-key", &key, "--json"])
        .await;
    deposit(&rpc, &cfg.custody.vault, &key, trader, "50000000").await;
    cast(&rpc, &["rpc", "anvil_mine", "0x4"]).await;
    catch_up(&mut watcher, &node.tx).await;
    assert_eq!(node.balance(trader).await, Some(usdc("50")));
    assert_eq!(node.read(move |e| e.accounts()[&trader].unfinalised_deposits).await, usdc("0"));
    deposit(&rpc, &cfg.custody.vault, &key, trader, "100000000").await;
    catch_up(&mut watcher, &node.tx).await;
    assert_eq!(node.balance(trader).await, Some(usdc("150")));
    assert_eq!(node.read(move |e| e.accounts()[&trader].unfinalised_deposits).await, usdc("100"));
    let now = node.read(|e| e.now()).await;
    node.submit(Command::Prices {
        market: 1,
        quotes: ["a", "b"]
            .map(|source| SourceQuote { source: source.into(), mid: usdc("100"), ts: now, contract: None })
            .to_vec(),
    })
    .await;
    let mut order = OrderRequest {
        account: trader,
        client_id: 1,
        market: 1,
        side: Side::Buy,
        // Margin for six units exceeds the finalised 50, so only pending principal can fund it.
        qty: usdc("6"),
        kind: OrderKind::Market { quoted: usdc("100"), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    };
    let (nonce, expires_at) = node.read(move |e| (e.next_request_nonce(trader), e.now() + 60_000)).await;
    order.auth = Some(RequestAuth {
        nonce,
        expires_at,
        signature: eip712::sign(&client.key, &eip712::order_hash(&client.domain, &order, nonce, expires_at)),
    });
    let fills = node.submit(Command::PlaceOrder(order)).await;
    assert!(
        fills.iter().any(|event| matches!(event, Event::Fill(fill) if fill.qty == usdc("6"))),
        "immediate trade: {fills:?}"
    );
    assert_eq!(
        node.submit(client.withdraw(&node, "51").await).await,
        vec![Event::WithdrawalRejected {
            account: trader,
            amount: usdc("51"),
            reason: Reject::InsufficientFunds { available: usdc("50") }
        }]
    );
    assert_eq!(
        node.submit(client.withdraw(&node, "50").await).await,
        vec![
            Event::WithdrawalQueued { account: trader, amount: usdc("50"), requested_seq: 15 },
            Event::WithdrawalApproved { account: trader, amount: usdc("50"), nonce: 1 },
            Event::WithdrawalQueueUpdated { account: trader, requested_seq: 15, remaining: usdc("0") },
        ]
    );
    cast(&rpc, &["rpc", "anvil_mine", "0x4"]).await;
    catch_up(&mut watcher, &node.tx).await;
    assert_eq!(node.balance(trader).await, Some(usdc("100")));
    assert_eq!(node.read(move |e| e.accounts()[&trader].unfinalised_deposits).await, usdc("0"));
    assert!(
        node.submit(client.withdraw(&node, "1").await)
            .await
            .iter()
            .any(|event| matches!(event, Event::WithdrawalApproved { nonce: 2, .. }))
    );
    let snapshot = cast(&rpc, &["rpc", "evm_snapshot"]).await;
    deposit(&rpc, &cfg.custody.vault, &key, trader, "100000000").await;
    catch_up(&mut watcher, &node.tx).await;
    assert_eq!(node.balance(trader).await, Some(usdc("199")));
    assert_eq!(cast(&rpc, &["rpc", "evm_revert", snapshot.as_str().unwrap()]).await, Value::Bool(true));
    cast(&rpc, &["rpc", "anvil_mine", "0x1"]).await;
    catch_up(&mut watcher, &node.tx).await;
    catch_up(&mut watcher, &node.tx).await;
    assert_eq!(node.balance(trader).await, Some(usdc("99")));
    let alerts = logs.lines_with("alert=\"deposit_reversed\"");
    assert_eq!(alerts.len(), 1);
    assert!(alerts[0].contains("ERROR"));
    assert_eq!(node.read(move |e| e.accounts()[&trader].unfinalised_deposits).await, usdc("0"));
    node.stop().await;
}

/// Polls until the watcher has nothing left to read; each poll reads one bounded batch.
async fn catch_up<V: VaultReader>(watcher: &mut DepositWatcher<V>, tx: &mpsc::Sender<Request>) {
    for _ in 0..20 {
        if watcher.poll(tx).await.unwrap().caught_up() {
            return;
        }
    }
    panic!("the watcher did not catch up");
}

/// Calls only the private loopback Anvil, with a bounded child lifetime and no arguments logged.
async fn cast(rpc: &str, args: &[&str]) -> Value {
    let mut child = Process::new("cast")
        .args(args)
        .args(["--rpc-url", rpc])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("local Anvil command exceeded its deadline");
        }
        sleep(Duration::from_millis(10)).await;
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "local Anvil command failed: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

async fn deposit(rpc: &str, vault: &str, key: &str, trader: AccountId, units: &str) {
    cast(
        rpc,
        &["send", vault, "depositFor(address,uint256)", &trader.to_string(), units, "--private-key", key, "--json"],
    )
    .await;
}

struct Trader {
    account: AccountId,
    key: SigningKey,
    domain: Eip712Domain,
}

impl Trader {
    /// Signs the actual account nonce and the fixture's chain/vault domain.
    async fn withdraw(&self, node: &Node, amount: &str) -> Command {
        let account = self.account;
        let (nonce, expires_at) = node.read(move |e| (e.next_request_nonce(account), e.now() + 60_000)).await;
        let amount = usdc(amount);
        let hash = eip712::withdrawal_hash(&self.domain, account, amount, nonce, expires_at);
        Command::Withdraw {
            account,
            amount,
            auth: Some(RequestAuth { nonce, expires_at, signature: eip712::sign(&self.key, &hash) }),
        }
    }
}
