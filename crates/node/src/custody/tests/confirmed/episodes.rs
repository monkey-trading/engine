//! Deposit alert episodes: a reversal resolves only when its exact receipt is reinstated, directly or
//! as the replacement a reorg back returns to.

use super::*;

#[tokio::test]
async fn only_reinstating_the_reversed_receipt_resolves_its_reversal_alert() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("confirmed-episodes-reinstate");
    let node = Node::start_confirmed(&dir);
    let chains = chains(5, None);
    let original = confirmed_deposit(0, 3, 100);
    add(&chains, &original);
    let mut scan = watcher(&chains, &dir, 10, None);
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    assert_eq!(logs.count("alert_resolved"), 0, "an ordinary credit resolves nothing");
    for chain in &chains {
        chain.remove(&original);
        fork(chain, 3, 5);
    }
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.balance(TRADER).await, Some(usdc("0")));
    assert_eq!(logs.count("alert=\"deposit_reversed\""), 1);
    assert_eq!(logs.count("alert_resolved"), 0, "a reversal does not resolve itself");
    // The original block returns to the canonical chain, so the same receipt is reinstated.
    for chain in &chains {
        chain.0.lock().unwrap().forks.clear();
        chain.add(original.clone());
    }
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    let resolved = logs.lines_with("alert_resolved");
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert!(resolved[0].contains("alert_resolved=\"deposit_reversed\""), "{resolved:?}");
    assert!(resolved[0].contains(&original.chain_ref()), "{resolved:?}");
    node.stop().await;
}

#[tokio::test]
async fn a_reorg_back_reinstates_the_original_receipt_and_resolves_only_its_reversal() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("confirmed-episodes-reorg-back");
    let node = Node::start_confirmed(&dir);
    let chains = chains(5, None);
    let original = confirmed_deposit(0, 3, 100);
    add(&chains, &original);
    let mut scan = watcher(&chains, &dir, 10, None);
    catch_up(&mut scan, &node.tx).await;
    let moved = VaultDeposit { block: 5, block_hash: fork_hash(5), log_index: 9, ..original.clone() };
    for chain in &chains {
        chain.remove(&original);
        chain.add(moved.clone());
        fork(chain, 3, 5);
    }
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(logs.count("alert=\"deposit_reversed\""), 1);
    assert_eq!(logs.count("alert_resolved"), 0, "moving away resolves nothing");
    // The original block returns: the moved receipt is orphaned and the original reinstated.
    for chain in &chains {
        chain.remove(&moved);
        chain.add(original.clone());
        chain.0.lock().unwrap().forks.clear();
    }
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("100")), usdc("100"))
    );
    assert_eq!(logs.count("alert=\"deposit_reversed\""), 2, "the moved receipt's orphaning raises its own alert");
    let resolved = logs.lines_with("alert_resolved");
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert!(resolved[0].contains(&original.chain_ref()), "{resolved:?}");
    assert!(!resolved[0].contains(&moved.chain_ref()), "{resolved:?}");
    node.stop().await;
    assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Replace { .. })), 2);
}
