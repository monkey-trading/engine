//! Confirmed deposits through the actual quorum, watcher and durable sequencer.

use std::path::Path;

use alloy::primitives::{B256, U256};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest};
use engine_core::deposits::DepositUpdate;
use engine_core::event::{Event, Reject};
use engine_core::price::SourceQuote;
use engine_core::types::Side;
use tokio::sync::{mpsc, oneshot};

use crate::config::EvmCustodySection;
use crate::custody::evm::{Beneficiary, DepositWatcher, VaultDeposit};
use crate::custody::quorum::Quorum;
use crate::custody::stall::PollError;
use crate::custody::tests::fakes::*;
use crate::sequencer::Request;
use crate::test_support::dev_config;

mod episodes;
mod frozen;
mod reinclusion;
mod scan;

#[tokio::test]
async fn scripted_confirmation_finality_scenario() {
    let dir = scratch("confirmed-scenario");
    let (logs, _guard) = Logs::capture();
    let node = Node::start_confirmed(&dir);
    assert!(node.read(|e| e.deposit_finality_enabled()).await, "this build activates deposit finality");
    configure_trading(&node).await;
    let chains = chains(10, Some(2));
    let finalised = confirmed_deposit(0, 1, 50);
    let pending = confirmed_deposit(1, 5, 100);
    add(&chains, &finalised);
    add(&chains, &pending);
    let mut watcher = watcher(&chains, &dir, 100, None);
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("150")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    let now = node.read(|e| e.now()).await;
    node.submit(Command::Prices {
        market: 1,
        quotes: vec![
            SourceQuote { source: "a".into(), mid: usdc("100"), ts: now, contract: None },
            SourceQuote { source: "b".into(), mid: usdc("100"), ts: now, contract: None },
        ],
    })
    .await;
    let fills = node
        .submit(Command::PlaceOrder(OrderRequest {
            account: TRADER,
            client_id: 1,
            market: 1,
            side: Side::Buy,
            // Margin for six units exceeds the finalised 50, so only pending principal can fund it.
            qty: usdc("6"),
            kind: OrderKind::Market { quoted: usdc("100"), max_slippage_bps: None },
            reduce_only: false,
            auth: None,
        }))
        .await;
    assert!(
        fills.iter().any(|event| matches!(event, Event::Fill(fill) if fill.qty == usdc("6"))),
        "pending principal is trading collateral: {fills:?}"
    );
    assert_eq!(
        node.submit(withdraw("51")).await,
        vec![Event::WithdrawalRejected {
            account: TRADER,
            amount: usdc("51"),
            reason: Reject::InsufficientFunds { available: usdc("50") }
        }]
    );
    assert_eq!(
        node.submit(withdraw("50")).await,
        vec![Event::WithdrawalApproved { account: TRADER, amount: usdc("50"), nonce: 1 }]
    );
    set_release(&chains, Some(10));
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")), "finality must not credit again");
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("0"));
    assert!(
        node.submit(withdraw("1"))
            .await
            .iter()
            .any(|event| matches!(event, Event::WithdrawalApproved { nonce: 2, .. }))
    );
    let orphan = confirmed_deposit(2, 11, 100);
    add(&chains, &orphan);
    set_head(&chains, 11);
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("199")));
    for chain in &chains {
        chain.remove(&orphan);
        chain.0.lock().unwrap().forks.insert(11, B256::repeat_byte(0x77));
    }
    watcher.poll(&node.tx).await.unwrap();
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("99")));
    let alerts = logs.lines_with("alert=\"deposit_reversed\"");
    assert_eq!(alerts.len(), 1);
    assert!(alerts[0].contains("ERROR"));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("0"));
    node.stop().await;
}

