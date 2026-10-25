//! Majority voting at the custody boundary, including lies, lag, outages and recovery.

use alloy::primitives::{B256, U256};
use engine_core::command::Command;

use crate::config::EvmCustodySection;
use crate::custody::evm::{Beneficiary, BlockRef, DepositWatcher, VaultReader};
use crate::custody::quorum::Quorum;
use crate::custody::stall::Polled;
use crate::custody::tests::fakes::*;

#[tokio::test]
async fn a_fabricated_deposit_is_not_credited_and_the_provider_is_alerted() {
    let dir = scratch("quorum-fabrication");
    let node = Node::start(&dir);
    let chains = chains();
    chains[0].add(deposit(0, 1, Beneficiary::Trader(TRADER), 50 * USDC));
    let (logs, _guard) = Logs::capture();
    let mut watcher = watcher(&chains, &dir);
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    assert_eq!(node.balance(TRADER).await, None);
    assert_eq!(stored_cursor(&dir).next_deposit_id, 0);
    let alert = logs.lines_with("alert=\"custody_rpc_provider_disagreed\"");
    assert!(alert.iter().any(|l| l.contains("provider=\"p0\"") && l.contains("operation=\"deposits\"")), "{alert:?}");
    assert!(alert.iter().any(|l| l.contains("operation=\"deposit_count\"")), "{alert:?}");
}

#[tokio::test]
async fn a_lost_quorum_credits_nothing_and_a_recovered_quorum_credits_once() {
    let dir = scratch("quorum-outages");
    let node = Node::start(&dir);
    let chains = chains();
    let d = deposit(0, 1, Beneficiary::Trader(TRADER), 10 * USDC);
    for chain in &chains {
        chain.add(d.clone());
    }
    let (logs, _guard) = Logs::capture();
    let mut watcher = watcher(&chains, &dir);
    chains[2].0.lock().unwrap().failing = true;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    assert_eq!(node.balance(TRADER).await, Some(usdc("10")));
    for chain in &chains {
        chain.add(deposit(1, 2, Beneficiary::Trader(TRADER), 20 * USDC));
        chain.set_final(Some(2));
        chain.0.lock().unwrap().head = 2;
    }
    chains[1].0.lock().unwrap().failing = true;
    for _ in 0..2 {
        assert!(watcher.poll(&node.tx).await.is_err());
    }
    assert_eq!(node.balance(TRADER).await, Some(usdc("10")));
    assert_eq!(stored_cursor(&dir).next_block, 2);
    let seq = node.last_seq().await;
    node.submit(Command::Tick).await;
    assert!(node.last_seq().await > seq, "sequencing continues without custody quorum");
    assert_eq!(logs.lines_with("alert=\"custody_rpc_quorum_lost\"").len(), 1);
    assert!(logs.lines_with("alert=\"custody_rpc_provider_failed\"").iter().any(|l| l.contains("provider=\"p1\"")));
    chains[1].0.lock().unwrap().failing = false;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    assert_eq!(node.balance(TRADER).await, Some(usdc("30")));
    assert_eq!(logs.lines_with("alert_resolved=\"custody_rpc_quorum_lost\"").len(), 1);
    assert_eq!(deposit_commands(&dir), 2);
}

