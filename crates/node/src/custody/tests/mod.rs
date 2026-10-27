//! The deposit watcher against a fake chain and a real sequencer over a scratch log.

use super::*;

use tokio::sync::mpsc;

use crate::custody::cursor::Cursor;
use crate::custody::evm::{Beneficiary, VaultDeposit, VaultIdentity, check_complete};
use crate::custody::sizing::AdaptiveSize;
use crate::custody::stall::{PollError, Polled, Stall};
mod alerts;
mod anvil_finality;
mod confirmed;
mod cursor;
mod fakes;
mod legacy;
mod reorg;
use crate::custody::tests::fakes::*;
mod quorum;
mod quorum_diagnostics;
mod rpc;
mod staking;

#[test]
fn usdc_base_units_convert_to_fixed_exactly() {
    let fixed = |units: u64| usdc_to_fixed(U256::from(units)).unwrap();
    assert_eq!(fixed(1), "0.000001".parse().unwrap());
    assert_eq!(fixed(123_456_789), "123.456789".parse().unwrap());
    // 1e9 USDC is the engine's input bound, the vault's largest deposit.
    assert_eq!(fixed(1_000_000_000 * USDC), Fixed::MAX_INPUT);
    assert!(!fixed(1_000_000_000 * USDC + 1).is_valid_input());
    let largest = U256::from(i128::MAX / 1_000);
    assert_eq!(usdc_to_fixed(largest), Some(Fixed::from_raw(i128::MAX / 1_000 * 1_000)));
    assert_eq!(usdc_to_fixed(largest + U256::from(1)), None);
    assert_eq!(usdc_to_fixed(U256::MAX), None);
}

#[tokio::test]
async fn a_deposit_is_credited_once_its_block_is_final() {
    let dir = scratch("final");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 5, Beneficiary::Trader(TRADER), 250 * USDC));
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    chain.set_final(Some(4));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert_eq!(node.balance(TRADER).await, None);

    chain.set_final(Some(5));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert_eq!(node.balance(TRADER).await, Some(usdc("250")));
    let cursor = stored_cursor(&dir);
    assert_eq!((cursor.next_block, cursor.next_deposit_id), (6, 1));
}

#[tokio::test]
async fn nothing_is_credited_from_an_endpoint_on_another_chain_or_vault_token() {
    let dir = scratch("identity");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 1, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(1));
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    let identities = [
        VaultIdentity { chain_id: 1, token: Some(USDC_TOKEN) },
        VaultIdentity { chain_id: CHAIN_ID, token: Some(VAULT) },
        // No vault at that address: the endpoint is on another chain with the same id.
        VaultIdentity { chain_id: CHAIN_ID, token: None },
    ];
    for identity in identities {
        chain.0.lock().unwrap().identity = Some(identity);
        assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Stalled(Stall::Inconsistent(_)))));
    }
    assert_eq!(node.balance(TRADER).await, None);
    assert!(chain.reads().is_empty());

    chain.0.lock().unwrap().identity = None;
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert_eq!(node.balance(TRADER).await, Some(usdc("1")));
}

#[tokio::test]
async fn a_house_deposit_adds_house_capital_only() {
    let dir = scratch("house");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 1, Beneficiary::House, 5_000_000 * USDC));
    chain.set_final(Some(1));
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert_eq!(node.read(|e| (e.house_nav(), e.accounts().len())).await, (usdc("5000000"), 0));
}

#[tokio::test]
async fn a_deposit_read_again_after_a_restart_is_credited_once() {
    let dir = scratch("restart");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 3, Beneficiary::Trader(TRADER), 40 * USDC));
    chain.set_final(Some(3));
    assert!(matches!(watcher(&chain, &dir, 1_000, None).poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    node.stop().await;

    // A crash after the credit but before the cursor write: the restart reads block 3 again.
    std::fs::remove_file(dir.join("cursor.json")).unwrap();
    let node = Node::start(&dir);
    let mut watcher = watcher(&chain, &dir, 1_000, node.last_seq().await);
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));

    assert_eq!(node.balance(TRADER).await, Some(usdc("40")));
    assert_eq!(deposit_commands(&dir), 1, "the second read must not reach the log");
}

#[tokio::test]
async fn a_missing_log_pauses_crediting_instead_of_skipping_the_deposit() {
    let dir = scratch("missing");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 3, Beneficiary::Trader(TRADER), 7 * USDC));
    chain.set_final(Some(5));
    chain.0.lock().unwrap().hide_logs = true;
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Stalled(Stall::Unsettled(_)))));
    assert!(!dir.join("cursor.json").exists());

    chain.0.lock().unwrap().hide_logs = false;
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert_eq!(node.balance(TRADER).await, Some(usdc("7")));
}

