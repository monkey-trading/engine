//! The staking mirror's range read against the blocks its events are in: each such block's header is
//! read through the quorum and each block read again by its hash, so an event off its canonical block, a
//! range answer that drops one of a block's events, and log backends that refuse the reads by hash all
//! stall; and a poll reads a fixed number of headers however many blocks it adds.

use super::*;
use crate::custody::stall::Polled;

#[tokio::test]
async fn a_majority_range_read_that_drops_one_of_a_blocks_events_stalls_until_the_reads_agree() {
    let dir = scratch("staking-event-omitted");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([0x51; 20]);
    let chains = poll_chains(10);
    let (kept, dropped) = (staked(2, 0x51, account, 500), staked(2, 0x52, account, 250));
    for (index, chain) in chains.iter().enumerate() {
        let mut state = chain.0.lock().unwrap();
        state.staking.extend([kept.clone(), StakingEvent { log_index: 1, ..dropped.clone() }]);
        // Two of the three log backends answer the range without the block's second event.
        if index < 2 {
            state.omit_ranged_staking.insert(dropped.tx_hash);
        }
    }
    let mut watcher = staking_watcher(&chains, &dir);
    let error = watcher.poll(&node.tx).await.unwrap_err();
    assert!(matches!(error, PollError::Stalled(Stall::Unsettled(_))), "{error:?}");
    assert!(node.read(|engine| engine.staked_balances().is_empty()).await);
    assert!(node.read(|engine| engine.staking().unwrap().completed_through.is_none()).await, "progress passed it");
    for chain in &chains {
        chain.0.lock().unwrap().omit_ranged_staking.clear();
    }
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(750));
    node.stop().await;
}

#[tokio::test]
async fn a_majority_event_off_its_canonical_block_stalls_the_poll() {
    let dir = scratch("staking-event-off-canonical");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([0x53; 20]);
    let chains = poll_chains(10);
    for chain in &chains {
        // Every log backend answers from another fork's block 2, whose header the quorum does not hold.
        let event = StakingEvent { block_hash: B256::repeat_byte(0xe2), ..staked(2, 0x53, account, 500) };
        chain.0.lock().unwrap().staking.push(event);
    }
    let mut watcher = staking_watcher(&chains, &dir);
    let error = watcher.poll(&node.tx).await.unwrap_err();
    assert!(matches!(error, PollError::Stalled(Stall::Unsettled(_))), "{error:?}");
    assert!(format!("{error:?}").contains("not the canonical block there"), "{error:?}");
    assert!(node.read(|engine| engine.staked_balances().is_empty()).await);
    assert!(node.read(|engine| engine.staking().unwrap().completed_through.is_none()).await, "progress passed it");
    node.stop().await;
}

#[tokio::test]
async fn an_event_block_whose_reads_by_hash_are_refused_holds_staking_progress_and_narrows_the_window() {
    let dir = scratch("staking-pinned-refused");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([0x52; 20]);
    let chains = poll_chains(10);
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        state.staking.push(staked(2, 0x52, account, 500));
        // Every log backend refuses the block by its hash, as one that does not hold it does.
        state.refuse_pinned_staking = true;
    }
    let mut watcher = staking_watcher(&chains, &dir);
    let error = watcher.poll(&node.tx).await.unwrap_err();
    assert!(matches!(error, PollError::Stalled(Stall::Rpc(_))), "{error:?}");
    assert_eq!(watcher.staking_range_for_test(), 50, "the refused reads narrow the next window");
    assert!(node.read(|engine| engine.staked_balances().is_empty()).await);
    assert!(node.read(|engine| engine.staking().unwrap().completed_through.is_none()).await, "progress passed it");
    node.stop().await;
}

#[tokio::test]
async fn a_poll_reads_a_fixed_number_of_headers_however_many_blocks_it_adds() {
    // The finalized tag's lag behind the head, as on Arbitrum One, scaled down.
    const LAG: u64 = 200;
    for new in [20u64, 60, 600] {
        let dir = scratch(&format!("staking-fixed-headers-{new}"));
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
            advance(new);
            caught_up(&mut watcher).await;
        }
        for chain in &chains {
            chain.0.lock().unwrap().header_reads.clear();
        }
        advance(new);
        caught_up(&mut watcher).await;
        let released = chains[0].0.lock().unwrap().withdrawal_block.unwrap();
        let completed = node.read(|engine| engine.staking().unwrap().completed_through.map(|p| p.block)).await;
        assert_eq!(completed, Some(released), "staking follows the release");
        for chain in &chains {
            let headers: u64 = chain.0.lock().unwrap().header_reads.iter().map(|(from, to)| to - from + 1).sum();
            // The two finality votes' headers, the frontier, the boundary, the deposit window's end, and the
            // staking frontier, its window's end and the range read's binding to it.
            assert!(headers <= 8, "the poll read {headers} headers for {new} new blocks");
        }
        node.stop().await;
    }
}

#[tokio::test]
async fn a_changed_staking_frontier_block_is_refused_as_inconsistent() {
    let dir = scratch("staking-frontier-changed");
    let node = Node::start_confirmed(&dir);
    let chains = poll_chains(64);
    let mut watcher = staking_watcher_with_range(&chains, &dir, 16);
    for _ in 0..8 {
        watcher.poll(&node.tx).await.unwrap();
    }
    assert_eq!(node.read(|engine| engine.custody_checkpoint().unwrap().block).await, 64);
    // Staking starts behind the deposit frontier, so its first window ends below it.
    init_staking(&node).await;
    watcher.poll(&node.tx).await.unwrap();
    let frontier = node.read(|engine| engine.staking().unwrap().completed_through.unwrap()).await;
    assert!(frontier.block < 64, "{frontier:?}");
    for chain in &chains {
        chain.0.lock().unwrap().forks.insert(frontier.block, B256::repeat_byte(0x77));
    }
    let error = watcher.poll(&node.tx).await.unwrap_err();
    assert!(
        matches!(&error, PollError::Stalled(Stall::Inconsistent(cause)) if cause.to_string().contains("staking anchor changed")),
        "{error:?}"
    );
    assert_eq!(node.read(|engine| engine.staking().unwrap().completed_through).await, Some(frontier));
    node.stop().await;
}

#[tokio::test]
async fn a_staking_range_read_bound_to_another_release_block_is_refused() {
    let chains = poll_chains(64);
    let quorum =
        Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    quorum.identity().await.unwrap();
    let to = BlockRef { number: 16, hash: canonical_hash(16) };
    let release = BlockRef { number: 64, hash: canonical_hash(64) };
    assert!(quorum.released_staking_events(0, to, Some(release)).await.is_ok());
    // Every provider's header at the release block is on another chain than the tag the quorum voted.
    let elsewhere = BlockRef { hash: B256::repeat_byte(0x64), ..release };
    let error = quorum.released_staking_events(0, to, Some(elsewhere)).await.unwrap_err();
    assert!(format!("{error:#}").contains("release block differs"), "{error:#}");
    // A release block at the range's end binds nothing more than the range's own end does.
    assert!(
        quorum.released_staking_events(0, to, Some(BlockRef { hash: B256::repeat_byte(0x16), ..to })).await.is_ok()
    );
}
