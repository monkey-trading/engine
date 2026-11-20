//! The scan's one reorganisation check a poll: the boundary's header read again under the quorum, and on a
//! mismatch a walk back through the stored window ends to the highest that still matches; and the final
//! frontier's blocks and count read again before principal is released.

use engine_core::deposits::{DepositStatus, RecordedDeposit};

use super::*;

/// A scan with windows of four blocks from the finalised frontier at block 2 to block 14, its ends at
/// blocks 6, 10 and 14, holding `deposits`.
async fn scanned(
    dir: &Path,
    node: &Node,
    deposits: &[VaultDeposit],
) -> ([FakeChain; 3], DepositWatcher<Quorum<FakeChain>>) {
    let chains = chains(2, Some(2));
    let mut scan = watcher(&chains, dir, 4, None);
    scan.poll(&node.tx).await.unwrap();
    for deposit in deposits {
        add(&chains, deposit);
    }
    set_head(&chains, 14);
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.read(|e| e.custody_checkpoint().unwrap().block).await, 2);
    (chains, scan)
}

/// The block ranges each chain's deposit reads asked for since `from`, as `(first, last)`.
fn reads_since(chain: &FakeChain, from: usize) -> Vec<(u64, u64)> {
    chain.reads()[from..].to_vec()
}

#[tokio::test]
async fn a_reorganisation_inside_a_range_with_no_events_is_read_again_from_the_highest_end_that_matches() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("boundary-empty-range");
    let node = Node::start_confirmed(&dir);
    let (chains, mut scan) = scanned(&dir, &node, &[confirmed_deposit(0, 4, 100)]).await;
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    let before = chains[0].reads().len();
    for chain in &chains {
        fork(chain, 12, 14);
    }
    assert_eq!(scan.poll(&node.tx).await.unwrap(), Polled::Read { caught_up: false }, "the boundary moved");
    assert!(reads_since(&chains[0], before).is_empty(), "the poll that finds the reorganisation reads no window");
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(reads_since(&chains[0], before), [(11, 14)], "only the range above the last matching end is read");
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    assert_eq!(logs.count("alert=\"deposit_reversed\""), 0);
    // The new boundary holds, so the next poll reads nothing again.
    let after = chains[0].reads().len();
    assert_eq!(scan.poll(&node.tx).await.unwrap(), Polled::Read { caught_up: true });
    assert_eq!(chains[0].reads().len(), after);
    node.stop().await;
}

#[tokio::test]
async fn a_reorganisation_that_drops_an_event_block_reverses_its_deposit() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("boundary-dropped-event");
    let node = Node::start_confirmed(&dir);
    let dropped = confirmed_deposit(0, 9, 100);
    let (chains, mut scan) = scanned(&dir, &node, std::slice::from_ref(&dropped)).await;
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    for chain in &chains {
        chain.remove(&dropped);
        fork(chain, 8, 14);
    }
    scan.poll(&node.tx).await.unwrap();
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(node.balance(TRADER).await, Some(usdc("0")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("0"));
    assert_eq!(logs.lines_with("alert=\"deposit_reversed\"").len(), 1);
    node.stop().await;
}

#[tokio::test]
async fn a_reorganisation_deeper_than_every_stored_end_is_read_again_from_the_finalised_frontier() {
    let dir = scratch("boundary-deeper");
    let node = Node::start_confirmed(&dir);
    let moved = confirmed_deposit(0, 4, 100);
    let (chains, mut scan) = scanned(&dir, &node, std::slice::from_ref(&moved)).await;
    let before = chains[0].reads().len();
    // Below the lowest stored end, block 6, and above the finalised frontier, block 2.
    let replacement = VaultDeposit { block: 5, block_hash: fork_hash(5), log_index: 9, ..moved.clone() };
    for chain in &chains {
        chain.remove(&moved);
        chain.add(replacement.clone());
        fork(chain, 3, 14);
    }
    scan.poll(&node.tx).await.unwrap();
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(reads_since(&chains[0], before)[0], (3, 6), "the rescan starts above the finalised frontier");
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")), "the moved deposit stays credited");
    let (block, hash, tx, index) =
        (replacement.block, replacement.block_hash, replacement.tx_hash, replacement.log_index);
    let reference = format!("{block:020}:{hash}:{tx}:{index}");
    assert!(node.read(move |e| e.recorded_deposit(&reference).is_some()).await, "under its new receipt");
    // A reorganisation that reaches the finalised frontier itself is refused as inconsistent.
    for chain in &chains {
        fork(chain, 2, 14);
    }
    assert!(matches!(scan.poll(&node.tx).await, Err(PollError::Stalled(Stall::Inconsistent(_)))));
    node.stop().await;
}