#[tokio::test]
async fn a_modern_watcher_cannot_credit_a_legacy_command_while_activation_is_pending() {
    let dir = scratch("confirmed-activation-boundary");
    let node = Node::start(&dir);
    assert!(node.read(|e| e.protocol_version() < engine_core::protocol::ProtocolVersion::LATEST).await);
    assert!(node.read(|_| engine_core::protocol::ProtocolVersion::LATEST.supports_deposit_finality()).await);
    let chains = chains(5, None);
    add(&chains, &confirmed_deposit(0, 3, 100));
    let mut watcher = watcher(&chains, &dir, 10, None);
    assert!(watcher.poll(&node.tx).await.is_err(), "inactive core must refuse confirmed receipts");
    assert_eq!(node.balance(TRADER).await, None);
    node.stop().await;
    let entries = crate::test_support::log_entries(&dir.join("log"));
    assert!(entries.iter().any(|entry| matches!(entry, crate::log::LogEntry::Command(command) if matches!(command.sequenced.cmd, Command::CustodyDeposit(_)))));
    assert!(!entries.iter().any(|entry| matches!(entry, crate::log::LogEntry::Command(command) if matches!(command.sequenced.cmd, Command::Deposit { .. } | Command::HouseCapital { .. }))));
}

#[tokio::test]
async fn multiple_deposits_in_one_transaction_have_distinct_locks_and_credit_once_each() {
    let dir = scratch("confirmed-multiple-events");
    let node = Node::start_confirmed(&dir);
    assert!(node.read(|e| e.deposit_finality_enabled()).await);
    let chains = chains(5, None);
    let first = confirmed_deposit(0, 3, 100);
    let second =
        VaultDeposit { tx_hash: first.tx_hash, log_index: first.log_index + 1, ..confirmed_deposit(1, 3, 100) };
    add(&chains, &first);
    add(&chains, &second);
    let mut watcher = watcher(&chains, &dir, 10, None);
    watcher.poll(&node.tx).await.unwrap();
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("200")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("200"));
    set_release(&chains, Some(5));
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("200")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("0"));
    node.stop().await;
    let credits = crate::test_support::log_entries(&dir.join("log")).iter().filter(|entry| {
        matches!(entry, crate::log::LogEntry::Command(command) if matches!(&command.sequenced.cmd, Command::CustodyDeposit(update) if matches!(update.as_ref(), DepositUpdate::Credit { .. })))
    }).count();
    assert_eq!(credits, 2);
}

#[tokio::test]
async fn withdrawal_finality_quorum_loss_pauses_credit_and_release_until_recovery() {
    let dir = scratch("confirmed-release-quorum");
    let node = Node::start_confirmed(&dir);
    assert!(node.read(|e| e.deposit_finality_enabled()).await);
    let (logs, _guard) = Logs::capture();
    let chains = chains(5, None);
    add(&chains, &confirmed_deposit(0, 3, 100));
    let mut watcher = watcher(&chains, &dir, 10, None);
    watcher.poll(&node.tx).await.unwrap();
    add(&chains, &confirmed_deposit(1, 6, 20));
    set_head(&chains, 10);
    set_release(&chains, Some(9));
    for chain in &chains[..2] {
        chain.0.lock().unwrap().withdrawal_failing = true;
    }
    assert!(watcher.poll(&node.tx).await.is_err());
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    assert_eq!(logs.count("alert=\"custody_rpc_quorum_lost\" operation=\"withdrawal_finality\""), 1);
    let before = node.last_seq().await;
    node.submit(Command::Tick).await;
    assert!(node.last_seq().await > before);
    for chain in &chains[..2] {
        chain.0.lock().unwrap().withdrawal_failing = false;
    }
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("120")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("0"));
    node.stop().await;
}

