//! Legacy Latest/Safe credits, independent finality proof, and actual upgrade/recovery boundaries.

use std::collections::BTreeMap;

use crate::sequencer::legacy::Approvals;
use alloy::primitives::{B256, U256};
use engine_core::Engine;
use engine_core::command::{Command, Sequenced};
use engine_core::deposits::LegacyDeposit;
use std::path::Path;

use crate::config::{EvmCustodySection, FinalityTag};
use crate::custody::evm::{Beneficiary, DepositWatcher, VaultDeposit};
use crate::custody::quorum::Quorum;
use crate::custody::tests::fakes::*;

mod cache;

#[tokio::test]
async fn legacy_latest_or_safe_credit_needs_independent_finality_before_signature_redelivery() {
    for tag in [FinalityTag::Latest, FinalityTag::Safe] {
        let dir = scratch("legacy-finality-proof");
        let mut cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, 4)).unwrap();
        cfg.finality_tag = tag;
        // A manipulated cursor is not consulted by the proof path, even if unreadable as a cursor.
        std::fs::write(&cfg.cursor_path, b"host claims every old deposit was finalized").unwrap();
        let (engine, ledger, approvals, d) = history();
        let chains = chains(Some(4), Some(d.clone()));
        let proof = prove(&chains, &cfg, &engine, &ledger, &approvals).await;
        assert!(proof.unwrap_err().to_string().contains("without canonical finality proof"));
        for chain in &chains {
            chain.0.lock().unwrap().withdrawal_block = Some(6);
        }
        prove(&chains, &cfg, &engine, &ledger, &approvals).await.unwrap();
        assert_eq!(engine.accounts()[&TRADER].balance, usdc("75"));
        assert_eq!(std::fs::read(&cfg.cursor_path).unwrap(), b"host claims every old deposit was finalized");
    }
}

#[tokio::test]
async fn missing_reorged_or_changed_legacy_receipts_and_no_quorum_cannot_authorise_an_old_payout() {
    let dir = scratch("legacy-proof-errors");
    let cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, 4)).unwrap();
    let (engine, ledger, approvals, d) = history();
    for receipt in [
        None,
        Some(VaultDeposit { log_index: d.log_index + 1, ..d.clone() }),
        Some(VaultDeposit { units: U256::from(101 * USDC), ..d.clone() }),
    ] {
        assert!(prove(&chains(Some(6), receipt), &cfg, &engine, &ledger, &approvals).await.is_err());
    }
    assert!(
        prove(&chains(None, Some(d.clone())), &cfg, &engine, &ledger, &approvals)
            .await
            .unwrap_err()
            .to_string()
            .contains("no finalised block")
    );
    let chains = chains(Some(6), Some(d));
    chains[0].0.lock().unwrap().failing = true;
    prove(&chains, &cfg, &engine, &ledger, &approvals).await.unwrap();
    chains[1].0.lock().unwrap().failing = true;
    assert!(prove(&chains, &cfg, &engine, &ledger, &approvals).await.is_err());
}

