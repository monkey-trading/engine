//! Mirroring staking events through `staking::mirror`: ordering against `InitStaking`, once-only
//! credit under the engine's references, and refusal to trust an event on an unproven block.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use alloy::primitives::{B256, U256};
use tokio::time::timeout;

use super::*;
use engine_core::command::{AdminCommand, Command};
use engine_core::emissions::TokenUnits;
use engine_core::event::Event;
use engine_core::staking::StakingConfig;
use engine_core::types::AccountId;

use crate::config::EvmCustodySection;
use crate::custody::chain::BlockLink;
use crate::custody::evm::DepositWatcher;
use crate::custody::evm::{BlockRef, Finality, StakingEvent, StakingEventKind, VaultReader};
use crate::custody::quorum::Quorum;
use crate::custody::staking::{StakingCursor, mirror, monkey_units};

/// A reader answering only `staking_events`, from a fixed event list.
struct StubReader(Vec<StakingEvent>);

impl VaultReader for StubReader {
    async fn identity(&self) -> anyhow::Result<VaultIdentity> {
        Ok(VaultIdentity { chain_id: CHAIN_ID, token: None })
    }

    async fn finality(&self) -> anyhow::Result<Finality> {
        Ok(Finality { head: 0, head_time: 0, tagged: None, head_support: None })
    }

    async fn headers(&self, from: u64, to: u64) -> anyhow::Result<Vec<BlockLink>> {
        Ok((from..=to)
            .map(|number| BlockLink {
                number,
                hash: canonical_hash(number),
                parent_hash: B256::ZERO,
                timestamp: number * 12,
            })
            .collect())
    }

    async fn deposits(&self, from: u64, to: u64) -> anyhow::Result<Vec<VaultDeposit>> {
        let _ = (from, to);
        Ok(Vec::new())
    }

    async fn deposit_count(&self, at: BlockRef) -> anyhow::Result<u64> {
        let _ = at;
        Ok(0)
    }

    async fn staking_events(&self, blocks: &[BlockRef]) -> anyhow::Result<Vec<StakingEvent>> {
        let from = blocks.first().unwrap().number;
        let to = *blocks.last().unwrap();
        anyhow::ensure!(to.hash == canonical_hash(to.number), "staking endpoint is not canonical");
        Ok(self.0.iter().filter(|e| (from..=to.number).contains(&e.block)).cloned().collect())
    }
}

fn staked(block: u64, tx: u8, account: AccountId, amount: u64) -> StakingEvent {
    event(block, tx, StakingEventKind::Staked { account, amount: u128::from(amount) })
}

fn unstaked(block: u64, tx: u8, account: AccountId, amount: u64) -> StakingEvent {
    event(block, tx, StakingEventKind::UnstakeCompleted { account, amount: u128::from(amount) })
}

fn event(block: u64, tx: u8, kind: StakingEventKind) -> StakingEvent {
    StakingEvent { block, block_hash: canonical_hash(block), tx_hash: B256::repeat_byte(tx), log_index: 0, kind }
}

fn hashes_through(to: u64) -> BTreeMap<u64, B256> {
    (0..=to).map(|number| (number, canonical_hash(number))).collect()
}

fn cursor_at(start: u64) -> StakingCursor {
    StakingCursor { chain_id: CHAIN_ID, contract: alloy::primitives::Address::ZERO, next_block: start, log_seq: None }
}

async fn init_staking(node: &Node) {
    let latest = engine_core::protocol::ProtocolVersion::LATEST;
    if node.read(move |engine| engine.protocol_version() < latest).await {
        assert_eq!(
            node.submit(Command::Admin(AdminCommand::ActivateProtocol { version: latest })).await,
            [Event::AdminApplied]
        );
    }
    // A start just past the stamps the sequencer admits, as sequenced time keeps moving.
    let start = crate::test_support::trusted_now().latest_ms + 60_000;
    let events = node
        .submit(Command::Admin(AdminCommand::InitStaking(StakingConfig {
            liquidity_floor: Fixed::ONE,
            epoch_ms: 30 * 60_000,
            start_ms: start,
            cooldown_ms: 86_400_000,
        })))
        .await;
    assert!(matches!(events[..], [Event::AdminApplied]), "{events:?}");
}

