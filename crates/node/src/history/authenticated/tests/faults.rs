//! Semantic guard tests use deliberately inconsistent roots, not forged host proofs.

use super::*;

use crate::history::authenticated::model::{Queued, queue_key};
use crate::history::authenticated::state::STATE_KEY;

#[test]
fn missing_interior_lookahead_and_search_ranks_cannot_produce_short_success() {
    for (case, missing_rank, limit, before) in
        [("interior", 3, 5, None), ("lookahead", 4, 1, None), ("search", 3, 1, Some(Offset { seq: 5, index: 0 }))]
    {
        for selected in [None, Some("deposited".to_owned())] {
            let name = format!("missing-{case}-{}", selected.is_some());
            let (mut index, _) = ranked_fixture(&name);
            let kind = selected.as_deref().map(|_| event_kind(&deposit(A, 1)).expect("kind"));
            let meta = meta(&index, A, kind);
            rewrite(&mut index, vec![(meta.position(missing_rank).key(), None)]);
            assert!(index.page(PageQuery { kind: selected, before, limit, ..query(A) }).is_err(), "{case}");
        }
    }
}

#[test]
fn malformed_metadata_and_out_of_generation_counts_fail_closed() {
    for case in 0..3 {
        let (mut index, _) = ranked_fixture(&format!("metadata-{case}"));
        let mut value = meta(&index, A, None);
        match case {
            0 => value.hi = value.lo,
            1 => value.lo = value.hi + 1,
            _ => value.generation = index.head().next_generation,
        }
        rewrite(&mut index, vec![(ListId { account: A, kind: None }.key(), encoded(&value))]);
        assert!(index.page(query(A)).is_err(), "metadata fault {case}");
    }
}

#[test]
fn wrong_account_kind_cursor_order_and_time_cannot_be_served() {
    for case in 0..6 {
        let (mut index, _) = ranked_fixture(&format!("row-identity-{case}"));
        let list = meta(&index, A, None);
        let position = list.position(list.hi - 1);
        let mut value = stored_row(&index, position);
        match case {
            0 => value.account = B,
            1 => {
                rewrite(&mut index, vec![(value.chunk_key(0), encoded(&deposit(B, 1)))]);
            }
            2 => value.kind = 250,
            3 => value.cursor.seq = 100,
            4 => value.ts = -1,
            _ => value.cursor.seq = 4,
        }
        rewrite(&mut index, vec![(position.key(), encoded(&value))]);
        assert!(index.page(PageQuery { limit: 2, ..query(A) }).is_err(), "row fault {case}");
    }
}

#[test]
fn both_queue_references_are_verified_before_expiry_and_no_partial_change_is_committed() {
    for case in 0..5 {
        let (mut index, _) = ranked_fixture(&format!("queue-reference-{case}"));
        let bytes = index.store.read(&index.head.root, &queue_key(0)).expect("queue proof").expect("queue");
        let mut queued: Queued = serde_json::from_slice(&bytes).expect("queue payload");
        match case {
            0 => queued.all.rank += 1,
            1 => queued.kind.rank += 1,
            2 => queued.kind.generation = queued.all.generation,
            3 => queued.row.payload.length += 1,
            _ => {
                let second = index.store.read(&index.head.root, &queue_key(1)).expect("queue proof").expect("queue");
                queued = serde_json::from_slice(&second).expect("second queue payload");
            }
        }
        rewrite(&mut index, vec![(queue_key(0), encoded(&queued))]);
        let head = index.head();
        assert!(index.ingest(command(6, 2_000, None)).is_err(), "queue fault {case}");
        assert_eq!(index.head(), head);
        let list = meta(&index, A, None);
        assert_eq!((list.lo, list.hi), (0, 6));
        assert_eq!(stored_row(&index, list.position(0)).cursor, Offset { seq: 0, index: 0 });
    }
}