#[tokio::test]
async fn minority_forks_and_missing_quorum_cannot_debit_or_unlock_a_credited_deposit() {
    let dir = scratch("confirmed-quorum");
    let node = Node::start_confirmed(&dir);
    assert!(node.read(|e| e.deposit_finality_enabled()).await);
    let chains = chains(5, None);
    let d = confirmed_deposit(0, 3, 100);
    add(&chains, &d);
    let mut watcher = watcher(&chains, &dir, 10, None);
    watcher.poll(&node.tx).await.unwrap();
    chains[0].remove(&d);
    fork(&chains[0], 3, 5);
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    chains[1].0.lock().unwrap().failing = true;
    assert!(watcher.poll(&node.tx).await.is_err());
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    chains[1].0.lock().unwrap().failing = false;
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    node.stop().await;
}

#[tokio::test]
async fn finality_stall_alerts_while_confirmed_credit_continues() {
    let dir = scratch("confirmed-finality-stall");
    let node = Node::start_confirmed(&dir);
    assert!(node.read(|e| e.deposit_finality_enabled()).await);
    let (logs, _guard) = Logs::capture();
    let chains = chains(10, Some(1));
    let mut watcher = watcher(&chains, &dir, 100, None);
    watcher.poll(&node.tx).await.unwrap();
    set_head(&chains, 11);
    watcher.poll(&node.tx).await.unwrap();
    // The test controls the watchdog budget instead of sleeping on a production interval.
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    set_head(&chains, 12);
    add(&chains, &confirmed_deposit(0, 12, 100));
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(logs.count("alert=\"custody_finality_stalled\""), 1);
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    set_release(&chains, Some(12));
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(logs.count("alert_resolved=\"custody_finality_stalled\""), 1);
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("0"));
    tokio::time::resume();
    node.stop().await;
}

#[tokio::test]
async fn configured_confirmation_depth_and_missing_tags_bound_early_credit() {
    let dir = scratch("confirmed-depth");
    let node = Node::start_confirmed(&dir);
    assert!(node.read(|e| e.deposit_finality_enabled()).await);
    let chains = chains(2, None);
    add(&chains, &confirmed_deposit(0, 1, 100));
    let mut cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, 10)).unwrap();
    cfg.confirmations = 2;
    let quorum = Quorum::new(chains.iter().enumerate().map(|(i, c)| (format!("p{i}"), c.clone())).collect()).unwrap();
    let mut watcher = DepositWatcher::start(quorum, &cfg, CHAIN_ID, VAULT, None).unwrap();
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, None);
    for chain in &chains {
        chain.set_final(None);
    }
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, None);
    set_head(&chains, 3);
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    set_release(&chains, Some(1));
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("0"));
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    node.stop().await;
}

#[tokio::test]
async fn durable_transitions_survive_lost_replies_at_every_delivery_boundary() {
    for cutoff in [Cutoff::Credit, Cutoff::Finalise, Cutoff::Progress, Cutoff::Reverse, Cutoff::Replace] {
        let dir = scratch("confirmed-delivery-boundary");
        let node = Node::start_confirmed(&dir);
        assert!(node.read(|e| e.deposit_finality_enabled()).await);
        let released = matches!(cutoff, Cutoff::Finalise | Cutoff::Progress).then_some(5);
        let chains = chains(5, released);
        let original = confirmed_deposit(0, 3, 100);
        add(&chains, &original);
        let mut w = watcher(&chains, &dir, 10, None);
        if matches!(cutoff, Cutoff::Reverse | Cutoff::Replace) {
            w.poll(&node.tx).await.unwrap();
            let moved = VaultDeposit { block: 5, block_hash: fork_hash(5), log_index: 9, ..original.clone() };
            for chain in &chains {
                chain.remove(&original);
                if matches!(cutoff, Cutoff::Replace) {
                    chain.add(moved.clone());
                }
                fork(chain, 3, 5);
            }
        }
        let relay = lose_reply_after(&node.tx, cutoff);
        poll_until_error(&mut w, &relay).await;
        assert!(relay.is_closed(), "the watcher must observe the lost sequencer reply");
        let expected = if matches!(cutoff, Cutoff::Reverse) { "0" } else { "100" };
        assert_eq!(node.balance(TRADER).await, Some(usdc(expected)));
        node.stop().await;
        let node = Node::start_confirmed(&dir);
        let mut w = watcher(&chains, &dir, 10, node.last_seq().await);
        catch_up(&mut w, &node.tx).await;
        assert_eq!(node.balance(TRADER).await, Some(usdc(expected)));
        let locked = if matches!(cutoff, Cutoff::Credit | Cutoff::Replace) { "100" } else { "0" };
        assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc(locked));
        node.stop().await;
        let credits = transitions(&dir, |update| matches!(update, DepositUpdate::Credit { .. }));
        assert_eq!(credits, 1, "durable credit must never be submitted twice after reply loss");
        let moves = transitions(&dir, |update| matches!(update, DepositUpdate::Replace { .. }));
        assert_eq!(moves, usize::from(matches!(cutoff, Cutoff::Replace)), "a durable move is never repeated");
    }
}