#[tokio::test]
async fn legacy_upgrade_locks_only_real_evm_principal_until_proof_and_ignores_a_forward_cursor() {
    assert!(
        engine_core::protocol::ProtocolVersion::LATEST.supports_deposit_finality(),
        "this build activates deposit finality"
    );
    let dir = scratch("legacy-real-upgrade");
    let node = start_old(&dir).await;
    let d = legacy_receipt();
    let chains = chains(Some(4), Some(d));
    let mut cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, 100)).unwrap();
    cfg.finality_tag = FinalityTag::Latest;
    let mut old_watcher = DepositWatcher::start_legacy(quorum(&chains), &cfg, CHAIN_ID, VAULT, None).unwrap();
    old_watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")), "legacy latest credit predates release block 5");
    node.submit(Command::Deposit { account: TRADER, amount: usdc("50"), chain_ref: "dev:unrelated".into() }).await;
    node.stop().await;
    let node = Node::start_confirmed(&dir);
    assert!(node.read(|e| e.deposit_finality_enabled()).await, "this build activates deposit finality");
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    assert!(
        node.submit(withdraw("51"))
            .await
            .iter()
            .any(|e| matches!(e, engine_core::event::Event::WithdrawalRejected { .. }))
    );
    assert!(
        node.submit(withdraw("50"))
            .await
            .iter()
            .any(|e| matches!(e, engine_core::event::Event::WithdrawalApproved { .. }))
    );
    let cursor = crate::custody::cursor::Cursor {
        chain_id: CHAIN_ID,
        vault: VAULT,
        next_block: 10000,
        next_deposit_id: 1000,
        log_seq: node.last_seq().await,
        last_block_hash: Some(B256::repeat_byte(99)),
        crediting: None,
    };
    cursor.store(&cfg.cursor_path).unwrap();
    let mut watcher =
        DepositWatcher::start_unscreened(quorum(&chains), &cfg, CHAIN_ID, VAULT, node.last_seq().await).unwrap();
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("100")), usdc("100"))
    );
    assert!(
        node.submit(withdraw("1"))
            .await
            .iter()
            .any(|e| matches!(e, engine_core::event::Event::WithdrawalRejected { .. }))
    );
    for chain in &chains {
        chain.0.lock().unwrap().withdrawal_block = Some(6);
    }
    watcher.poll(&node.tx).await.unwrap();
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("0"));
    assert!(
        node.submit(withdraw("1"))
            .await
            .iter()
            .any(|e| matches!(e, engine_core::event::Event::WithdrawalApproved { .. }))
    );
    node.stop().await;
}

#[tokio::test]
async fn missing_legacy_identity_keeps_only_its_principal_locked_while_independent_final_funds_release() {
    assert!(
        engine_core::protocol::ProtocolVersion::LATEST.supports_deposit_finality(),
        "this build activates deposit finality"
    );
    let dir = scratch("legacy-missing-identity");
    let node = start_old(&dir).await;
    let missing = legacy_receipt();
    let known = VaultDeposit { tx_hash: B256::repeat_byte(3), ..deposit(0, 6, Beneficiary::Trader(TRADER), 50 * USDC) };
    for d in [&missing, &known] {
        node.submit(Command::Deposit {
            account: TRADER,
            amount: crate::custody::usdc_to_fixed(d.units).unwrap(),
            chain_ref: d.chain_ref(),
        })
        .await;
    }
    node.stop().await;
    let (logs, _guard) = Logs::capture();
    let node = Node::start_confirmed(&dir);
    let chains = chains(Some(6), Some(known));
    let cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, 100)).unwrap();
    let mut watcher =
        DepositWatcher::start_unscreened(quorum(&chains), &cfg, CHAIN_ID, VAULT, node.last_seq().await).unwrap();
    watcher.poll(&node.tx).await.unwrap();
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("150")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    assert_eq!(node.read(|e| e.unresolved_legacy_deposit().unwrap().chain_ref.clone()).await, missing.chain_ref());
    assert!(logs.lines_with("alert=\"legacy_deposit_unproven\"").iter().any(|line| line.contains("ERROR")));
    assert!(
        node.submit(withdraw("51"))
            .await
            .iter()
            .any(|e| matches!(e, engine_core::event::Event::WithdrawalRejected { .. }))
    );
    assert!(
        node.submit(withdraw("50"))
            .await
            .iter()
            .any(|e| matches!(e, engine_core::event::Event::WithdrawalApproved { .. }))
    );
    node.stop().await;
}

