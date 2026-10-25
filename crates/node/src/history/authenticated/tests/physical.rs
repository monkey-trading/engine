//! Host disk faults leave the trusted root unchanged and use the actual packed Store.

use super::*;

#[test]
fn physical_metadata_deletion_and_narrowed_counts_cannot_forge_absence_or_completeness() {
    for selected in [None, Some("deposited".to_owned())] {
        for narrow in [false, true] {
            let (index, path) = ranked_fixture(&format!("physical-meta-{}-{narrow}", selected.is_some()));
            let kind = selected.as_deref().map(|_| event_kind(&deposit(A, 1)).expect("kind"));
            let original = meta(&index, A, kind);
            let mut forged = original.clone();
            forged.hi -= 1;
            let original_bytes = encoded(&original).expect("metadata bytes");
            let forged_bytes = encoded(&forged).expect("forged count bytes");
            tamper_payload(&path, &original_bytes, narrow.then_some(forged_bytes.as_slice()));
            assert!(index.page(PageQuery { kind: selected.clone(), ..query(A) }).is_err());
        }
    }
}

#[test]
fn physical_interior_and_lookahead_deletion_is_not_a_shorter_successful_page() {
    for (name, rank, limit) in [("interior", 3, 5), ("lookahead", 4, 1)] {
        for selected in [None, Some("deposited".to_owned())] {
            let (index, path) = ranked_fixture(&format!("physical-{name}-{}", selected.is_some()));
            let kind = selected.as_deref().map(|_| event_kind(&deposit(A, 1)).expect("kind"));
            let list = meta(&index, A, kind);
            let value = stored_row(&index, list.position(rank));
            tamper_payload(&path, &encoded(&value).expect("row bytes"), None);
            assert!(index.page(PageQuery { kind: selected, limit, ..query(A) }).is_err(), "{name}");
        }
    }
}

#[test]
fn same_length_valid_json_host_rewrite_is_rejected_after_resume_and_on_repeated_reads() {
    let (index, path) = ranked_fixture("physical-valid-json");
    let head = index.head();
    let list = meta(&index, A, None);
    let original = stored_row(&index, list.position(5));
    let mut forged = original.clone();
    forged.ts -= 1;
    let original_bytes = encoded(&original).expect("original");
    let forged_bytes = encoded(&forged).expect("forged");
    assert_eq!(original_bytes.len(), forged_bytes.len());
    drop(index);
    let resumed = Index::resume(open_store(&path), head.clone()).expect("fixed state valid before row damage");
    tamper_payload(&path, &original_bytes, Some(&forged_bytes));
    for _ in 0..2 {
        assert!(resumed.page(PageQuery { limit: 1, ..query(A) }).is_err());
    }
    assert_eq!(resumed.head(), head);
}

#[test]
fn expiry_failure_after_overlay_deletions_preserves_old_rows_and_retry_is_exact() {
    let (mut index, path) = ranked_fixture("physical-expiry-atomic");
    let head = index.head();
    let list = meta(&index, A, None);
    let original = stored_row(&index, list.position(3));
    let mut forged = original.clone();
    forged.ts -= 1;
    let original_bytes = encoded(&original).expect("original");
    let forged_bytes = encoded(&forged).expect("forged");
    tamper_payload(&path, &original_bytes, Some(&forged_bytes));
    assert!(index.ingest(command(6, 2_000, None)).is_err());
    assert_eq!(index.head(), head);
    assert_eq!(stored_row(&index, list.position(0)).cursor, Offset { seq: 0, index: 0 });
    assert_eq!((meta(&index, A, None).lo, meta(&index, A, None).hi), (0, 6));
    tamper_payload(&path, &forged_bytes, Some(&original_bytes));
    assert_eq!(index.page(query(A)).expect("restored old root").events.len(), 6);
    assert!(index.requires_rebuild());
    assert!(index.ingest(command(6, 2_000, None)).is_err(), "failed generation cannot retry");
    let mut fresh = Index::empty(index.store.clone(), [0x38; 32], 1_000).expect("fresh verified generation");
    for seq in 0..6 {
        fresh
            .ingest(command(seq, i64::try_from(seq).expect("time"), Some(vec![deposit(A, 1)])))
            .expect("rebuild prefix");
    }
    fresh.ingest(command(6, 2_000, None)).expect("rebuild exact expiry");
    assert_eq!((fresh.head().queue_lo, fresh.head().queue_hi), (6, 6));
    assert_eq!(fresh.page(query(A)).expect("expired").events.len(), 0);
}

#[test]
fn unrelated_host_rewrite_cannot_be_laundered_by_a_valid_append() {
    let (mut index, path) = fixture("physical-update", 1_000);
    index.ingest(command(0, 0, Some(vec![deposit(A, 1), deposit(B, 2)]))).expect("initial");
    let b = meta(&index, B, None);
    let original = stored_row(&index, b.position(0));
    let original_bytes = encoded(&stored_event(&index, &original)).expect("original event");
    let forged_bytes = encoded(&deposit(B, 9)).expect("forged event");
    tamper_payload(&path, &original_bytes, Some(&forged_bytes));
    let before = index.head();
    match index.ingest(command(1, 1, Some(vec![deposit(A, 3)]))) {
        Ok(()) => {
            // Preserving the old commitment is safe even if unrelated missing bytes are diagnosed later.
            assert!(index.page(query(B)).is_err());
            assert_eq!(index.page(query(A)).expect("untampered account").events[0].event, deposit(A, 3));
        }
        Err(_) => assert_eq!(index.head(), before),
    }
}

#[test]
fn physical_fixed_state_loss_fails_every_read_including_an_absent_account_and_retries() {
    let (mut index, path) = ranked_fixture("physical-state");
    let head = index.head();
    let state = index
        .store
        .read(&head.root, crate::history::authenticated::state::STATE_KEY)
        .expect("proved state")
        .expect("state");
    tamper_payload(&path, &state, None);
    assert!(index.page(query(A)).is_err());
    assert!(index.page(query(B)).is_err());
    assert!(index.ingest(command(5, 5, Some(vec![deposit(A, 1)]))).is_err());
    assert_eq!(index.head(), head);
}

#[test]
fn coherent_disk_rollback_cannot_reopen_a_certified_newer_root() {
    let (mut index, path) = fixture("physical-rollback", 1_000);
    index.ingest(command(0, 0, Some(vec![deposit(A, 1)]))).expect("first");
    let old = index.head();
    drop(index);
    let saved = path.with_extension("saved.sqlite");
    fs::copy(&path, &saved).expect("save clean closed packed store");
    index = Index::resume(open_writer(&path), old).expect("resume old head");
    index.ingest(command(1, 1, Some(vec![deposit(A, 2)]))).expect("newer");
    let current = index.head();
    drop(index);
    fs::copy(&saved, &path).expect("host rolls database back coherently");
    assert!(Index::resume(open_store(&path), current).is_err());
}