#[tokio::test]
async fn identity_is_rechecked_after_startup_and_provider_recovery_resolves_its_alert() {
    let dir = scratch("quorum-identity-recovery");
    let node = Node::start(&dir);
    let chains = chains();
    let mut watcher = watcher(&chains, &dir);
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    let (logs, _guard) = Logs::capture();
    let wrong = crate::custody::evm::VaultIdentity { chain_id: 1, token: Some(USDC_TOKEN) };
    chains[2].0.lock().unwrap().identity = Some(wrong);
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    assert!(
        logs.lines_with("alert=\"custody_rpc_provider_disagreed\"")
            .iter()
            .any(|l| l.contains("operation=\"identity\""))
    );
    chains[1].0.lock().unwrap().identity = Some(wrong);
    assert!(watcher.poll(&node.tx).await.is_err(), "a quorum on another chain is rejected after startup");
    for chain in &chains {
        chain.0.lock().unwrap().identity = None;
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    assert!(
        logs.lines_with("alert_resolved=\"custody_rpc_provider_disagreed\"")
            .iter()
            .any(|l| l.contains("operation=\"identity\""))
    );
}

#[tokio::test]
async fn finality_uses_a_common_height_and_votes_on_its_hash() {
    let chains = chains();
    for (i, chain) in chains.iter().enumerate() {
        let mut state = chain.0.lock().unwrap();
        state.head = 12 + i as u64;
        state.final_block = Some(8 + i as u64);
        state.head_time = Some(100 + i as u64);
    }
    let quorum = quorum(&chains).await;
    let f = quorum.finality().await.unwrap();
    assert_eq!(f.tagged, Some(BlockRef { number: 9, hash: canonical_hash(9) }));
    assert_eq!((f.head, f.head_time), (13, 101));
    // One endpoint forging the common block cannot replace the other provider's vote.
    chains[2].0.lock().unwrap().forks.insert(9, B256::repeat_byte(0xff));
    chains[2].0.lock().unwrap().forks.insert(8, B256::repeat_byte(0xff));
    let (logs, _guard) = Logs::capture();
    assert_eq!(quorum.finality().await.unwrap().tagged.unwrap().number, 8);
    assert_eq!(logs.lines_with("alert=\"custody_rpc_quorum_lost\"").len(), 0);
    assert!(logs.lines_with("alert=\"custody_rpc_provider_disagreed\"").iter().any(|l| l.contains("provider=\"p2\"")));
    chains[0].set_final(Some(9));
    assert_eq!(quorum.finality().await.unwrap().tagged.unwrap().hash, canonical_hash(9));
}

#[tokio::test]
async fn finality_unavailable_on_a_majority_waits_and_inconsistent_tags_cannot_vote() {
    let chains = chains();
    let quorum = quorum(&chains).await;
    chains[0].set_final(None);
    chains[1].set_final(None);
    assert_eq!(quorum.finality().await.unwrap().tagged, None);
    chains[1].set_final(Some(1));
    chains[2].0.lock().unwrap().tag_hash = Some(B256::repeat_byte(99));
    assert!(quorum.finality().await.is_err());
    chains[2].0.lock().unwrap().tag_hash = None;
    chains[2].0.lock().unwrap().head = 0;
    assert!(quorum.finality().await.is_err(), "a finality tag above the provider head is refused");
    chains[2].0.lock().unwrap().head = 1;
    assert_eq!(quorum.finality().await.unwrap().tagged.unwrap().number, 1);
}

#[tokio::test]
async fn log_absence_needs_a_header_at_the_range_end_and_the_same_block_hash() {
    let chains = chains();
    let quorum = quorum(&chains).await;
    chains[1].0.lock().unwrap().header_failures = 10;
    chains[2].0.lock().unwrap().forks.insert(5, B256::repeat_byte(99));
    assert!(quorum.deposits(1, 5).await.is_err(), "empty logs on different forks do not agree");
    chains[2].0.lock().unwrap().header_failures = 10;
    assert!(quorum.deposits(1, 5).await.is_err(), "a lagging empty response is not a vote");
    chains[1].0.lock().unwrap().header_failures = 0;
    assert!(quorum.deposits(1, 5).await.unwrap().is_empty());
}

#[tokio::test]
async fn every_deposit_field_and_the_complete_set_must_agree() {
    let chains = chains();
    let d = deposit(0, 1, Beneficiary::Trader(TRADER), USDC);
    chains[0].add(d.clone());
    chains[1].add(d.clone());
    chains[2].add(d.clone());
    let quorum = quorum(&chains).await;
    let changed = [
        crate::custody::evm::VaultDeposit { id: 7, ..d.clone() },
        crate::custody::evm::VaultDeposit { block: 0, ..d.clone() },
        crate::custody::evm::VaultDeposit { units: U256::from(2 * USDC), ..d.clone() },
        crate::custody::evm::VaultDeposit { beneficiary: Beneficiary::House, ..d.clone() },
        crate::custody::evm::VaultDeposit { block_hash: B256::ZERO, ..d.clone() },
        crate::custody::evm::VaultDeposit { tx_hash: B256::repeat_byte(99), ..d.clone() },
        crate::custody::evm::VaultDeposit { log_index: 99, ..d.clone() },
    ];
    for bad in changed {
        chains[1].0.lock().unwrap().deposits = vec![bad];
        chains[2].0.lock().unwrap().deposits.clear();
        assert!(quorum.deposits(0, 1).await.is_err());
    }
    chains[1].0.lock().unwrap().deposits = vec![d.clone()];
    assert_eq!(quorum.deposits(0, 1).await.unwrap(), vec![d]);
}

#[tokio::test]
async fn header_and_count_votes_use_the_same_block_and_recover_without_cached_answers() {
    let chains = chains();
    let quorum = quorum(&chains).await;
    chains[2].0.lock().unwrap().forks.insert(1, B256::repeat_byte(99));
    assert_eq!(quorum.headers(1, 1).await.unwrap()[0].hash, canonical_hash(1));
    let at = BlockRef { number: 1, hash: canonical_hash(1) };
    assert_eq!(quorum.deposit_count(at).await.unwrap(), 0);
    chains[1].0.lock().unwrap().forks.insert(1, B256::repeat_byte(98));
    assert!(quorum.headers(1, 1).await.is_err());
    assert!(quorum.deposit_count(at).await.is_err());
    chains[1].0.lock().unwrap().forks.clear();
    assert_eq!(quorum.deposit_count(at).await.unwrap(), 0);
}

#[tokio::test]
async fn fewer_than_three_providers_are_refused_and_four_require_three_votes() {
    assert!(Quorum::<FakeChain>::new(vec![]).is_err());
    let providers = (0..4).map(|i| (format!("p{i}"), FakeChain::default())).collect();
    let quorum = Quorum::new(providers).unwrap();
    quorum.providers[3].1.0.lock().unwrap().failing = true;
    assert_eq!(quorum.identity().await.unwrap().chain_id, CHAIN_ID);
    quorum.providers[2].1.0.lock().unwrap().failing = true;
    assert!(quorum.identity().await.is_err(), "two of four must not form a quorum");
}

#[tokio::test]
async fn identity_dissent_cannot_supply_an_alternate_chain_credit_and_recovery_restores_votes() {
    let dir = scratch("quorum-identity-eligibility");
    let node = Node::start(&dir);
    let chains = chains();
    let other_hash = B256::repeat_byte(0x99);
    // Provider 1 truthfully serves another chain; provider 2 claims the intended identity
    // but mirrors that other chain's internally coherent headers, logs and historical count.
    chains[1].0.lock().unwrap().identity =
        Some(crate::custody::evm::VaultIdentity { chain_id: CHAIN_ID + 1, token: Some(USDC_TOKEN) });
    let mut fake = deposit(0, 1, Beneficiary::Trader(TRADER), 50 * USDC);
    fake.block_hash = other_hash;
    for chain in &chains[1..] {
        chain.0.lock().unwrap().forks.insert(1, other_hash);
        chain.add(fake.clone());
    }
    let mut watcher = watcher(&chains, &dir);
    assert!(watcher.poll(&node.tx).await.is_err());
    assert_eq!(node.balance(TRADER).await, None);
    assert_eq!(deposit_commands(&dir), 0);
    // Repairing identities/transcripts restores the same configured two-vote threshold.
    for chain in &chains {
        let mut state = chain.0.lock().unwrap();
        state.identity = None;
        state.forks.clear();
        state.deposits = vec![deposit(0, 1, Beneficiary::Trader(TRADER), 10 * USDC)];
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    assert_eq!(node.balance(TRADER).await, Some(usdc("10")));
    assert_eq!(deposit_commands(&dir), 1);
}

#[tokio::test]
async fn every_query_requires_current_identity_eligibility_and_failed_identity_clears_it() {
    let chains = chains();
    let quorum = Quorum::new(chains.iter().enumerate().map(|(i, c)| (format!("p{i}"), c.clone())).collect()).unwrap();
    let at = BlockRef { number: 1, hash: canonical_hash(1) };
    for phase in 0..4 {
        if phase > 0 {
            quorum.identity().await.unwrap();
        }
        if phase == 2 {
            chains[0].0.lock().unwrap().failing = true;
            chains[1].0.lock().unwrap().failing = true;
            assert!(quorum.identity().await.is_err());
            chains[0].0.lock().unwrap().failing = false;
            chains[1].0.lock().unwrap().failing = false;
        }
        let admitted = phase == 1 || phase == 3;
        assert_eq!(quorum.finality().await.is_ok(), admitted, "finality phase {phase}");
        assert_eq!(quorum.headers(1, 1).await.is_ok(), admitted, "headers phase {phase}");
        assert_eq!(quorum.deposits(1, 1).await.is_ok(), admitted, "deposits phase {phase}");
        assert_eq!(quorum.deposit_count(at).await.is_ok(), admitted, "count phase {phase}");
    }
    // Each query must independently exclude an identity dissenter even when its answer
    // matches a malicious admitted provider; the threshold must stay two out of three.
    chains[1].0.lock().unwrap().identity =
        Some(crate::custody::evm::VaultIdentity { chain_id: CHAIN_ID + 1, token: Some(USDC_TOKEN) });
    let other_hash = B256::repeat_byte(0x99);
    for chain in &chains[1..] {
        chain.0.lock().unwrap().forks.insert(1, other_hash);
    }
    quorum.identity().await.unwrap();
    assert!(quorum.finality().await.is_err());
    assert!(quorum.headers(1, 1).await.is_err());
    assert!(quorum.deposits(1, 1).await.is_err());
    assert!(quorum.deposit_count(BlockRef { number: 1, hash: other_hash }).await.is_err());
}

#[tokio::test]
async fn minority_head_and_timestamp_extremes_cannot_override_majority_diagnostics() {
    let chains = chains();
    let quorum = quorum(&chains).await;
    chains[1].set_final(Some(0));
    for (minority_head, minority_time, expected_head, expected_time) in [(1, 1, 10, 100), (999, 999_999, 11, 101)] {
        for (i, chain) in chains.iter().take(2).enumerate() {
            let mut state = chain.0.lock().unwrap();
            state.head = 10 + i as u64;
            state.head_time = Some(100 + i as u64);
        }
        chains[2].0.lock().unwrap().head = minority_head;
        chains[2].0.lock().unwrap().head_time = Some(minority_time);
        let finality = quorum.finality().await.unwrap();
        assert_eq!((finality.head, finality.head_time), (expected_head, expected_time));
        assert_eq!(finality.tagged.unwrap().hash, canonical_hash(1));
    }
}

#[tokio::test(start_paused = true)]
async fn one_pinned_head_cannot_hide_a_genuine_finality_stall_and_recovery_resolves_it() {
    let dir = scratch("quorum-pinned-head");
    let node = Node::start(&dir);
    let chains = chains();
    chains[1].set_final(Some(0));
    let mut watcher = watcher(&chains, &dir);
    let (logs, _guard) = Logs::capture();
    for chain in &chains[..2] {
        chain.0.lock().unwrap().head = 10;
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    for chain in &chains[..2] {
        chain.0.lock().unwrap().head = 11;
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    assert_eq!(logs.lines_with("alert=\"custody_finality_stalled\"").len(), 1);
    for chain in &chains[..2] {
        chain.set_final(Some(2));
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    assert_eq!(logs.lines_with("alert_resolved=\"custody_finality_stalled\"").len(), 1);
}

#[tokio::test]
async fn detected_tag_contradictions_cannot_supply_diagnostics_even_when_finality_has_a_majority() {
    let chains = chains();
    let quorum = quorum(&chains).await;
    for (i, chain) in chains.iter().take(2).enumerate() {
        chain.0.lock().unwrap().head = 10 + i as u64;
        chain.0.lock().unwrap().head_time = Some(100 + i as u64);
    }
    chains[2].0.lock().unwrap().head = 999;
    chains[2].0.lock().unwrap().head_time = Some(999_999);
    for above_head in [false, true] {
        chains[2].0.lock().unwrap().tag_hash = Some(B256::repeat_byte(0xff));
        chains[2].set_final(Some(if above_head { 1000 } else { 1 }));
        let f = quorum.finality().await.unwrap();
        assert_eq!((f.head, f.head_time), (10, 100));
        assert_eq!(f.tagged.unwrap().hash, canonical_hash(1));
    }
}

#[tokio::test(start_paused = true)]
async fn changing_minority_contributors_cannot_hide_majority_progress_from_unequal_baselines() {
    let dir = scratch("quorum-changing-contributors");
    let node = Node::start(&dir);
    let chains = chains();
    chains[1].set_final(Some(0));
    let mut watcher = watcher(&chains, &dir);
    let (logs, _guard) = Logs::capture();
    for (chain, head) in chains.iter().zip([10, 100, 999]) {
        chain.0.lock().unwrap().head = head;
    }
    chains[2].0.lock().unwrap().head_time = Some(u64::MAX);
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    for (chain, head) in chains.iter().zip([11, 101, 1]) {
        chain.0.lock().unwrap().head = head;
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    for (chain, head) in chains.iter().zip([12, 102, 1]) {
        chain.0.lock().unwrap().head = head;
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    assert_eq!(logs.lines_with("alert=\"custody_finality_stalled\"").len(), 1);
}

#[tokio::test(start_paused = true)]
async fn minority_only_progress_cannot_start_the_watchdog_and_a_later_majority_can() {
    let dir = scratch("quorum-minority-only-progress");
    let node = Node::start(&dir);
    let chains = chains();
    let mut watcher = watcher(&chains, &dir);
    let (logs, _guard) = Logs::capture();
    for (chain, head) in chains.iter().zip([10, 100, 1]) {
        chain.0.lock().unwrap().head = head;
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    chains[2].0.lock().unwrap().head = 999;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    assert_eq!(logs.lines_with("alert=\"custody_finality_stalled\"").len(), 0);
    for chain in &chains[..2] {
        chain.0.lock().unwrap().head += 1;
    }
    chains[2].0.lock().unwrap().head = 1;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    assert_eq!(logs.lines_with("alert=\"custody_finality_stalled\"").len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_newly_admitted_head_sets_its_baseline_without_fabricating_progress() {
    let dir = scratch("quorum-new-head-baseline");
    let node = Node::start(&dir);
    let chains = chains();
    chains[0].0.lock().unwrap().head = 10;
    chains[1].0.lock().unwrap().head = 1000;
    chains[1].0.lock().unwrap().failing = true;
    chains[2].0.lock().unwrap().head = 100;
    let mut watcher = watcher(&chains, &dir);
    let (logs, _guard) = Logs::capture();
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Read { .. })));
    chains[1].0.lock().unwrap().failing = false;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    assert_eq!(logs.lines_with("alert=\"custody_finality_stalled\"").len(), 0);
    for index in [0, 2] {
        chains[index].0.lock().unwrap().head += 1;
    }
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    assert!(matches!(watcher.poll(&node.tx).await, Ok(Polled::Waiting)));
    assert_eq!(logs.lines_with("alert=\"custody_finality_stalled\"").len(), 1);
}

// ── Helpers ──

/// Three independent fake chains, all final through block one.
pub(super) fn chains() -> [FakeChain; 3] {
    std::array::from_fn(|_| {
        let chain = FakeChain::default();
        chain.set_final(Some(1));
        chain.0.lock().unwrap().head = 1;
        chain
    })
}

/// A quorum that keeps the tests' handles to each provider.
pub(super) async fn quorum(chains: &[FakeChain; 3]) -> Quorum<FakeChain> {
    let quorum = Quorum::new(chains.iter().enumerate().map(|(i, c)| (format!("p{i}"), c.clone())).collect()).unwrap();
    quorum.identity().await.unwrap();
    quorum
}

/// A real watcher and sequencer around quorum reads.
pub(super) fn watcher(chains: &[FakeChain; 3], dir: &std::path::Path) -> DepositWatcher<Quorum<FakeChain>> {
    let cfg: EvmCustodySection = toml::from_str(&evm_toml(dir, 100)).unwrap();
    DepositWatcher::start_legacy(
        Quorum::new(chains.iter().enumerate().map(|(i, c)| (format!("p{i}"), c.clone())).collect()).unwrap(),
        &cfg,
        CHAIN_ID,
        VAULT,
        None,
    )
    .unwrap()
}