#[tokio::test]
async fn a_vault_count_below_the_deposits_already_read_pauses_crediting() {
    let dir = scratch("regress");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    // A vault redeployed at the same address: one deposit, where the cursor has read three.
    let stale = Cursor {
        chain_id: CHAIN_ID,
        vault: VAULT,
        next_block: 10,
        next_deposit_id: 3,
        log_seq: None,
        last_block_hash: None,
        crediting: None,
    };
    stale.store(&dir.join("cursor.json")).unwrap();
    chain.add(deposit(0, 7, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(12));
    // Any non-empty log: the cursor's watermark is not what this test checks.
    let mut watcher = watcher(&chain, &dir, 1_000, Some(0));

    assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Stalled(Stall::Inconsistent(_)))));
    assert_eq!(stored_cursor(&dir), stale);
}

#[tokio::test]
async fn a_log_read_the_provider_refuses_is_retried_over_smaller_ranges() {
    let dir = scratch("cap");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 50, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(999));
    chain.0.lock().unwrap().log_range_cap = Some(100);
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    // 1000 → 500 → 250 → 125 → 62 blocks: four refusals, then a read that fits.
    for _ in 0..4 {
        assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Stalled(Stall::Rpc(_)))));
    }
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(false)));

    assert_eq!(chain.reads(), vec![(0, 61)]);
    assert_eq!(node.balance(TRADER).await, Some(usdc("1")));
}

#[tokio::test]
async fn a_header_batch_that_fails_stays_at_the_size_that_works_until_caught_up() {
    let dir = scratch("header-fail");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.set_final(Some(1_999));
    chain.0.lock().unwrap().header_failures = 1;
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Stalled(Stall::Rpc(_)))));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(false)));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    // Half-size after the failure, and no larger while catching up, through the proof and the
    // second window's own read; logs keep their own size.
    let halved: Vec<(u64, u64)> = [0, 1, 2, 3, 2, 3].iter().map(|i| (i * 500, i * 500 + 499)).collect();
    assert_eq!(chain.0.lock().unwrap().header_reads, halved);
    assert_eq!(chain.reads(), vec![(0, 999), (1_000, 1_999)]);

    // Once caught up, good batches earn the size back.
    chain.set_final(Some(3_999));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(false)));
    let reads = chain.0.lock().unwrap().header_reads.clone();
    assert_eq!(reads[6..], [(2_000, 2_499), (2_500, 3_499), (3_500, 3_999)]);
}

#[tokio::test]
async fn windows_past_the_first_are_read_under_a_lasting_batch_limit() {
    let dir = scratch("batch-cap");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 2_500, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(2_999));
    chain.0.lock().unwrap().header_batch_cap = Some(100);
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    for _ in 0..30 {
        let _ = watcher.poll(&node.tx).await;
    }
    assert_eq!(stored_cursor(&dir).next_block, 3_000);
    assert_eq!(node.balance(TRADER).await, Some(usdc("1")));
}

#[tokio::test]
async fn a_capped_log_read_settles_below_the_cap_while_the_tag_moves() {
    let dir = scratch("cap-moving");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 300, Beneficiary::Trader(TRADER), USDC));
    chain.0.lock().unwrap().log_range_cap = Some(250);
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    let mut failures = 0;
    for poll in 0..8u64 {
        // The tag moves on before every poll, so header reads keep succeeding.
        chain.set_final(Some(999 + 10 * poll));
        failures += usize::from(watcher.poll(&node.tx).await.is_err());
    }
    assert_eq!(node.balance(TRADER).await, Some(usdc("1")));
    // 1000 and 500 fail; 250 works and is kept, so no read fails again.
    assert_eq!(failures, 2);
}

#[tokio::test]
async fn nothing_is_read_before_any_block_is_final() {
    let dir = scratch("no-final");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 0, Beneficiary::Trader(TRADER), USDC));
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert!(chain.reads().is_empty());
    assert!(!dir.join("cursor.json").exists());
}

#[test]
fn a_deposit_reported_outside_the_read_range_is_refused() {
    let inside = deposit(0, 5, Beneficiary::Trader(TRADER), USDC);
    assert!(check_complete(std::slice::from_ref(&inside), 5, 9, 0, 1).is_ok());
    for block in [4, 10] {
        let outside = VaultDeposit { block, ..inside.clone() };
        let err = check_complete(&[outside], 5, 9, 0, 1).unwrap_err();
        assert!(format!("{err:#}").contains("outside 5..=9"), "{err:#}");
    }
}

#[tokio::test]
async fn a_caught_up_watcher_reads_each_header_once() {
    let dir = scratch("one-read");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.set_final(Some(9));
    let mut watcher = watcher(&chain, &dir, 1_000, None);
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    chain.set_final(Some(15));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));

    assert_eq!(chain.0.lock().unwrap().header_reads, vec![(0, 9), (10, 15)]);
}

#[tokio::test]
async fn a_proof_interrupted_by_a_failed_read_resumes_where_it_stopped() {
    let dir = scratch("resume-proof");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.set_final(Some(99));
    let mut watcher = watcher(&chain, &dir, 100, None);
    // The first read fails (the range halves to 50); so does the one after 0..=49 succeeds.
    chain.0.lock().unwrap().fail_header_reads = vec![0, 2];

    assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Stalled(Stall::Rpc(_)))));
    assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Stalled(Stall::Rpc(_)))));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));

    let reads = chain.0.lock().unwrap().header_reads.clone();
    assert_eq!(reads[..2], [(0, 49), (50, 74)], "the third attempt starts past block 49");
}

