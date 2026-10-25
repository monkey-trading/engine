//! Exact chronology, horizon equality, no-event commands, and retries.

use super::*;

use crate::history::authenticated::model::queue_key;

#[test]
fn every_command_expires_strictly_older_events_and_retains_horizon_equality() {
    let (mut index, _) = fixture("horizon", 10);
    let commands = [
        command(0, 0, Some(vec![deposit(A, 1), deposit(B, 2)])),
        command(1, 5, Some(vec![deposit(A, 3)])),
        command(2, 10, Some(vec![])),
        command(3, 11, None),
        command(4, 15, Some(vec![Event::AdminApplied])),
        command(5, 16, Some(vec![])),
    ];
    for (n, entry) in commands.iter().enumerate() {
        let previous_prefix = index.head().command_prefix;
        index.ingest(entry.clone()).expect("exact command");
        assert_eq!(
            index.head().command_prefix,
            extend_command_prefix(previous_prefix, command_digest(entry).expect("digest"))
        );
        for account in [A, B] {
            assert_eq!(index.page(query(account)).expect("page"), expected(&commands[..=n], &query(account), 10));
        }
        assert_eq!(index.head().last_command, Some(CommandId::of(entry).expect("identity")));
        if n == 2 {
            assert_eq!(index.page(query(A)).expect("horizon equality").events.len(), 2);
            assert_eq!(index.page(query(B)).expect("horizon equality B").events.len(), 1);
        }
        if n == 4 {
            assert_eq!(index.page(query(A)).expect("second horizon equality").events[0].ts, 5);
        }
    }
    let head = index.head();
    assert_eq!(head.queue_lo, 3);
    assert_eq!(head.queue_hi, 3);
    assert!(index.store.read(&head.root, &ListId { account: A, kind: None }.key()).expect("proved absence").is_none());
    assert!(index.store.read(&head.root, &ListId { account: B, kind: None }.key()).expect("proved absence").is_none());
    for ordinal in 0..3 {
        assert!(index.store.read(&head.root, &queue_key(ordinal)).expect("proved deleted queue entry").is_none());
    }
}

#[test]
fn empty_list_recreation_uses_fresh_generations_and_old_roots_keep_their_rows() {
    let (mut index, path) = fixture("generation", 1);
    let initial = command(0, 0, Some(vec![deposit(A, 1)]));
    index.ingest(initial.clone()).expect("initial");
    let first = meta(&index, A, None);
    let first_kind = meta(&index, A, Some(event_kind(&deposit(A, 1)).expect("kind")));
    let old_head = index.head();
    index.ingest(command(1, 2, Some(vec![deposit(A, 2)]))).expect("expire and recreate in one command");
    let second = meta(&index, A, None);
    assert!(second.generation > first.generation);
    assert!(second.generation > first_kind.generation);
    assert_eq!((second.lo, second.hi), (0, 1));
    assert_eq!(index.head().next_generation, 4);
    assert!(index.store.read(&index.head.root, &first.position(0).key()).expect("old ordinal deletion").is_none());
    assert_eq!(index.page(query(A)).expect("new event").events[0].event, deposit(A, 2));
    drop(index);
    let old = Index::resume(open_store(&path), old_head).expect("retained old root");
    assert_eq!(old.page(query(A)).expect("old event"), expected(&[initial], &query(A), 1));
}

#[test]
fn large_time_jump_drains_the_global_queue_across_accounts_and_kinds_without_a_cap() {
    let (mut index, _) = fixture("large-expiry", 2);
    let commands = [
        command(
            0,
            0,
            Some(vec![deposit(A, 1), deposit(B, 2), Event::BadDebt { account: A, amount: Fixed::from_int(1) }]),
        ),
        command(1, 1, Some(vec![deposit(B, 4), deposit(A, 5)])),
        command(2, i64::MAX, None),
    ];
    for entry in &commands {
        index.ingest(entry.clone()).expect("ingest");
    }
    assert_eq!((index.head().queue_lo, index.head().queue_hi), (5, 5));
    for account in [A, B] {
        for kind in [None, Some("deposited".to_owned()), Some("bad_debt".to_owned())] {
            let request = PageQuery { kind, ..query(account) };
            assert_eq!(index.page(request).expect("fully expired"), Page { events: vec![], next: None });
        }
    }
}