async fn mirror_range(
    node: &Node,
    reader: &StubReader,
    cursor: &mut StakingCursor,
    dir: &Path,
    from: u64,
    to: u64,
) -> anyhow::Result<(), PollError> {
    let mut range = AdaptiveSize::new(1_000);
    mirror(reader, &node.tx, cursor, &dir.join("staking-cursor.json"), &mut range, from..=to, &hashes_through(to), true)
        .await
}

#[tokio::test]
async fn mirrors_stake_and_unstake_in_chain_order_once_initialised() {
    let dir = scratch("staking-mirror");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([7; 20]);
    let events = vec![staked(1, 1, account, 500), staked(2, 2, account, 250), unstaked(3, 3, account, 750)];
    let reader = StubReader(events.clone());
    let mut cursor = cursor_at(1);
    mirror_range(&node, &reader, &mut cursor, &dir, 1, 3).await.unwrap();
    assert_eq!(cursor.next_block, 4);
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(0));
    // A lagging cursor re-reads the range; the engine's references credit nothing twice.
    let mut lagging = cursor_at(2);
    mirror_range(&node, &reader, &mut lagging, &dir, 2, 3).await.unwrap();
    assert_eq!(lagging.next_block, 4);
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(0));
    assert!(node.read(move |engine| engine.is_staking_credited(&format!("{}:0", B256::repeat_byte(1)))).await);
    node.stop().await;
}

#[tokio::test]
async fn mirrors_hold_until_init_staking_applies() {
    let dir = scratch("staking-hold");
    let node = Node::start_confirmed(&dir);
    let account = AccountId([8; 20]);
    let events = vec![staked(1, 1, account, 500)];
    let reader = StubReader(events);
    let mut cursor = cursor_at(1);
    mirror_range(&node, &reader, &mut cursor, &dir, 1, 3).await.unwrap();
    assert_eq!(cursor.next_block, 1, "the cursor holds while staking is not initialised");
    assert!(node.read(move |engine| engine.staked_balances().is_empty()).await);
    init_staking(&node).await;
    mirror_range(&node, &reader, &mut cursor, &dir, 1, 3).await.unwrap();
    assert_eq!(cursor.next_block, 4);
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(500));
    node.stop().await;
}

#[tokio::test]
async fn an_event_on_an_unproven_block_stalls_the_mirror() {
    let dir = scratch("staking-unproven");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([9; 20]);
    let mut moved = staked(2, 2, account, 500);
    moved.block_hash = B256::repeat_byte(0xe0);
    let reader = StubReader(vec![moved]);
    let mut cursor = cursor_at(1);
    let error = mirror_range(&node, &reader, &mut cursor, &dir, 1, 3).await.unwrap_err();
    assert!(matches!(error, PollError::Stalled(Stall::Unsettled(_))), "{error:?}");
    assert_eq!(cursor.next_block, 1, "the cursor holds for the retry");
    assert!(node.read(move |engine| engine.staked_balances().is_empty()).await);
    node.stop().await;
}

#[test]
fn the_staking_cursor_refuses_another_contract_or_ahead_of_the_log() {
    let dir = scratch("staking-cursor");
    let path = dir.join("staking-cursor.json");
    let mut cursor = cursor_at(10);
    cursor.log_seq = Some(5);
    cursor.store(&path).unwrap();
    let loaded = StakingCursor::load(&path, CHAIN_ID, cursor.contract, 1, Some(5)).unwrap();
    assert_eq!(loaded, cursor);
    assert!(StakingCursor::load(&path, CHAIN_ID, cursor.contract, 1, Some(4)).is_err());
    assert!(StakingCursor::load(&path, CHAIN_ID, alloy::primitives::Address::repeat_byte(1), 1, Some(5)).is_err());
}

#[test]
fn monkey_units_refuse_amounts_past_u128() {
    assert_eq!(monkey_units(U256::from(u128::MAX)), Some(u128::MAX));
    assert_eq!(monkey_units(U256::from(u128::MAX) + U256::from(1)), None);
}

