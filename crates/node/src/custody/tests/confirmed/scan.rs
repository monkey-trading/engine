//! Empty confirmed windows advance independently of finality, without trusting a host cursor.

use std::time::Duration;

use tokio::time::sleep;

use super::*;
use crate::custody::cursor::Cursor;
use crate::custody::stall::{PollError, Polled, Stall};

#[tokio::test]
async fn empty_windows_reach_confirmed_collateral_while_finality_is_frozen() {
    let dir = scratch("confirmed-empty-prefix");
    let node = Node::start_confirmed(&dir);
    configure_trading(&node).await;
    let chains = chains(2, Some(2));
    let mut scan = watcher(&chains, &dir, 4, None);
    assert_eq!(scan.poll(&node.tx).await.unwrap(), Polled::Read { caught_up: true });
    set_head(&chains, 12);
    add(&chains, &confirmed_deposit(0, 9, 100));
    assert_eq!(scan.poll(&node.tx).await.unwrap(), Polled::Read { caught_up: false });
    assert_eq!(node.balance(TRADER).await, None);
    // The deposit's window is credited before the scan reaches the confirmed tag.
    for _ in 0..10 {
        if node.balance(TRADER).await.is_some() {
            break;
        }
        assert_eq!(scan.poll(&node.tx).await.unwrap(), Polled::Read { caught_up: false });
    }
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    assert_eq!(node.read(|e| e.custody_checkpoint().unwrap().block).await, 2);
    let now = node.read(|e| e.now()).await;
    node.submit(Command::Prices {
        market: 1,
        quotes: vec![
            SourceQuote { source: "a".into(), mid: usdc("100"), ts: now },
            SourceQuote { source: "b".into(), mid: usdc("100"), ts: now },
        ],
    })
    .await;
    let fills = node
        .submit(Command::PlaceOrder(OrderRequest {
            account: TRADER,
            client_id: 1,
            market: 1,
            side: Side::Buy,
            qty: usdc("1"),
            kind: OrderKind::Market { quoted: usdc("100"), max_slippage_bps: None },
            reduce_only: false,
            auth: None,
        }))
        .await;
    assert!(fills.iter().any(|event| matches!(event, Event::Fill(fill) if fill.qty == usdc("1"))));
    assert_eq!(
        node.submit(withdraw("1")).await,
        vec![Event::WithdrawalRejected {
            account: TRADER,
            amount: usdc("1"),
            reason: Reject::InsufficientFunds { available: usdc("0") },
        }]
    );
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    node.stop().await;
}

#[tokio::test]
async fn finality_advances_past_a_cached_empty_head_without_blocking_later_confirmations() {
    let dir = scratch("confirmed-empty-finality-advance");
    let node = Node::start_confirmed(&dir);
    let chains = chains(2, Some(2));
    let mut scan = watcher(&chains, &dir, 4, None);
    scan.poll(&node.tx).await.unwrap();
    set_head(&chains, 12);
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.read(|e| e.custody_checkpoint().unwrap().block).await, 2);
    set_release(&chains, Some(8));
    let reads = chains[0].reads().len();
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.read(|e| e.custody_checkpoint().unwrap().block).await, 8);
    assert_eq!(chains[0].reads().len(), reads, "finality advances from the cached scan without rereading logs");
    add(&chains, &confirmed_deposit(0, 13, 100));
    set_head(&chains, 14);
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("100")), usdc("100"))
    );
    assert_eq!(node.read(|e| e.custody_checkpoint().unwrap().block).await, 8);
    node.stop().await;
}