#[test]
fn same_timestamp_events_keep_cursor_order_and_legacy_none_never_creates_rows() {
    let (mut index, _) = fixture("legacy", i64::MAX);
    let commands = [
        command(0, 0, None),
        command(1, 0, Some(vec![deposit(A, 1), deposit(A, 2)])),
        command(2, 0, None),
        command(3, 0, Some(vec![deposit(A, 3)])),
    ];
    for entry in &commands {
        index.ingest(entry.clone()).expect("ingest");
    }
    let got = index.page(query(A)).expect("legacy semantics");
    assert_eq!(got, expected(&commands, &query(A), i64::MAX));
    assert_eq!(
        got.events.iter().map(|row| row.cursor).collect::<Vec<_>>(),
        [Offset { seq: 3, index: 0 }, Offset { seq: 1, index: 1 }, Offset { seq: 1, index: 0 },]
    );
    assert_eq!(index.head().queue_hi, 3);
}

#[test]
fn retries_do_not_reinsert_expired_rows_advance_versions_or_regress_progress() {
    let (mut index, path) = fixture("retries", 1);
    let older = command(0, 0, Some(vec![deposit(A, 1)]));
    let latest = command(1, 100, None);
    index.ingest(older.clone()).expect("older");
    index.ingest(latest.clone()).expect("expire");
    let head = index.head();
    index.ingest(latest.clone()).expect("equal-tip retry");
    index.ingest(older).expect("already covered prefix retry");
    assert_eq!(index.head(), head);
    assert_eq!(index.page(query(A)).expect("no reinsertion"), Page { events: vec![], next: None });
    let conflicting = command(1, 100, Some(vec![]));
    assert!(index.ingest(conflicting).is_err());
    assert_eq!(index.head(), head);
    drop(index);
    let mut resumed = Index::resume(open_store(&path), head.clone()).expect("resume");
    resumed.ingest(latest).expect("retry after restart");
    assert_eq!(resumed.head(), head);
}

#[test]
fn gaps_reversed_time_and_wrong_first_command_leave_the_head_unchanged() {
    let (mut index, _) = fixture("ordering", 100);
    let empty = index.head();
    assert!(index.ingest(command(1, 0, None)).is_err());
    assert!(index.ingest(command(0, -1, None)).is_err());
    assert_eq!(index.head(), empty);
    index.ingest(command(0, 10, Some(vec![deposit(A, 1)]))).expect("initial");
    let head = index.head();
    for bad in [command(2, 11, None), command(1, 9, None)] {
        assert!(index.ingest(bad).is_err());
        assert_eq!(index.head(), head);
    }
    assert_eq!(index.page(query(A)).expect("unchanged").events[0].event, deposit(A, 1));
}

#[test]
fn exact_large_expiry_requires_store_support_for_the_whole_batch() {
    let (mut index, _) = fixture("uncapped-expiry", 1_000);
    for seq in 0..100 {
        index
            .ingest(command(seq, i64::try_from(seq).expect("fixture time"), Some(vec![deposit(A, 1)])))
            .expect("seed one event per command");
    }
    assert_eq!(index.page(query(A)).expect("all retained events").events.len(), 100);
    let before = index.head();
    let expiry = command(100, 10_000, None);
    let mut private_commits = 0;
    index
        .ingest_observed(expiry.clone(), |visible, _, _| {
            private_commits += 1;
            assert_eq!(visible.head(), before);
            Ok(())
        })
        .expect("complete expiry through finite private batches");
    assert!(private_commits > 1);
    assert_eq!(
        index.head().command_prefix,
        extend_command_prefix(before.command_prefix, command_digest(&expiry).expect("digest"))
    );
    assert_eq!((index.head().queue_lo, index.head().queue_hi), (100, 100));
    assert_eq!(index.page(query(A)).expect("empty after full expiry"), Page { events: vec![], next: None });
}

#[test]
fn one_valid_command_is_not_split_or_capped_by_a_storage_update_envelope() {
    let (mut index, _) = fixture("uncapped-command", 1_000);
    let entry = command(0, 0, Some((0..100).map(|_| deposit(A, 1)).collect()));
    index.ingest(entry.clone()).expect("whole command update");
    assert_eq!(index.head().command_prefix, prefix(std::slice::from_ref(&entry)));
    assert_eq!(index.page(query(A)).expect("whole command page"), expected(&[entry], &query(A), 1_000));
    assert_eq!(index.head().queue_hi, 100);
}