#[tokio::test]
async fn a_long_backlog_keeps_one_proven_hash_per_block_range() {
    let dir = scratch("sparse");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.set_final(Some(999));
    let mut watcher = watcher(&chain, &dir, 10, None);

    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(false)));
    // 1,000 blocks proven, 10 read: anchors at every tenth block from 9, not one per block.
    assert_eq!(watcher.chain.len(), 100);
}

#[tokio::test]
async fn a_long_backlog_is_read_in_bounded_ranges() {
    let dir = scratch("backlog");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    for (id, block) in [3, 15, 25].into_iter().enumerate() {
        chain.add(deposit(id as u64, block, Beneficiary::Trader(TRADER), USDC));
    }
    chain.set_final(Some(25));
    let mut watcher = watcher(&chain, &dir, 10, None);

    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(false)));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(false)));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));

    assert_eq!(chain.reads(), vec![(0, 9), (10, 19), (20, 25)]);
    assert_eq!(node.balance(TRADER).await, Some(usdc("3")));
}

#[tokio::test]
async fn deposits_beyond_the_engine_bounds_are_refused_with_an_alert() {
    let (logs, _guard) = Logs::capture();
    let dir = scratch("bounds");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    let max = 1_000_000_000 * USDC;
    chain.add(deposit(0, 1, Beneficiary::Trader(TRADER), max));
    chain.add(deposit(1, 2, Beneficiary::Trader(TRADER), max + 1));
    chain.add(deposit(2, 3, Beneficiary::House, max + 1));
    chain.add(VaultDeposit { units: U256::MAX, ..deposit(3, 4, Beneficiary::Trader(TRADER), 0) });
    chain.set_final(Some(4));
    let mut watcher = watcher(&chain, &dir, 1_000, None);

    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));

    assert_eq!(node.read(|e| (e.accounts()[&TRADER].balance, e.house_nav())).await, (Fixed::MAX_INPUT, Fixed::ZERO));
    assert_eq!(logs.count("alert=\"deposit_rejected\""), 1);
    assert_eq!(logs.count("alert=\"house_deposit_rejected\""), 1);
    assert_eq!(logs.count("alert=\"deposit_unrepresentable\""), 1);
    // The unrepresentable deposit never reaches the log; the cursor still moves past it.
    assert_eq!(deposit_commands(&dir), 3);
    let cursor = stored_cursor(&dir);
    assert_eq!(cursor.next_deposit_id, 4);
}

#[test]
fn a_deposit_reference_is_the_full_transaction_hash_and_the_log_index() {
    let deposit = VaultDeposit { log_index: 12, ..deposit(5, 1, Beneficiary::Trader(TRADER), USDC) };
    assert_eq!(deposit.chain_ref(), format!("0x{}05:12", "0".repeat(62)));
}

#[tokio::test]
async fn a_closed_sequencer_is_fatal_before_any_chain_read_without_eligible_blocks() {
    let dir = scratch("closed-before-read");
    let chain = FakeChain::default();
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    let mut watcher = watcher(&chain, &dir, 10, None);
    assert!(matches!(watcher.poll(&tx).await, Err(PollError::Fatal(_))));
    assert_eq!(chain.0.lock().unwrap().polls, 0);
}

#[tokio::test]
async fn the_watcher_stops_when_the_sequencer_has_gone() {
    let dir = scratch("gone");
    let chain = FakeChain::default();
    chain.add(deposit(0, 1, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(1));
    let (tx, rx) = mpsc::channel(1);
    drop(rx);

    let err = watcher(&chain, &dir, 1_000, None).run(tx).await.unwrap_err();
    assert!(format!("{err:#}").contains("sequencer stopped"), "{err:#}");
    assert!(!dir.join("cursor.json").exists());
}

#[test]
fn a_read_size_halves_on_failure_holds_and_recovers_after_good_reads() {
    let mut size = AdaptiveSize::new(1_000);
    size.failed();
    size.failed();
    assert_eq!(size.get(), 250);
    size.succeeded();
    assert_eq!(size.get(), 250, "no larger than the size that worked");
    (0..9).for_each(|_| size.succeeded());
    assert_eq!(size.get(), 500, "ten good reads raise the ceiling");
    size.reset_ceiling();
    size.succeeded();
    assert_eq!(size.get(), 1_000);
}

#[tokio::test]
async fn a_lower_tag_from_a_lagging_backend_is_checked_without_proving_again() {
    let dir = scratch("lower-tag");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.set_final(Some(30));
    let mut watcher = watcher(&chain, &dir, 10, None);
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(false)));
    let proof = chain.0.lock().unwrap().header_reads.len();

    // A backend behind the batch post reports an older safe block, still above the cursor: one
    // read up to the anchor above it, then the next window's own read.
    chain.set_final(Some(25));
    assert!(matches!(watcher.poll(&node.tx).await.map(Polled::caught_up), Ok(false)));
    assert_eq!(chain.0.lock().unwrap().header_reads[proof..], [(25, 29), (10, 19)]);
}