#[tokio::test]
async fn a_provider_that_lies_about_the_boundary_header_but_not_the_range_is_outvoted() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("boundary-lying-provider");
    let node = Node::start_confirmed(&dir);
    let (chains, mut scan) = scanned(&dir, &node, &[confirmed_deposit(0, 9, 100)]).await;
    let before = chains[0].reads().len();
    // One provider reports another hash for the boundary block only; its logs and other headers are true.
    chains[0].0.lock().unwrap().forks.insert(14, B256::repeat_byte(0x14));
    for _ in 0..3 {
        assert_eq!(scan.poll(&node.tx).await.unwrap(), Polled::Read { caught_up: true });
    }
    assert!(reads_since(&chains[0], before).is_empty(), "the majority's boundary holds, so nothing is read again");
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    assert_eq!(node.read(|e| e.accounts()[&TRADER].unfinalised_deposits).await, usdc("100"));
    assert!(!logs.lines_with("alert=\"custody_rpc_provider_disagreed\"").is_empty(), "the dissent pages");
    node.stop().await;
}

#[tokio::test]
async fn a_receipt_whose_block_is_not_the_final_one_is_scanned_again_and_never_released() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("boundary-final-receipt");
    let node = Node::start_confirmed(&dir);
    let credited = confirmed_deposit(0, 9, 100);
    let (chains, mut scan) = scanned(&dir, &node, std::slice::from_ref(&credited)).await;
    // Block 9 changed under a boundary that still matches, as when a poll's boundary and window reads
    // straddle a reorganisation that is then undone: only the final read of block 9 can see it.
    let moved = VaultDeposit { block_hash: fork_hash(9), ..credited.clone() };
    for chain in &chains {
        chain.remove(&credited);
        chain.add(moved.clone());
        chain.0.lock().unwrap().forks.insert(9, fork_hash(9));
    }
    set_release(&chains, Some(14));
    scan.poll(&node.tx).await.unwrap();
    let stale = format!("{:020}:{}:{}:{}", 9, credited.block_hash, credited.tx_hash, credited.log_index);
    let finalised = |d: &RecordedDeposit| d.status == DepositStatus::Finalised;
    assert!(
        !node.read(move |e| e.recorded_deposit(&stale).is_some_and(finalised)).await,
        "the stale receipt is never released"
    );
    assert_eq!(node.read(|e| e.custody_checkpoint().unwrap().block).await, 2, "the frontier waits");
    catch_up(&mut scan, &node.tx).await;
    scan.poll(&node.tx).await.unwrap();
    let current = format!("{:020}:{}:{}:{}", 9, moved.block_hash, moved.tx_hash, moved.log_index);
    assert!(
        node.read(move |e| e.recorded_deposit(&current).is_some_and(finalised)).await,
        "the receipt on the final block is released"
    );
    assert_eq!(node.read(|e| e.custody_checkpoint().unwrap().block).await, 14);
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    assert_eq!(logs.lines_with("alert=\"deposit_reversed\"").len(), 1, "the moved receipt pages as any reorganisation");
    node.stop().await;
}

#[tokio::test]
async fn a_deposit_the_scan_missed_is_found_by_the_count_at_the_final_frontier() {
    let dir = scratch("boundary-final-count");
    let node = Node::start_confirmed(&dir);
    let (chains, mut scan) = scanned(&dir, &node, &[]).await;
    // A deposit below the boundary that the scan's window read did not return.
    add(&chains, &confirmed_deposit(0, 9, 100));
    // The release tag stops inside the last window, so its count is read again there.
    set_release(&chains, Some(12));
    scan.poll(&node.tx).await.unwrap();
    assert_eq!(node.read(|e| e.custody_checkpoint().unwrap().block).await, 2, "the frontier waits");
    catch_up(&mut scan, &node.tx).await;
    scan.poll(&node.tx).await.unwrap();
    assert_eq!(node.balance(TRADER).await, Some(usdc("100")));
    assert_eq!(node.read(|e| e.custody_checkpoint().map(|p| (p.block, p.deposit_count))).await, Some((12, 1)));
    node.stop().await;
}