#[tokio::test]
async fn canonical_reinclusion_across_ranges_and_restart_moves_the_receipt_once() {
    for log_index in [1, 8] {
        let (logs, _guard) = Logs::capture();
        let dir = scratch("confirmed-reinclude");
        let node = Node::start_confirmed(&dir);
        assert!(node.read(|e| e.deposit_finality_enabled()).await);
        let chains = chains(5, None);
        let original = confirmed_deposit(0, 3, 100);
        add(&chains, &original);
        let mut w = watcher(&chains, &dir, 4, None);
        catch_up(&mut w, &node.tx).await;
        assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
        node.stop().await;
        let moved = VaultDeposit { block: 12, block_hash: fork_hash(12), log_index, ..original.clone() };
        for chain in &chains {
            chain.remove(&original);
            chain.add(moved.clone());
            fork(chain, 3, 12);
        }
        set_head(&chains, 12);
        let node = Node::start_confirmed(&dir);
        let mut w = watcher(&chains, &dir, 4, node.last_seq().await);
        w.cursor.next_block = 10_000;
        w.cursor.next_deposit_id = 999;
        catch_up(&mut w, &node.tx).await;
        assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
        assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
        set_release(&chains, Some(12));
        catch_up(&mut w, &node.tx).await;
        assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
        assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("0"));
        let alerts = logs.lines_with("alert=\"deposit_reversed\"");
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].contains("ERROR"));
        node.stop().await;
        assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Credit { .. })), 1);
        assert_eq!(
            transitions(&dir, |update| matches!(update, DepositUpdate::Replace { .. })),
            1,
            "the reincluded deposit moves to its replacement once"
        );
        assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Reverse { .. })), 0);
    }
}

// ── Helpers ──

#[derive(Clone, Copy)]
enum Cutoff {
    Credit,
    Finalise,
    Progress,
    Reverse,
    Replace,
}

/// Polls until the watcher has nothing left to read: it reached the confirmed tag, or waits on it.
async fn catch_up(scan: &mut DepositWatcher<Quorum<FakeChain>>, tx: &mpsc::Sender<Request>) {
    for _ in 0..80 {
        if scan.poll(tx).await.unwrap().caught_up() {
            return;
        }
    }
    panic!("the watcher did not catch up");
}

/// Polls until a poll fails, returning its error.
async fn poll_until_error(scan: &mut DepositWatcher<Quorum<FakeChain>>, tx: &mpsc::Sender<Request>) -> PollError {
    for _ in 0..80 {
        if let Err(error) = scan.poll(tx).await {
            return error;
        }
    }
    panic!("no poll failed");
}

/// How many logged receipt transitions match `kind`.
fn transitions(dir: &Path, kind: impl Fn(&DepositUpdate) -> bool) -> usize {
    crate::test_support::log_entries(&dir.join("log"))
        .iter()
        .filter(|entry| {
            matches!(entry, crate::log::LogEntry::Command(command)
                if matches!(&command.sequenced.cmd, Command::CustodyDeposit(update) if kind(update)))
        })
        .count()
}