fn poll_chains(head: u64) -> [FakeChain; 3] {
    std::array::from_fn(|_| {
        let chain = FakeChain::default();
        {
            let mut state = chain.0.lock().unwrap();
            state.head = head;
            state.final_block = Some(head);
            state.withdrawal_block = Some(head);
        }
        chain
    })
}

/// The confirmed watcher with a staking contract configured.
fn staking_watcher(chains: &[FakeChain; 3], dir: &Path) -> DepositWatcher<Quorum<FakeChain>> {
    staking_watcher_with_range(chains, dir, 100)
}

fn staking_watcher_with_range(chains: &[FakeChain; 3], dir: &Path, range: u64) -> DepositWatcher<Quorum<FakeChain>> {
    let toml = format!(
        r#"{}
        staking = {{ contract = "0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a", cursor_path = "{}", registry = {{ contract = "{REGISTRY}" }} }}
        "#,
        evm_toml(dir, range),
        dir.join("staking-cursor.json").display()
    );
    let cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
    let quorum =
        Quorum::new(chains.iter().enumerate().map(|(i, chain)| (format!("p{i}"), chain.clone())).collect()).unwrap();
    DepositWatcher::start(quorum, &cfg, CHAIN_ID, VAULT, None).unwrap()
}

#[tokio::test]
async fn the_confirmed_poll_mirrors_staking_events_through_the_proven_window() {
    let dir = scratch("staking-poll");
    let node = Node::start_confirmed(&dir);
    init_staking(&node).await;
    let account = AccountId([3; 20]);
    let chains = poll_chains(10);
    for chain in &chains {
        chain.0.lock().unwrap().staking.push(staked(2, 1, account, 500));
    }
    let mut watcher = staking_watcher(&chains, &dir);
    watcher.poll(&node.tx).await.unwrap();
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(500));
    let cursor: StakingCursor =
        serde_json::from_str(&std::fs::read_to_string(dir.join("staking-cursor.json")).unwrap()).unwrap();
    assert_eq!(cursor.next_block, 11, "the mirror follows the poll's proven window");
    node.stop().await;
}

#[tokio::test]
async fn the_confirmed_poll_holds_staking_mirrors_before_init_staking() {
    let dir = scratch("staking-poll-hold");
    let node = Node::start_confirmed(&dir);
    let account = AccountId([4; 20]);
    let chains = poll_chains(10);
    for chain in &chains {
        chain.0.lock().unwrap().staking.push(staked(2, 1, account, 500));
    }
    let mut watcher = staking_watcher(&chains, &dir);
    watcher.poll(&node.tx).await.unwrap();
    assert!(node.read(move |engine| engine.staked_balances().is_empty()).await);
    assert!(!dir.join("staking-cursor.json").exists(), "the cursor holds with the mirrors");
    node.stop().await;
}

#[tokio::test]
async fn authenticated_progress_overrides_a_forged_cursor_and_replays_a_partial_frontier_block() {
    for completed in [false, true] {
        let dir = scratch(if completed { "staking-forged-cursor" } else { "staking-partial-block" });
        let node = Node::start_confirmed(&dir);
        init_staking(&node).await;
        let account = AccountId([9; 20]);
        let first_block = if completed { 1 } else { 2 };
        let first = staked(first_block, 21, account, 500);
        let events = node
            .submit(Command::Stake {
                account,
                amount: TokenUnits(500),
                block: first_block,
                chain_ref: first.chain_ref(),
            })
            .await;
        assert!(matches!(events[..], [Event::Staked { .. }]));
        if completed {
            assert!(
                node.submit(Command::StakingProgress { block: 1, block_hash: canonical_hash(1), block_time: None })
                    .await
                    .is_empty()
            );
        }
        let mut forged = cursor_at(10_000);
        forged.contract = alloy::primitives::Address::repeat_byte(0x5a);
        forged.store(&dir.join("staking-cursor.json")).unwrap();
        let chains = poll_chains(10);
        for chain in &chains {
            chain.0.lock().unwrap().staking.extend([first.clone(), unstaked(2, 22, account, 500)]);
        }
        let mut watcher = staking_watcher(&chains, &dir);
        watcher.poll(&node.tx).await.unwrap();
        assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(0));
        let progress = node.read(|engine| engine.staking().unwrap().completed_through.unwrap()).await;
        assert_eq!(progress.block, 10);
        assert_eq!(progress.block_hash, canonical_hash(10));
        node.stop().await;
    }
}