#[test]
fn missing_queue_item_and_missing_state_never_become_empty_history() {
    let (mut index, _) = ranked_fixture("missing-queue");
    rewrite(&mut index, vec![(queue_key(0), None)]);
    let head = index.head();
    assert!(index.ingest(command(6, 2_000, None)).is_err());
    assert_eq!(index.head(), head);
    rewrite(&mut index, vec![(STATE_KEY.to_vec(), None)]);
    assert!(index.page(query(A)).is_err());
    assert!(index.page(query(B)).is_err());
    assert!(index.ingest(command(6, 6, None)).is_err());
}

#[test]
fn resume_compares_all_fixed_state_fields_including_retention() {
    let (index, path) = ranked_fixture("resume-state");
    let head = index.head();
    drop(index);
    for case in 0..7 {
        let mut supplied = head.clone();
        match case {
            0 => supplied.retention_ms += 1,
            1 => supplied.queue_lo += 1,
            2 => supplied.queue_hi += 1,
            3 => supplied.next_generation += 1,
            4 => supplied.last_command = None,
            5 => supplied.last_command.as_mut().expect("tip").digest[0] ^= 1,
            _ => supplied.command_prefix[0] ^= 1,
        }
        assert!(Index::resume(open_store(&path), supplied).is_err(), "state field {case}");
    }
    let mut substituted = head.clone();
    substituted.root.namespace = [0x38; 32];
    assert!(Index::resume(open_store(&path), substituted).is_err());
    let resumed = Index::resume(open_store(&path), head.clone()).expect("unaltered head");
    assert_eq!(resumed.head(), head);
    assert_eq!(resumed.page(query(A)).expect("complete page").events.len(), 6);
}

#[test]
fn missing_nullable_certificate_fields_are_not_defaulted_to_authority() {
    let empty = Head::empty(NAMESPACE, 100).expect("empty head");
    let mut encoded = serde_json::to_value(empty).expect("encode");
    encoded.as_object_mut().expect("head object").remove("last_command");
    assert!(serde_json::from_value::<Head>(encoded).is_err());
    let mut encoded = serde_json::to_value(Head::empty(NAMESPACE, 100).expect("head")).expect("encode");
    encoded["root"].as_object_mut().expect("root object").remove("version");
    assert!(serde_json::from_value::<Head>(encoded).is_err());
    let mut encoded = serde_json::to_value(Head::empty(NAMESPACE, 100).expect("head")).expect("encode");
    encoded.as_object_mut().expect("head object").remove("command_prefix");
    assert!(serde_json::from_value::<Head>(encoded).is_err());
}

#[test]
fn rejected_store_batch_does_not_advance_the_in_memory_head_and_can_retry_cleanly() {
    let (mut index, path) = fixture("store-batch-failure", 100);
    let before = index.head();
    let entry = command(0, 0, Some((0..100).map(|_| deposit(A, 1)).collect()));
    let mut injected = false;
    assert!(
        index
            .ingest_observed(entry.clone(), |visible, root, _| {
                assert_eq!(visible.head(), before);
                if !injected {
                    let bytes = visible.store.read(root, STATE_KEY)?.expect("proved private state");
                    tamper_payload(&path, &bytes, None);
                    injected = true;
                }
                Ok(())
            })
            .is_err(),
        "the next actual fixed-state proof must reject deleted private state bytes"
    );
    assert!(injected);
    assert!(index.requires_rebuild());
    assert_eq!(index.head(), before);
    assert_eq!(index.page(query(A)).expect("unchanged complete empty root"), Page { events: vec![], next: None });
    assert!(index.ingest(entry.clone()).is_err(), "stale-root retries must be refused");
    let mut fresh = Index::empty(index.store.clone(), [0x38; 32], 100).expect("fresh verified generation");
    fresh.ingest(entry.clone()).expect("clean rebuild from verified input");
    assert_eq!(fresh.page(query(A)).expect("complete rebuild"), expected(&[entry], &query(A), 100));
}