/// Forwards to the actual sequencer, then loses precisely one durable transition's reply.
fn lose_reply_after(real: &mpsc::Sender<Request>, cutoff: Cutoff) -> mpsc::Sender<Request> {
    let real = real.clone();
    let (tx, mut rx) = mpsc::channel::<Request>(64);
    tokio::spawn(async move {
        while let Some(request) = rx.recv().await {
            let stop = match &request {
                Request::Submit { cmd: Command::CustodyFinalised(_), .. } => matches!(cutoff, Cutoff::Progress),
                Request::Submit { cmd: Command::CustodyDeposit(update), .. } => matches!(
                    (cutoff, update.as_ref()),
                    (Cutoff::Credit, DepositUpdate::Credit { .. })
                        | (Cutoff::Finalise, DepositUpdate::Finalise { .. })
                        | (Cutoff::Reverse, DepositUpdate::Reverse { .. })
                        | (Cutoff::Replace, DepositUpdate::Replace { .. })
                ),
                _ => false,
            };
            if stop {
                let Request::Submit { cmd, reply } = request else { unreachable!() };
                let (durable, received) = oneshot::channel();
                real.send(Request::Submit { cmd, reply: Some(durable) }).await.unwrap();
                received.await.unwrap();
                drop(reply);
                return;
            }
            real.send(request).await.unwrap();
        }
    });
    tx
}

fn chains(head: u64, release: Option<u64>) -> [FakeChain; 3] {
    std::array::from_fn(|_| {
        let chain = FakeChain::default();
        {
            let mut state = chain.0.lock().unwrap();
            state.head = head;
            state.final_block = Some(head);
            state.withdrawal_block = release;
        }
        chain
    })
}

fn watcher(chains: &[FakeChain; 3], dir: &Path, range: u64, seq: Option<u64>) -> DepositWatcher<Quorum<FakeChain>> {
    let cfg: EvmCustodySection = toml::from_str(&evm_toml(dir, range)).unwrap();
    let quorum =
        Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    DepositWatcher::start(quorum, &cfg, CHAIN_ID, VAULT, seq).unwrap()
}

fn confirmed_deposit(id: u64, block: u64, amount: u64) -> VaultDeposit {
    VaultDeposit {
        tx_hash: B256::repeat_byte(id as u8 + 1),
        ..deposit(id, block, Beneficiary::Trader(TRADER), amount * USDC)
    }
}

fn add(chains: &[FakeChain; 3], deposit: &VaultDeposit) {
    for chain in chains {
        chain.add(deposit.clone());
    }
}

fn set_head(chains: &[FakeChain; 3], height: u64) {
    for chain in chains {
        let mut state = chain.0.lock().unwrap();
        state.head = height;
        state.final_block = Some(height);
    }
}

fn set_release(chains: &[FakeChain; 3], height: Option<u64>) {
    for chain in chains {
        chain.0.lock().unwrap().withdrawal_block = height;
    }
}

fn withdraw(amount: &str) -> Command {
    Command::Withdraw { account: TRADER, amount: usdc(amount), auth: None }
}

async fn configure_trading(node: &Node) {
    let cfg = dev_config();
    node.submit(Command::Admin(AdminCommand::SetParams(cfg.genesis.params))).await;
    node.submit(Command::Admin(AdminCommand::UpsertMarket(cfg.genesis.markets[0].clone()))).await;
    node.submit(Command::HouseCapital { amount: usdc("1000000"), chain_ref: "scenario-house".into() }).await;
}

/// Rewrites every descendant too, as a real block hash commits to its parent.
fn fork(chain: &FakeChain, from: u64, to: u64) {
    let mut state = chain.0.lock().unwrap();
    state.forks.extend((from..=to).map(|height| (height, fork_hash(height))));
}

fn fork_hash(height: u64) -> B256 {
    B256::from(U256::from(0xf000 + height))
}
