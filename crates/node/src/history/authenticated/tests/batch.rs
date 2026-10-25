//! Batched ingestion indexes exactly what per-command ingestion does, in fewer durable commits.

use super::*;

use crate::history::KINDS;
use crate::test_support::every_event;

/// The same commands, split into batches at every boundary, expiring across batches and within
/// one, reach the same progress and pages as one-at-a-time ingestion. Roots differ only by the
/// store's own retirement receipts, which follow its commits.
#[test]
fn every_batch_split_reaches_the_per_command_head_and_pages() {
    let commands = vec![
        command(0, 0, None),
        command(1, 100, Some(every_event())),
        command(2, 150, Some(vec![deposit(A, 2), deposit(B, 3), deposit(A, 4)])),
        command(3, 1_200, None),
        command(4, 1_300, Some(vec![deposit(A, 5)])),
        command(5, 2_600, Some(vec![deposit(B, 6), deposit(A, 7)])),
    ];
    let (mut single, _) = fixture("batch-single", 1_000);
    for entry in &commands {
        single.ingest(entry.clone()).expect("per-command ingest");
    }
    for split in 0..=commands.len() {
        let (mut batched, _) = fixture(&format!("batch-split-{split}"), 1_000);
        batched.ingest_batch(&commands[..split]).expect("first batch");
        batched.ingest_batch(&commands[split.saturating_sub(1)..]).expect("second batch retries the tip");
        let (want, got) = (single.head(), batched.head());
        assert_eq!((got.last_command, got.command_prefix), (want.last_command, want.command_prefix), "split {split}");
        assert_eq!(
            (got.queue_lo, got.queue_hi, got.next_generation),
            (want.queue_lo, want.queue_hi, want.next_generation)
        );
        let mut accounts = vec![A, B];
        accounts.extend(every_event().iter().filter_map(account_of));
        for account in accounts {
            for kind in std::iter::once(None).chain(KINDS.into_iter().map(|kind| Some(kind.to_owned()))) {
                let request = || PageQuery { kind: kind.clone(), ..query(account) };
                assert_eq!(batched.page(request()).expect("proved page"), single.page(request()).unwrap());
            }
        }
    }
}

/// A batch of commands with no account events is one durable commit, not one per command.
#[test]
fn a_batch_without_account_events_is_one_durable_commit() {
    let (mut index, _) = fixture("batch-one-commit", 1_000);
    index.ingest(command(0, 0, None)).expect("first command");
    let before = index.head().root.version.expect("committed");
    let batch: Vec<_> = (1..=500).map(|seq| command(seq, i64::try_from(seq).unwrap(), None)).collect();
    index.ingest_batch(&batch).expect("batch");
    assert_eq!(index.head().root.version, Some(before + 1));
    assert_eq!(index.head().last_command.map(|c| c.seq), Some(500));
}

/// A gap or reversed time anywhere in a batch refuses the whole batch and leaves the head unchanged.
#[test]
fn a_bad_command_anywhere_in_a_batch_leaves_the_head_unchanged() {
    for bad in [command(3, 3, None), command(2, 0, None)] {
        let (mut index, _) = fixture("batch-refused", 1_000);
        index.ingest(command(0, 1, None)).expect("first command");
        let head = index.head();
        assert!(index.ingest_batch(&[command(1, 1, Some(vec![deposit(A, 1)])), bad]).is_err());
        assert_eq!(index.head(), head);
        index.ingest(command(1, 1, Some(vec![deposit(A, 1)]))).expect("the writer still accepts the next command");
    }
}