#[tokio::test]
async fn staking_catchup_keeps_its_anchor_while_deposit_finality_advances() {
    for backlog in [64u64, 256, 1024] {
        let dir = scratch(&format!("staking-moving-finality-{backlog}"));
        let node = Node::start_confirmed(&dir);
        let chains = poll_chains(backlog);
        let mut watcher = staking_watcher_with_range(&chains, &dir, 4);
        for _ in 0..backlog * 2 {
            watcher.poll(&node.tx).await.unwrap();
            if node.read(move |engine| engine.custody_checkpoint().is_some_and(|p| p.block == backlog)).await {
                break;
            }
        }
        assert_eq!(node.read(|engine| engine.custody_checkpoint().unwrap().block).await, backlog);
        init_staking(&node).await;
        let account = AccountId([10; 20]);
        for chain in &chains {
            let mut state = chain.0.lock().unwrap();
            state.header_reads.clear();
            state.staking.extend([staked(2, 23, account, 500), unstaked(backlog - 1, 24, account, 500)]);
        }
        let mut caught_up = false;
        let mut peak = 0;
        for poll in 0..backlog * 4 {
            if poll % 4 == 0 {
                for chain in &chains {
                    let mut state = chain.0.lock().unwrap();
                    state.head += 1;
                    state.final_block = Some(state.head);
                    state.withdrawal_block = Some(state.head);
                }
            }
            watcher.poll(&node.tx).await.unwrap();
            peak = peak.max(watcher.staking_anchor_count());
            assert!(peak <= 66, "hierarchical anchors must have a fixed memory bound");
            assert!(watcher.staking_hash_window_peak() <= 4, "full hashes cannot exceed the configured chunk");
            let target = chains[0].0.lock().unwrap().head;
            if node
                .read(move |engine| engine.staking().unwrap().completed_through.is_some_and(|p| p.block == target))
                .await
            {
                caught_up = true;
                break;
            }
        }
        assert!(caught_up, "full staking frontier must catch advancing deposit finality: backlog {backlog}");
        assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(0));
        assert!(node.read(|engine| engine.is_staking_credited(&format!("{}:0", B256::repeat_byte(24)))).await);
        let levels = u64::from(backlog.ilog2()) + 1;
        for chain in &chains {
            let work: u64 = chain.0.lock().unwrap().header_reads.iter().map(|(from, to)| to - from + 1).sum();
            println!("backlog={backlog} headers={work} peak_anchors={peak}");
            assert!(work <= 8 * backlog * levels, "header work must be O(N log N), not quadratic: {work}");
        }
        watcher.poll(&node.tx).await.unwrap();
        assert!(watcher.staking_anchor_count() <= 1, "completed anchors are pruned with authenticated progress");
        node.stop().await;
    }
}