#[tokio::test]
async fn a_walk_back_that_a_failed_read_stops_scans_again_from_the_finalised_frontier() {
    let dir = scratch("boundary-failed-walk");
    let node = Node::start_confirmed(&dir);
    let (chains, mut scan) = scanned(&dir, &node, &[]).await;
    set_head(&chains, 18);
    for chain in &chains {
        fork(chain, 12, 18);
    }
    // A poll's header reads, on each chain: the two finality votes, the frontier, the window's last block
    // and the boundary; the walk back's first read comes next, and two providers fail it.
    for chain in &chains[..2] {
        let mut state = chain.0.lock().unwrap();
        let walk = state.header_calls + 5;
        state.fail_header_reads.push(walk);
    }
    let before = chains[2].reads().len();
    assert!(matches!(scan.poll(&node.tx).await, Err(PollError::Stalled(Stall::Rpc(_)))));
    catch_up(&mut scan, &node.tx).await;
    assert_eq!(reads_since(&chains[2], before)[1], (3, 6), "the mismatch seen is not forgotten");
    node.stop().await;
}

#[tokio::test]
async fn a_provider_answering_its_window_and_boundary_from_two_branches_joins_no_majority() {
    let dir = scratch("boundary-mixed-answer");
    let node = Node::start_confirmed(&dir);
    let (chains, mut scan) = scanned(&dir, &node, &[]).await;
    set_head(&chains, 18);
    // The chain reorganised from block 12. One provider has it all; one reads the window's blocks from the
    // new branch but its boundary, block 14, from a backend still on the old; one has not seen it.
    fork(&chains[0], 12, 18);
    fork(&chains[1], 15, 18);
    let before = chains[0].reads().len();
    assert!(scan.poll(&node.tx).await.is_err(), "no two answers agree on the window and its boundary");
    assert_eq!(scan.chain.top().map(|(number, _)| number), Some(14), "the window does not join the scan");
    for chain in &chains[1..] {
        fork(chain, 12, 18);
    }
    catch_up(&mut scan, &node.tx).await;
    let reads = reads_since(&chains[0], before);
    // The failed poll's window, the window whose answer shows the boundary moved, then the rescan; the
    // failed read narrowed the windows that follow.
    assert_eq!(
        (reads[0], reads[1].0, reads[2].0),
        ((15, 18), 15, 11),
        "the scan reads again above the last end that matches"
    );
    node.stop().await;
}

#[tokio::test]
async fn a_receipt_the_vault_records_otherwise_at_the_release_block_is_never_released() {
    let dir = scratch("boundary-release-record");
    let node = Node::start_confirmed(&dir);
    let credited = confirmed_deposit(0, 9, 100);
    let (chains, mut scan) = scanned(&dir, &node, std::slice::from_ref(&credited)).await;
    // The release tag's chain holds deposit 0 for the house, on a block whose header still matches: only
    // the vault's record, read in the release tag's block, shows it.
    let housed = VaultDeposit { beneficiary: Beneficiary::House, ..credited.clone() };
    for chain in &chains {
        chain.remove(&credited);
        chain.add(housed.clone());
    }
    set_release(&chains, Some(14));
    assert_eq!(scan.poll(&node.tx).await.unwrap(), Polled::Read { caught_up: false });
    assert_eq!(node.read(|e| e.custody_checkpoint().unwrap().block).await, 2, "the frontier waits");
    let reference = format!("{:020}:{}:{}:{}", 9, credited.block_hash, credited.tx_hash, credited.log_index);
    let finalised = node.read(move |e| e.recorded_deposit(&reference).map(|d| d.status == DepositStatus::Finalised));
    assert_eq!(finalised.await, Some(false), "the credited receipt is not released");
    node.stop().await;
}