#[tokio::test]
async fn an_adopted_legacy_receipt_moves_to_a_moved_index_once_with_critical_and_no_extra_cash() {
    assert!(
        engine_core::protocol::ProtocolVersion::LATEST.supports_deposit_finality(),
        "this build activates deposit finality"
    );
    let dir = scratch("legacy-adopted-reorg");
    let node = start_old(&dir).await;
    let d = legacy_receipt();
    node.submit(Command::Deposit { account: TRADER, amount: usdc("100"), chain_ref: d.chain_ref() }).await;
    node.submit(Command::Deposit { account: TRADER, amount: usdc("50"), chain_ref: "dev:unrelated".into() }).await;
    node.stop().await;
    let (logs, _guard) = Logs::capture();
    let node = Node::start_confirmed(&dir);
    let chains = chains(Some(4), Some(d));
    let cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, 100)).unwrap();
    let mut watcher =
        DepositWatcher::start_unscreened(quorum(&chains), &cfg, CHAIN_ID, VAULT, node.last_seq().await).unwrap();
    watcher.poll(&node.tx).await.unwrap();
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        state.forks.extend((5..=7).map(|height| (height, B256::from(alloy::primitives::U256::from(100 + height)))));
        let hash = state.forks[&5];
        state.deposits[0].block_hash = hash;
        state.deposits[0].log_index = 2;
    }
    watcher.poll(&node.tx).await.unwrap();
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("150")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    let alerts = logs.lines_with("alert=\"deposit_reversed\"");
    assert_eq!(alerts.len(), 1);
    assert!(alerts[0].contains("ERROR"));
    let moves = crate::test_support::log_entries(&dir.join("log"))
        .iter()
        .filter_map(|entry| match entry {
            crate::log::LogEntry::Command(c) => match &c.sequenced.cmd {
                Command::CustodyDeposit(update) => Some(update.as_ref().clone()),
                _ => None,
            },
            _ => None,
        })
        .filter(|update| {
            matches!(
                update,
                engine_core::deposits::DepositUpdate::Reverse { .. }
                    | engine_core::deposits::DepositUpdate::Replace { .. }
            )
        })
        .collect::<Vec<_>>();
    assert!(
        matches!(moves.as_slice(), [engine_core::deposits::DepositUpdate::Replace { .. }]),
        "the adopted receipt moves once to its moved index: {moves:?}"
    );
    node.stop().await;
}

#[tokio::test]
async fn a_legacy_alert_resolves_once_when_its_receipt_is_adopted_and_credits_resolve_nothing() {
    let dir = scratch("legacy-alert-resolve");
    let node = start_old(&dir).await;
    let d = legacy_receipt();
    node.submit(Command::Deposit { account: TRADER, amount: usdc("100"), chain_ref: d.chain_ref() }).await;
    node.stop().await;
    let (logs, _guard) = Logs::capture();
    let node = Node::start_confirmed(&dir);
    // The confirmed chain ends before the legacy receipt's block, so it is not yet established.
    let chains = chains(None, None);
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        (state.head, state.final_block) = (4, Some(4));
    }
    let cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, 100)).unwrap();
    let mut watcher =
        DepositWatcher::start_unscreened(quorum(&chains), &cfg, CHAIN_ID, VAULT, node.last_seq().await).unwrap();
    watcher.poll(&node.tx).await.unwrap();
    watcher.poll(&node.tx).await.unwrap();
    assert!(logs.count("alert=\"legacy_deposit_unproven\"") >= 1);
    assert_eq!(logs.count("alert_resolved"), 0);
    for chain in &chains {
        chain.add(d.clone());
        let mut state = chain.0.lock().unwrap();
        (state.head, state.final_block) = (7, Some(7));
    }
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.read(|e| e.unresolved_legacy_deposit().cloned()).await, None);
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("100")), usdc("100")),
        "adoption keeps the lock and moves no cash"
    );
    let resolved = logs.lines_with("alert_resolved");
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert!(resolved[0].contains("alert_resolved=\"legacy_deposit_unproven\""), "{resolved:?}");
    assert!(resolved[0].contains(&d.chain_ref()), "{resolved:?}");
    let alerted = logs.count("alert=\"legacy_deposit_unproven\"");
    // A later ordinary credit and further polls neither resolve nor alert again.
    for chain in &chains {
        chain.add(VaultDeposit {
            tx_hash: B256::repeat_byte(9),
            ..deposit(1, 8, Beneficiary::Trader(TRADER), 20 * USDC)
        });
        let mut state = chain.0.lock().unwrap();
        (state.head, state.final_block) = (9, Some(9));
    }
    watcher.poll(&node.tx).await.unwrap();
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("120")));
    assert_eq!(logs.count("alert_resolved"), 1);
    assert_eq!(logs.count("alert=\"legacy_deposit_unproven\""), alerted, "a resolved credit is not alerted again");
    node.stop().await;
}

