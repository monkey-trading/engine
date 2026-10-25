//! A pending receipt under frozen withdrawal finality: completed confirmed windows are kept, so
//! later deposits are credited after reading each block once per canonical proof.

use super::*;
use crate::custody::stall::{PollError, Stall};

#[tokio::test]
async fn a_pending_receipt_under_frozen_finality_reads_each_window_once_across_quota_failures_and_lost_replies() {
    let dir = scratch("confirmed-frozen-progress");
    let node = Node::start_confirmed(&dir);
    let chains = chains(4, Some(2));
    add(&chains, &confirmed_deposit(0, 3, 100));
    let mut scan = watcher(&chains, &dir, 4, None);
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    let before = chains[0].reads().len();
    set_head(&chains, 44);
    add(&chains, &confirmed_deposit(1, 21, 100));
    add(&chains, &confirmed_deposit(2, 41, 100));
    // The relay loses the reply to the first new credit after the sequencer applies it.
    let relay = lose_reply_after(&node.tx, Cutoff::Credit);
    poll_until_error(&mut scan, &relay).await;
    assert_eq!(node.balance(TRADER).await, Some(usdc("200")));
    // A provider result cap fails the next log window; the scan keeps every window before it.
    for chain in &chains {
        chain.0.lock().unwrap().log_range_cap = Some(2);
    }
    assert!(matches!(poll_until_error(&mut scan, &node.tx).await, PollError::Stalled(Stall::Rpc(_))));
    scan.poll(&node.tx).await.unwrap();
    for chain in &chains {
        chain.0.lock().unwrap().log_range_cap = None;
    }
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("300")), usdc("300"))
    );
    assert_eq!(node.read(|e| e.custody_checkpoint().unwrap().block).await, 2, "finality stays frozen");
    assert_each_block_read_once(&chains[0].reads()[before..], 5, 44);
    node.stop().await;
    assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Credit { .. })), 3);
}

#[tokio::test]
async fn reorg_and_restart_under_frozen_finality_rescan_from_the_frontier_once_and_move_a_reincluded_deposit() {
    let dir = scratch("confirmed-frozen-reorg");
    let (logs, _guard) = Logs::capture();
    let node = Node::start_confirmed(&dir);
    let chains = chains(4, Some(2));
    add(&chains, &confirmed_deposit(0, 3, 100));
    let mut scan = watcher(&chains, &dir, 4, None);
    catch_up(&mut scan, &node.tx).await;
    set_head(&chains, 44);
    let moved = confirmed_deposit(2, 41, 100);
    add(&chains, &confirmed_deposit(1, 21, 100));
    add(&chains, &moved);
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("300"));
    // The reorg replaces blocks 30..=46 and re-includes the last deposit ten blocks earlier, in a
    // window the rescan reaches before it covers the credited receipt's block.
    let replacement = VaultDeposit { block: 31, block_hash: fork_hash(31), log_index: 9, ..moved.clone() };
    for chain in &chains {
        chain.remove(&moved);
        chain.add(replacement.clone());
        fork(chain, 30, 46);
    }
    set_head(&chains, 46);
    let before = chains[0].reads().len();
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("300")), usdc("300"))
    );
    assert_each_block_read_once(&chains[0].reads()[before..], 3, 46);
    assert_eq!(logs.lines_with("alert=\"deposit_reversed\"").len(), 1);
    node.stop().await;
    let node = Node::start_confirmed(&dir);
    let mut scan = watcher(&chains, &dir, 4, node.last_seq().await);
    let before = chains[0].reads().len();
    catch_up(&mut scan, &node.tx).await;
    assert_each_block_read_once(&chains[0].reads()[before..], 3, 46);
    set_release(&chains, Some(46));
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(
        (node.balance(TRADER).await, node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await),
        (Some(usdc("300")), usdc("0"))
    );
    assert_eq!(logs.lines_with("alert=\"deposit_reversed\"").len(), 1);
    assert_eq!(logs.count("alert_resolved"), 0, "credits, a replacement and finality resolve no alert");
    node.stop().await;
    assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Credit { .. })), 3);
    assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Reverse { .. })), 0);
    assert_eq!(transitions(&dir, |update| matches!(update, DepositUpdate::Replace { .. })), 1);
    let order: Vec<_> = crate::test_support::log_entries(&dir.join("log"))
        .into_iter()
        .filter_map(|entry| match entry {
            crate::log::LogEntry::Command(command) => match command.sequenced.cmd {
                Command::CustodyDeposit(update) if update.subject().tx_hash == moved.tx_hash => Some(*update),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert!(
        matches!(order.as_slice(), [DepositUpdate::Credit { .. }, DepositUpdate::Replace { .. }, ..]),
        "the reincluded deposit moves to its replacement in one step: {order:?}"
    );
}

// ── Helpers ──

/// Asserts the log reads cover blocks `from..=to` in order, each block exactly once.
fn assert_each_block_read_once(reads: &[(u64, u64)], from: u64, to: u64) {
    let next = reads.iter().fold(from, |next, &(start, end)| {
        assert_eq!(start, next, "a block was skipped or read again: {reads:?}");
        end + 1
    });
    assert_eq!(next, to + 1, "{reads:?}");
}
