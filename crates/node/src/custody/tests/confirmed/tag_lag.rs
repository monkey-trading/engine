//! A lagging quorum tag: the poll waits out the tag when it drops below the proven scan end or
//! an already credited receipt, instead of re-polling at once and arming the lag watchdog.

use super::*;
use crate::custody::stall::Polled;

#[tokio::test]
async fn a_lagging_tag_below_the_proven_scan_end_waits_instead_of_repolling_at_once() {
    let dir = scratch("confirmed-tag-drop");
    let node = Node::start_confirmed(&dir);
    assert!(node.read(|e| e.deposit_finality_enabled()).await);
    let (logs, _guard) = Logs::capture();
    let chains = chains(10, None);
    add(&chains, &confirmed_deposit(0, 5, 100));
    let mut watcher = watcher(&chains, &dir, 100, None);
    catch_up(&mut watcher, &node.tx).await;
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    // A lagging provider pulls the agreed tag back below the scan end the watcher proved.
    for chain in &chains {
        chain.set_final(Some(6));
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::resume();
    assert_eq!(logs.count("alert=\"custody_deposits_lagging\""), 0, "an ahead scan is not a lag");
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    node.stop().await;
}

#[tokio::test]
async fn a_pending_receipt_above_a_dropped_tag_waits_instead_of_lagging() {
    let dir = scratch("confirmed-pending-above-tag");
    let node = Node::start_confirmed(&dir);
    assert!(node.read(|e| e.deposit_finality_enabled()).await);
    let (logs, _guard) = Logs::capture();
    let chains = chains(12, None);
    add(&chains, &confirmed_deposit(0, 10, 100));
    let mut scan = watcher(&chains, &dir, 100, None);
    catch_up(&mut scan, &node.tx).await;
    // Finalise a frontier below the pending receipt, as the release tag trails; it stays
    // recorded and unfinalised above the sequenced checkpoint.
    set_release(&chains, Some(5));
    scan.poll(&node.tx).await.unwrap();
    // A restart replays the sequenced frontier, and the tag then drops below the receipt.
    let mut restarted = watcher(&chains, &dir, 100, node.last_seq().await);
    for chain in &chains {
        chain.set_final(Some(7));
    }
    assert!(matches!(restarted.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    assert!(matches!(restarted.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::resume();
    assert_eq!(logs.count("alert=\"custody_deposits_lagging\""), 0, "a receipt above the tag is not a lag");
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    node.stop().await;
}
