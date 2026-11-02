//! The deposit cursor: what a restart resumes from, when it may move, and which cursors a
//! recovered log refuses.

use std::path::{Path, PathBuf};

use alloy::primitives::Address;
use engine_core::Engine;
use engine_core::command::Sequenced;
use tokio::sync::mpsc;

use crate::custody::cursor::{Cursor, temp_path};
use crate::custody::evm::Beneficiary;
use crate::custody::stall::{PollError, Polled, Stall};
use crate::custody::tests::fakes::*;
use crate::sequencer::Request;

#[tokio::test]
async fn the_cursor_survives_a_restart() {
    let dir = scratch("cursor");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.add(deposit(0, 2, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(9));
    assert!(matches!(watcher(&chain, &dir, 1_000, None).poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    chain.add(deposit(1, 12, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(12));

    let mut restarted = watcher(&chain, &dir, 1_000, node.last_seq().await);
    assert!(matches!(restarted.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));

    assert_eq!(chain.reads(), vec![(0, 9), (10, 12)]);
    assert_eq!(node.balance(TRADER).await, Some(usdc("2")));
}

#[tokio::test]
async fn the_cursor_moves_only_after_the_log_is_synced_past_the_credits() {
    let dir = scratch("sync");
    let cursor_path = dir.join("cursor.json");
    // A sequencer that records, at each sync request, the block the stored cursor has reached.
    let (tx, mut rx) = mpsc::channel::<Request>(64);
    let sequencer = tokio::spawn(async move {
        let (mut engine, mut seq, mut cursor_at_sync) = (Engine::new(), 0, Vec::new());
        while let Some(request) = rx.recv().await {
            match request {
                Request::Submit { cmd, reply } => {
                    let events = engine.apply(&Sequenced { seq, ts: 0, cmd }).unwrap();
                    seq += 1;
                    if let Some(reply) = reply {
                        let _ = reply.send(events);
                    }
                }
                Request::Query(query) => query(&engine),
                Request::Sync(reply) => {
                    let stored: Cursor = serde_json::from_str(&std::fs::read_to_string(&cursor_path).unwrap()).unwrap();
                    cursor_at_sync.push((stored.next_block, stored.crediting.is_some()));
                    let _ = reply.send(engine.last_seq());
                }
                Request::CheckpointStatus { .. } => unreachable!("no checkpoint worker in this test"),
                Request::WatchBenchmarkSchedule(_) => unreachable!("only the API watches benchmark schedules"),
                Request::HouseState(_) => unreachable!("only the API reads the house state"),
                Request::ReissueWithdrawal { .. } => unreachable!("only the API asks for a reissue"),
                Request::AccountPayouts { .. } => unreachable!("only the API asks for payouts"),
                Request::FinalCheckpoint(_) => unreachable!("only a handover asks for the final checkpoint"),
                Request::TimeSample(_) => unreachable!("custody sends no time samples"),
            }
        }
        cursor_at_sync
    });
    let chain = FakeChain::default();
    chain.add(deposit(0, 1, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(1));

    assert!(matches!(watcher(&chain, &dir, 1_000, None).poll(&tx).await.map(Polled::caught_up), Ok(true)));
    drop(tx);
    // At the sync the cursor still stands before the range, marked as crediting it.
    assert_eq!(sequencer.await.unwrap(), vec![(0, true)]);
    let stored = stored_cursor(&dir);
    assert_eq!((stored.next_block, stored.log_seq, stored.crediting), (2, Some(0), None));
}

#[tokio::test]
async fn a_sequencer_that_cannot_sync_leaves_the_cursor_where_it_was() {
    let dir = scratch("sync-fails");
    let tx = engine_sequencer(false);
    let chain = FakeChain::default();
    chain.add(deposit(0, 1, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(1));

    let result = watcher(&chain, &dir, 1_000, None).poll(&tx).await;
    assert!(matches!(&result, Err(PollError::Fatal(e)) if format!("{e:#}").contains("sequencer stopped")));
    let stored = stored_cursor(&dir);
    assert_eq!((stored.next_block, stored.crediting), (0, Some((1, canonical_hash(1)))));
}

#[tokio::test]
async fn a_cursor_that_cannot_be_stored_stops_the_watcher() {
    let dir = scratch("store-fails");
    let (chain, node) = (FakeChain::default(), Node::start(&dir));
    chain.set_final(Some(3));
    let mut watcher = watcher(&chain, &dir, 1_000, None);
    // A directory where the cursor file belongs: the rename over it fails.
    let blocker: PathBuf = dir.join("cursor.json");
    std::fs::create_dir_all(blocker.join("occupied")).unwrap();

    assert!(matches!(watcher.poll(&node.tx).await, Err(PollError::Fatal(_))));
}

#[test]
fn a_cursor_is_written_beside_itself_whatever_its_name() {
    assert_eq!(temp_path(Path::new("data/custody.tmp")), Path::new("data/custody.tmp.tmp"));
    assert_eq!(temp_path(Path::new("data/cursor.json")), Path::new("data/cursor.json.tmp"));
}

#[test]
fn a_cursor_is_refused_when_it_belongs_elsewhere_or_is_ahead_of_its_log() {
    let path = scratch("cursor-load").join("cursor.json");
    let fresh = Cursor::load(&path, CHAIN_ID, VAULT, 77, None).unwrap();
    assert_eq!((fresh.next_block, fresh.next_deposit_id, fresh.log_seq), (77, 0, None));
    // (the cursor's log seq, the recovered log's last seq, whether the cursor loads). A cursor
    // with no log seq has credited nothing, so it fits every log, an empty one included.
    let cases = [
        (None, None, true),
        (None, Some(3), true),
        (Some(9), Some(9), true),
        (Some(9), Some(10), true),
        (Some(9), Some(8), false),
        (Some(9), None, false),
    ];
    for (cursor_seq, recovered, loads) in cases {
        Cursor { next_block: 100, next_deposit_id: 4, log_seq: cursor_seq, ..fresh.clone() }.store(&path).unwrap();
        let loaded = Cursor::load(&path, CHAIN_ID, VAULT, 77, recovered);
        assert_eq!(loaded.is_ok(), loads, "cursor at {cursor_seq:?}, log at {recovered:?}");
        if let Ok(cursor) = loaded {
            assert_eq!(cursor.next_block, 100);
        }
    }
    assert!(Cursor::load(&path, 1, VAULT, 77, Some(9)).is_err());
    assert!(Cursor::load(&path, CHAIN_ID, Address::repeat_byte(1), 77, Some(9)).is_err());
    std::fs::write(&path, "{not json").unwrap();
    assert!(Cursor::load(&path, CHAIN_ID, VAULT, 77, Some(9)).is_err());
}

#[tokio::test]
async fn a_cursor_stored_before_the_logs_first_command_resumes_on_restart() {
    // Trusted time refuses every command, so the log stays empty while the watcher stores its
    // cursor: once past blocks with no deposit, then marking a range it could not credit.
    let dir = scratch("cursor-before-log");
    let chain = FakeChain::default();
    chain.set_final(Some(4));
    let refusing = refusing_sequencer(true, usize::MAX);
    let mut first = watcher(&chain, &dir, 1_000, None);
    assert!(matches!(first.poll(&refusing).await.map(Polled::caught_up), Ok(true)));
    chain.add(deposit(0, 6, Beneficiary::Trader(TRADER), USDC));
    chain.set_final(Some(9));
    assert!(matches!(first.poll(&refusing).await, Err(PollError::Stalled(Stall::Refused(_)))));
    let stored = stored_cursor(&dir);
    assert_eq!((stored.next_block, stored.log_seq, stored.crediting.map(|(n, _)| n)), (5, None, Some(9)));

    // The restart finds the log still empty, resumes from the cursor, and credits the deposit.
    let node = Node::start(&dir);
    assert_eq!(node.last_seq().await, None);
    let mut restarted = watcher(&chain, &dir, 1_000, None);
    assert!(matches!(restarted.poll(&node.tx).await.map(Polled::caught_up), Ok(true)));
    assert_eq!(chain.reads()[2..], [(5, 9)]);
    assert_eq!(node.balance(TRADER).await, Some(usdc("1")));
}