#[tokio::test]
async fn a_moved_index_of_an_unresolved_legacy_credit_is_withheld_with_critical() {
    let dir = scratch("legacy-moved-unresolved");
    let node = start_old(&dir).await;
    let d = legacy_receipt();
    node.submit(Command::Deposit { account: TRADER, amount: usdc("100"), chain_ref: d.chain_ref() }).await;
    node.stop().await;
    let (logs, _guard) = Logs::capture();
    let node = Node::start_confirmed(&dir);
    // The legacy transaction is canonical, but at another log index than the credited one.
    let chains = chains(None, Some(VaultDeposit { log_index: 2, ..d.clone() }));
    let cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, 100)).unwrap();
    let mut watcher =
        DepositWatcher::start_unscreened(quorum(&chains), &cfg, CHAIN_ID, VAULT, node.last_seq().await).unwrap();
    watcher.poll(&node.tx).await.unwrap();
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("100")), usdc("100")),
        "the moved index is not credited again"
    );
    assert_eq!(node.read(|e| e.unresolved_legacy_deposit().map(|l| l.chain_ref.clone())).await, Some(d.chain_ref()));
    assert!(logs.lines_with("alert=\"legacy_deposit_unproven\"").iter().any(|line| line.contains("ERROR")));
    node.stop().await;
    let receipts = crate::test_support::log_entries(&dir.join("log"))
        .iter()
        .filter(|entry| matches!(entry, crate::log::LogEntry::Command(c) if matches!(c.sequenced.cmd, Command::CustodyDeposit(_))))
        .count();
    assert_eq!(receipts, 0);
}