#[tokio::test]
async fn staking_reads_narrow_after_size_failures_while_deposit_finality_advances() {
    let dir = scratch("staking-range-narrowing");
    let node = Node::start_confirmed(&dir);
    let chains = poll_chains(64);
    let mut watcher = staking_watcher_with_range(&chains, &dir, 16);
    for _ in 0..16 {
        watcher.poll(&node.tx).await.unwrap();
    }
    assert_eq!(node.read(|engine| engine.custody_checkpoint().unwrap().block).await, 64);
    init_staking(&node).await;
    let account = AccountId([11; 20]);
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        state.log_range_cap = Some(2);
        state.reads.clear();
        state.staking.extend([staked(2, 31, account, 500), unstaked(60, 32, account, 500)]);
    }
    let mut caught_up = false;
    for poll in 0..400 {
        if poll % 5 == 0 {
            for chain in &chains {
                let mut state = chain.0.lock().unwrap();
                state.head += 1;
                state.final_block = Some(state.head);
                state.withdrawal_block = Some(state.head);
            }
        }
        // A refused read stalls the poll; the next poll retries with a narrower range.
        let _ = watcher.poll(&node.tx).await;
        let target = chains[0].0.lock().unwrap().head;
        if node.read(move |engine| engine.staking().unwrap().completed_through.is_some_and(|p| p.block == target)).await
        {
            caught_up = true;
            break;
        }
    }
    assert!(caught_up, "staking catch-up must pass the provider's result cap");
    assert!(node.read(|engine| engine.is_staking_credited(&format!("{}:0", B256::repeat_byte(32)))).await);
    assert_eq!(node.read(move |engine| engine.staked(account)).await, TokenUnits(0));
    for chain in &chains {
        let state = chain.0.lock().unwrap();
        // 16 -> 8 -> 4 -> 2: the range halves per failure, so refusals are bounded by the halvings.
        assert!(state.staking_refusals <= 3 * 3, "refused {} times", state.staking_refusals);
    }
    assert!(watcher.staking_range_for_test() <= 2);
    assert_eq!(watcher.log_range_for_test(), 16, "staking failures leave the deposit range alone");
    node.stop().await;
}

#[tokio::test]
async fn a_staking_window_in_progress_keeps_its_end_while_the_release_tag_advances() {
    let dir = scratch("staking-window-retained");
    let node = Node::start_confirmed(&dir);
    let chains = poll_chains(6);
    let mut watcher = staking_watcher_with_range(&chains, &dir, 100);
    watcher.poll(&node.tx).await.unwrap();
    init_staking(&node).await;
    for chain in &chains {
        chain.0.lock().unwrap().header_batch_cap = Some(2);
    }
    let mut done = None;
    for _ in 0..40 {
        for chain in &chains {
            let mut state = chain.0.lock().unwrap();
            state.head += 1;
            state.final_block = Some(state.head);
            state.withdrawal_block = Some(state.head);
        }
        let _ = watcher.poll(&node.tx).await;
        done = node.read(|engine| engine.staking().unwrap().completed_through).await;
        if done.is_some() {
            break;
        }
    }
    let done = done.expect("a proof in progress must finish although the release tag moves every poll");
    assert!(done.block < 46, "the first window ended at {}, not at the moving tag", done.block);
    node.stop().await;
}

#[tokio::test]
async fn staking_hierarchy_survives_cancellation_and_rejects_changed_cached_anchor() {
    let dir = scratch("staking-hierarchical-proof");
    let chains = poll_chains(64);
    let mut watcher = staking_watcher_with_range(&chains, &dir, 4);
    watcher.chain.insert(64, canonical_hash(64));
    assert!(!watcher.prove_staking_for_test(0, 3).await.unwrap());
    for chain in &chains {
        chain.0.lock().unwrap().header_latency = Some(Duration::from_secs(1));
    }
    assert!(timeout(Duration::from_millis(10), watcher.prove_staking_for_test(0, 3)).await.is_err());
    for chain in &chains {
        chain.0.lock().unwrap().header_latency = None;
    }
    let mut complete = false;
    for _ in 0..64 {
        if watcher.prove_staking_for_test(0, 3).await.unwrap() {
            complete = true;
            break;
        }
    }
    assert!(complete);
    for chain in &chains {
        let state = chain.0.lock().unwrap();
        assert_eq!(state.header_reads.iter().filter(|range| **range == (0, 3)).count(), 1);
    }
    assert!(watcher.staking_anchor_count() > 0);
    // The first completed proof retained midpoint 32. It must be revalidated before reuse.
    for chain in &chains {
        chain.0.lock().unwrap().forks.insert(32, B256::repeat_byte(0x77));
    }
    let error = watcher.prove_staking_for_test(4, 7).await.unwrap_err();
    assert!(format!("{error:?}").contains("staking proof anchor changed"));
    assert_eq!(watcher.staking_anchor_count(), 0, "conflicting cached anchors must be discarded");
}

mod anchor;
mod registry;
