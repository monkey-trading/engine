//! The staking mirror's range read against its proven headers: a range answer that drops an event a
//! header's bloom admits stalls, as does a read by hash that the log backends refuse, and a poll proves
//! its finalized windows from the hashes its confirmation proof already linked, reading each new header
//! once.

use super::*;
use crate::custody::stall::Polled;

#[tokio::test]
async fn a_majority_range_read_that_drops_an_admitted_event_stalls_until_the_reads_agree() {
    let dir = scratch("staking-bloom-omitted");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([0x51; 20]);
    let chains = poll_chains(10);
    for (index, chain) in chains.iter().enumerate() {
        let mut state = chain.0.lock().unwrap();
        state.staking.push(staked(2, 0x51, account, 500));
        // Two of the three log backends answer the range from a fork without the event.
        state.omit_ranged_staking = index < 2;
    }
    let mut watcher = staking_watcher(&chains, &dir);
    let error = watcher.poll(&node.tx).await.unwrap_err();
    assert!(matches!(error, PollError::Stalled(Stall::Unsettled(_))), "{error:?}");
    assert!(node.read(|engine| engine.staked_balances().is_empty()).await);
    assert!(node.read(|engine| engine.staking().unwrap().completed_through.is_none()).await, "progress passed it");
    for chain in &chains {
        chain.0.lock().unwrap().omit_ranged_staking = false;
    }
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(500));
    node.stop().await;
}

#[tokio::test]
async fn an_admitted_block_whose_reads_by_hash_are_refused_holds_staking_progress() {
    let dir = scratch("staking-bloom-refused");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([0x52; 20]);
    let chains = poll_chains(10);
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        state.staking.push(staked(2, 0x52, account, 500));
        // Every log backend lags: the range reads empty and the reads by hash refuse.
        state.hide_logs = true;
    }
    let error = staking_watcher(&chains, &dir).poll(&node.tx).await.unwrap_err();
    assert!(matches!(error, PollError::Stalled(Stall::Rpc(_))), "{error:?}");
    assert!(node.read(|engine| engine.staked_balances().is_empty()).await);
    assert!(node.read(|engine| engine.staking().unwrap().completed_through.is_none()).await, "progress passed it");
    node.stop().await;
}

#[tokio::test]
async fn a_poll_reads_each_new_header_once_and_proves_its_finalized_windows_from_memory() {
    // The finalized tag's lag behind the head, and the blocks a poll adds, as on Arbitrum One at 5 s.
    const LAG: u64 = 200;
    const NEW: u64 = 20;
    let dir = scratch("staking-recent-windows");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let chains = poll_chains(1_000);
    let advance = |blocks: u64| {
        for chain in &chains {
            let mut state = chain.0.lock().unwrap();
            state.head += blocks;
            state.final_block = Some(state.head);
            state.withdrawal_block = Some(state.head - LAG);
        }
    };
    let caught_up = async |watcher: &mut DepositWatcher<Quorum<FakeChain>>| {
        for _ in 0..20 {
            if matches!(watcher.poll(&node.tx).await.unwrap(), Polled::Read { caught_up: true }) {
                return;
            }
        }
        panic!("the watcher did not catch up");
    };
    let mut watcher = staking_watcher_with_range(&chains, &dir, 1_000);
    for _ in 0..3 {
        advance(NEW);
        caught_up(&mut watcher).await;
    }
    for chain in &chains {
        chain.0.lock().unwrap().header_reads.clear();
    }
    advance(NEW);
    caught_up(&mut watcher).await;
    let released = chains[0].0.lock().unwrap().withdrawal_block.unwrap();
    let completed = node.read(|engine| engine.staking().unwrap().completed_through.map(|p| p.block)).await;
    assert_eq!(completed, Some(released), "staking follows the release");
    for chain in &chains {
        let headers: u64 = chain.0.lock().unwrap().header_reads.iter().map(|(from, to)| to - from + 1).sum();
        // The new blocks, then single headers: the two finality votes, the frontier and the staking
        // anchor, both reads' endpoints and the staking progress's time.
        assert!(headers <= NEW + 8, "the poll read {headers} headers for {NEW} new blocks");
    }
    node.stop().await;
}
