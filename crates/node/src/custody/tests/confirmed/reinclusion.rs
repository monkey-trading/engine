//! A credited deposit reincluded by a pre-finality reorg moves to its replacement in one step; an
//! orphan with no replacement waits for the eligible head, then takes the full reversal path.

use super::*;
use crate::custody::stall::Polled;

#[tokio::test]
async fn a_reincluded_deposit_moves_in_one_step_and_keeps_a_solvent_position_open() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("confirmed-reinclusion-solvent");
    let node = Node::start_confirmed(&dir);
    configure_trading(&node).await;
    let chains = chains(5, None);
    let original = confirmed_deposit(0, 3, 100);
    add(&chains, &original);
    let mut scan = watcher(&chains, &dir, 10, None);
    catch_up(&mut scan, &node.tx).await;
    price(&node, "100").await;
    let fills = node.submit(buy("6")).await;
    assert!(fills.iter().any(|event| matches!(event, Event::Fill(fill) if fill.qty == usdc("6"))), "{fills:?}");
    // Equity 64 still covers the 56.4 margin; without the deposit it would be -36.
    price(&node, "94").await;
    let moved = VaultDeposit { block: 5, block_hash: fork_hash(5), log_index: 9, ..original.clone() };
    for chain in &chains {
        chain.remove(&original);
        chain.add(moved.clone());
        fork(chain, 3, 5);
    }
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.read(|e| e.accounts()[&TRADER].positions.len()).await, 1, "a solvent position stays open");
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("100")), usdc("100"))
    );
    assert_eq!(logs.count("alert=\"deposit_reversed\""), 1, "the orphaned receipt still raises Critical");
    assert_eq!(logs.count("alert=\"bad_debt\""), 0);
    node.stop().await;
    assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Replace { .. })), 1);
    assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Reverse { .. })), 0);
}

#[tokio::test]
async fn an_orphan_waits_for_the_head_and_pairs_with_a_replacement_in_a_later_window() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("confirmed-reinclusion-later");
    let node = Node::start_confirmed(&dir);
    let chains = chains(4, None);
    let original = confirmed_deposit(0, 3, 100);
    add(&chains, &original);
    let mut scan = watcher(&chains, &dir, 4, None);
    catch_up(&mut scan, &node.tx).await;
    let moved = VaultDeposit { block: 10, block_hash: fork_hash(10), log_index: 9, ..original.clone() };
    for chain in &chains {
        chain.remove(&original);
        chain.add(moved.clone());
        fork(chain, 3, 12);
    }
    set_head(&chains, 12);
    set_release(&chains, Some(5));
    for _ in 0..40 {
        let polled = scan.poll(&node.tx).await.unwrap();
        // The orphan is the only receipt in block 3; it stays live until its replacement is found.
        if node.read(|e| e.unfinalised_deposits(3, 3).len()).await == 0 {
            break;
        }
        assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
        assert_eq!(node.read(|e| e.custody_checkpoint()).await, None, "the frontier waits behind the orphan");
        assert_ne!(polled, Polled::Read { caught_up: true });
    }
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("100")), usdc("100"))
    );
    assert_eq!(node.read(|e| e.custody_checkpoint().unwrap().block).await, 5);
    assert_eq!(logs.count("alert=\"deposit_reversed\""), 1);
    node.stop().await;
    assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Replace { .. })), 1);
    assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Reverse { .. })), 0);
    assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Credit { .. })), 1);
}

#[tokio::test]
async fn an_orphan_without_a_replacement_reverses_once_the_scan_reaches_the_head() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("confirmed-reinclusion-none");
    let node = Node::start_confirmed(&dir);
    let chains = chains(4, None);
    let original = confirmed_deposit(0, 3, 100);
    add(&chains, &original);
    let mut scan = watcher(&chains, &dir, 4, None);
    catch_up(&mut scan, &node.tx).await;
    for chain in &chains {
        chain.remove(&original);
        fork(chain, 3, 12);
    }
    set_head(&chains, 12);
    for _ in 0..40 {
        let polled = scan.poll(&node.tx).await.unwrap();
        if chains[0].reads().last().is_none_or(|(_, end)| *end != 12) {
            assert_eq!(node.balance(TRADER).await, Some(usdc("100")), "an orphan waits for the eligible head");
        }
        if polled.caught_up() {
            break;
        }
    }
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("0")), usdc("0"))
    );
    assert_eq!(logs.count("alert=\"deposit_reversed\""), 1);
    node.stop().await;
    assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Reverse { .. })), 1);
    assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Replace { .. })), 0);
}

// ── Helpers ──

/// Prices market 1 from two fresh sources at `mid`.
async fn price(node: &Node, mid: &str) {
    let now = node.read(|e| e.now()).await;
    let mid = usdc(mid);
    node.submit(Command::Prices {
        market: 1,
        quotes: ["a", "b"].map(|source| SourceQuote { source: source.into(), mid, ts: now, contract: None }).to_vec(),
    })
    .await;
}

/// A market buy of `qty` on market 1 quoted at 100.
fn buy(qty: &str) -> Command {
    Command::PlaceOrder(OrderRequest {
        account: TRADER,
        client_id: 1,
        market: 1,
        side: Side::Buy,
        qty: usdc(qty),
        kind: OrderKind::Market { quoted: usdc("100"), max_slippage_bps: None },
        reduce_only: false,
        auth: None,
    })
}