#[tokio::test]
async fn a_reorg_of_scanned_empty_windows_cannot_hide_a_new_receipt_or_its_replacement() {
    let dir = scratch("confirmed-empty-reorg");
    let (logs, _guard) = Logs::capture();
    let node = Node::start_confirmed(&dir);
    let chains = chains(2, Some(2));
    let mut scan = watcher(&chains, &dir, 4, None);
    scan.poll(&node.tx).await.unwrap();
    set_head(&chains, 12);
    catch_up(&mut scan, &node.tx).await;
    let reads = chains[0].reads().len();
    assert_eq!(scan.poll(&node.tx).await.unwrap(), Polled::Read { caught_up: true });
    assert_eq!(chains[0].reads().len(), reads, "an unchanged fully scanned head does not spin through logs");
    for chain in &chains {
        fork(chain, 7, 12);
    }
    let original = VaultDeposit { block_hash: fork_hash(9), ..confirmed_deposit(0, 9, 100) };
    add(&chains, &original);
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")), "the old empty-window scan must be invalidated");
    let alternative_hash = |height| B256::from(U256::from(0xe000u64 + height));
    let replacement = VaultDeposit { block: 11, block_hash: alternative_hash(11), log_index: 87, ..original.clone() };
    for chain in &chains {
        chain.remove(&original);
        let mut state = chain.0.lock().unwrap();
        state.forks.extend((7..=12).map(|height| (height, alternative_hash(height))));
    }
    add(&chains, &replacement);
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("100")), usdc("100"))
    );
    assert_eq!(logs.lines_with("alert=\"deposit_reversed\"").len(), 1);
    set_release(&chains, Some(12));
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("0"));
    assert_eq!(
        node.submit(withdraw("100")).await,
        vec![Event::WithdrawalApproved { account: TRADER, amount: usdc("100"), nonce: 1 }]
    );
    assert_eq!(logs.lines_with("alert=\"deposit_reversed\"").len(), 1);
    node.stop().await;
}

#[tokio::test]
async fn failed_ranges_and_restart_cannot_advance_an_empty_scan_past_an_uncredited_deposit() {
    let dir = scratch("confirmed-empty-restart");
    let node = Node::start_confirmed(&dir);
    let chains = chains(2, Some(2));
    let mut scan = watcher(&chains, &dir, 4, None);
    scan.poll(&node.tx).await.unwrap();
    set_head(&chains, 12);
    add(&chains, &confirmed_deposit(0, 9, 100));
    scan.poll(&node.tx).await.unwrap();
    for chain in &chains[..2] {
        chain.0.lock().unwrap().hide_logs = true;
    }
    assert!(matches!(poll_until_error(&mut scan, &node.tx).await, PollError::Stalled(Stall::Unsettled(_))));
    assert_eq!(node.balance(TRADER).await, None);
    for chain in &chains[..2] {
        let mut state = chain.0.lock().unwrap();
        state.hide_logs = false;
        state.failing = true;
    }
    assert!(matches!(scan.poll(&node.tx).await, Err(PollError::Stalled(Stall::Rpc(_)))));
    assert_eq!(node.balance(TRADER).await, None);
    for chain in &chains[..2] {
        chain.0.lock().unwrap().failing = false;
    }
    node.stop().await;
    let node = Node::start_confirmed(&dir);
    Cursor {
        chain_id: CHAIN_ID,
        vault: VAULT,
        next_block: 10000,
        next_deposit_id: 1000,
        log_seq: node.last_seq().await,
        last_block_hash: Some(B256::repeat_byte(99)),
        crediting: None,
    }
    .store(&dir.join("cursor.json"))
    .unwrap();
    let mut scan = watcher(&chains, &dir, 4, node.last_seq().await);
    let before = chains[0].reads().len();
    assert_eq!(scan.poll(&node.tx).await.unwrap(), Polled::Read { caught_up: false });
    assert_eq!(node.balance(TRADER).await, None, "restart must re-prove the first empty window");
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(chains[0].reads()[before].0, 3, "the rescan starts at the sequenced frontier, not the host cursor");
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    assert_eq!(node.read(|e| e.custody_checkpoint().unwrap().block).await, 2);
    set_release(&chains, Some(12));
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("100")), usdc("0"))
    );
    node.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_confirmed_header_proof_that_holds_back_every_read_raises_the_lag_alert() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("confirmed-long-proof");
    let node = Node::start_confirmed(&dir);
    // Ten headers per poll at 100ms each: proving 2,000 blocks takes far longer than the 1s budget.
    let chains = chains(2_000, None);
    for chain in &chains {
        chain.0.lock().unwrap().header_latency = Some(Duration::from_millis(100));
    }
    let run = tokio::spawn(watcher(&chains, &dir, 10, None).run(node.tx.clone()));
    sleep(Duration::from_millis(1_500)).await;
    assert!(chains[0].reads().is_empty(), "no deposit is read while the chain is proven");
    assert_eq!(logs.count("alert=\"custody_deposits_lagging\""), 1);
    run.abort();
}
