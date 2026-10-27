//! Mirroring staking events through `staking::mirror`: ordering against `InitStaking`, once-only
//! credit under the engine's references, and refusal to trust an event on an unproven block.

use std::collections::BTreeMap;
use std::path::Path;

use alloy::primitives::{B256, U256};

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
            .map(|number| BlockLink { number, hash: canonical_hash(number), parent_hash: B256::ZERO })
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

    async fn staking_events(&self, from: u64, to: u64) -> anyhow::Result<Vec<StakingEvent>> {
        Ok(self.0.iter().filter(|e| (from..=to).contains(&e.block)).cloned().collect())
    }
}

fn staked(block: u64, tx: u8, account: AccountId, amount: u64) -> StakingEvent {
    StakingEvent {
        block,
        block_hash: canonical_hash(block),
        tx_hash: B256::repeat_byte(tx),
        log_index: 0,
        account,
        amount: u128::from(amount),
        kind: StakingEventKind::Staked,
    }
}

fn unstaked(block: u64, tx: u8, account: AccountId, amount: u64) -> StakingEvent {
    StakingEvent { kind: StakingEventKind::UnstakeCompleted, ..staked(block, tx, account, amount) }
}

fn hashes_through(to: u64) -> BTreeMap<u64, B256> {
    (0..=to).map(|number| (number, canonical_hash(number))).collect()
}

fn cursor_at(start: u64) -> StakingCursor {
    StakingCursor { chain_id: CHAIN_ID, contract: alloy::primitives::Address::ZERO, next_block: start, log_seq: None }
}

async fn init_staking(node: &Node) {
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
    mirror(reader, &node.tx, cursor, &dir.join("staking-cursor.json"), from, to, &hashes_through(to)).await
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
    assert!(node.read(move |engine| engine.is_credited(&format!("{}:0", B256::repeat_byte(1)))).await);
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
    let toml = format!(
        r#"{}
        staking = {{ contract = "0x5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a", cursor_path = "{}" }}
        "#,
        evm_toml(dir, 100),
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