#[tokio::test]
async fn an_adopted_legacy_receipt_reincluded_after_the_head_at_another_index_credits_once() {
    let dir = scratch("legacy-adopted-late");
    let node = start_old(&dir).await;
    let d = legacy_receipt();
    node.submit(Command::Deposit { account: TRADER, amount: usdc("100"), chain_ref: d.chain_ref() }).await;
    node.stop().await;
    let node = Node::start_confirmed(&dir);
    let chains = chains(None, Some(d.clone()));
    let cfg: EvmCustodySection = toml::from_str(&evm_toml(&dir, 100)).unwrap();
    let mut watcher =
        DepositWatcher::start_unscreened(quorum(&chains), &cfg, CHAIN_ID, VAULT, node.last_seq().await).unwrap();
    watcher.poll(&node.tx).await.unwrap();
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.read(|e| e.unresolved_legacy_deposit().cloned()).await, None, "the legacy credit is adopted");
    // A reorg drops the transaction with no replacement up to the head, so the receipt is debited.
    for chain in &chains {
        chain.remove(&d);
        let mut state = chain.0.lock().unwrap();
        state.forks.extend((5..=7).map(|height| (height, B256::from(U256::from(100 + height)))));
    }
    for _ in 0..3 {
        watcher.poll(&node.tx).await.unwrap();
    }
    assert_eq!(node.balance(TRADER).await, Some(usdc("0")));
    // The transaction returns beyond that head, at another log index: a fresh receipt credits once.
    let late = VaultDeposit { tx_hash: d.tx_hash, ..deposit(0, 9, Beneficiary::Trader(TRADER), 100 * USDC) };
    let late = VaultDeposit { log_index: 2, ..late };
    for chain in &chains {
        chain.add(late.clone());
        let mut state = chain.0.lock().unwrap();
        (state.head, state.final_block) = (9, Some(9));
    }
    watcher.poll(&node.tx).await.unwrap();
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("100")), usdc("100"))
    );
    node.stop().await;
    let updates: Vec<_> = crate::test_support::log_entries(&dir.join("log"))
        .into_iter()
        .filter_map(|entry| match entry {
            crate::log::LogEntry::Command(c) => match c.sequenced.cmd {
                Command::CustodyDeposit(update) => Some(*update),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(
        matches!(
            updates.as_slice(),
            [
                engine_core::deposits::DepositUpdate::Credit { .. },
                engine_core::deposits::DepositUpdate::Reverse { .. },
                engine_core::deposits::DepositUpdate::Credit { .. },
            ]
        ),
        "adoption, the debit at the head, and one late credit: {updates:?}"
    );
}

async fn start_old(dir: &Path) -> Node {
    let node = Node::start(dir);
    for cmd in [
        engine_core::command::AdminCommand::SetClientAuth(engine_core::auth::ClientAuth::Unsigned),
        engine_core::command::AdminCommand::SetOperator(node.operator),
        engine_core::command::AdminCommand::ActivateProtocol { version: engine_core::protocol::ProtocolVersion::V17 },
    ] {
        let events = node.submit(Command::Admin(cmd)).await;
        assert!(events.contains(&engine_core::event::Event::AdminApplied), "{events:?}");
    }
    node
}

fn legacy_receipt() -> VaultDeposit {
    VaultDeposit { tx_hash: B256::repeat_byte(2), ..deposit(0, 5, Beneficiary::Trader(TRADER), 100 * USDC) }
}

fn history() -> (Engine, BTreeMap<String, LegacyDeposit>, Approvals, VaultDeposit) {
    let mut engine = Engine::new();
    let d = legacy_receipt();
    let commands = [
        Command::Deposit { account: TRADER, amount: usdc("100"), chain_ref: d.chain_ref() },
        Command::Deposit { account: TRADER, amount: usdc("25"), chain_ref: "dev:mock".into() },
        withdraw("50"),
    ];
    let mut ledger = BTreeMap::new();
    let mut approvals = Approvals::default();
    for (seq, cmd) in commands.into_iter().enumerate() {
        let sequenced = Sequenced { seq: seq as u64, ts: 1, cmd: cmd.clone() };
        let events = engine.apply(&sequenced).unwrap();
        approvals.observe(&engine, &sequenced, &events, true);
        if let Some(d) = LegacyDeposit::from_command(&cmd, &events) {
            ledger.insert(d.chain_ref.clone(), d);
        }
    }
    (engine, ledger, approvals, d)
}

fn withdraw(amount: &str) -> Command {
    Command::Withdraw { account: TRADER, amount: usdc(amount), auth: None }
}

fn chains(release: Option<u64>, d: Option<VaultDeposit>) -> [FakeChain; 3] {
    std::array::from_fn(|_| {
        let chain = FakeChain::default();
        if let Some(d) = &d {
            chain.add(d.clone());
        }
        let mut s = chain.0.lock().unwrap();
        s.head = 7;
        s.final_block = Some(7);
        s.withdrawal_block = release;
        drop(s);
        chain
    })
}

fn quorum(chains: &[FakeChain; 3]) -> Quorum<FakeChain> {
    Quorum::new(chains.iter().enumerate().map(|(i, c)| (format!("p{i}"), c.clone())).collect()).unwrap()
}

async fn prove(
    chains: &[FakeChain; 3],
    cfg: &EvmCustodySection,
    engine: &Engine,
    ledger: &BTreeMap<String, LegacyDeposit>,
    approvals: &Approvals,
) -> anyhow::Result<()> {
    DepositWatcher::prove_legacy_withdrawals(quorum(chains), cfg, CHAIN_ID, VAULT, engine, ledger, approvals)
        .await
        .map(|_| ())
}
