//! Large payloads remain exact and every scoped piece is required for returned and lookahead events.

use super::*;

use crate::history::authenticated::model::PAYLOAD_CHUNK_BYTES;
use crate::history::authenticated::state::STATE_KEY;

#[test]
fn event_larger_than_one_mib_has_bounded_progress_headers_and_exact_api_bytes() {
    let (mut index, _) = fixture("large-payload", 100);
    let entry = command(
        0,
        10,
        Some(vec![
            Event::AdminApplied,
            large_event(A, 2 * 1024 * 1024 + 19),
            deposit(B, 2),
            Event::Financing { account: A, market: 1, amount: Fixed::from_int(-1) },
        ]),
    );
    let before = index.head();
    let mut stages = 0;
    index
        .ingest_observed(entry.clone(), |visible, _, info| {
            stages += 1;
            assert_eq!(visible.head(), before);
            assert!(info.buffered_bytes < 8 * 1024 * 1024);
            Ok(())
        })
        .expect("large event through finite private chunks");
    assert!(stages > 0);
    assert_eq!(index.head().command_prefix, prefix(std::slice::from_ref(&entry)));
    for kind in [None, Some("deposit_rejected"), Some("financing"), Some("deposited")] {
        assert_all_pages(&index, std::slice::from_ref(&entry), A, kind, 1);
        assert_all_pages(&index, std::slice::from_ref(&entry), B, kind, 50);
    }
    let list = meta(&index, A, None);
    let row = stored_row(&index, list.position(0));
    assert_eq!(row.cursor, Offset { seq: 0, index: 1 });
    assert!(row.payload.length > 1024 * 1024);
    assert!(row.payload.chunks > 16);
    assert!(encoded(&row).expect("header bytes").len() < 1024);
    let state = index.store.read(&index.head.root, STATE_KEY).expect("proved state").expect("state");
    assert!(state.len() < 2048, "progress must not include full command/event bodies");
    for ordinal in 0..row.payload.chunks {
        let bytes = index.store.read(&index.head.root, &row.chunk_key(ordinal)).expect("piece proof").expect("piece");
        assert_eq!(bytes.len(), row.payload.chunk_len(ordinal).expect("expected piece length"));
        assert!(bytes.len() <= PAYLOAD_CHUNK_BYTES);
        assert!(!row.chunk_key(ordinal).starts_with(b"\0engine.authenticated/"));
    }
}

#[test]
fn same_tip_retry_commits_to_the_tail_of_a_large_payload_and_all_events() {
    let (mut index, _) = fixture("large-retry", 100);
    let mut entry = command(0, 0, Some(vec![large_event(A, 1024 * 1024 + 17)]));
    index.ingest(entry.clone()).expect("original large event");
    let head = index.head();
    index.ingest(entry.clone()).expect("exact large retry");
    assert_eq!(index.head(), head);
    let Event::DepositRejected { chain_ref, .. } = &mut entry.events.as_mut().expect("events")[0] else {
        panic!("fixture event");
    };
    chain_ref.push('z');
    assert_ne!(command_digest(&entry).expect("changed digest"), head.last_command.expect("identity").digest);
    assert!(index.ingest(entry).is_err());
    assert_eq!(index.head(), head);
}

#[test]
fn a_missing_payload_piece_fails_all_kind_specific_and_lookahead_pages() {
    for selected in [None, Some("deposit_rejected".to_owned())] {
        let (mut index, path) = fixture(&format!("missing-piece-{}", selected.is_some()), 100);
        index.ingest(command(0, 0, Some(vec![large_event(A, 1024 * 1024 + 123), deposit(A, 2)]))).expect("seed");
        let list = meta(&index, A, None);
        let large = stored_row(&index, list.position(0));
        let last = large.payload.chunks - 1;
        let bytes =
            index.store.read(&index.head.root, &large.chunk_key(last)).expect("piece proof").expect("last piece");
        tamper_payload(&path, &bytes, None);
        let head = index.head();
        assert!(index.page(PageQuery { kind: selected, limit: 1, ..query(A) }).is_err());
        assert_eq!(index.head(), head);
    }
}

#[test]
fn malformed_chunk_count_length_and_payload_account_kind_never_produce_partial_success() {
    for case in 0..4 {
        let (mut index, _) = fixture(&format!("payload-shape-{case}"), 100);
        index.ingest(command(0, 0, Some(vec![deposit(A, 1)]))).expect("seed");
        let list = meta(&index, A, None);
        let position = list.position(0);
        let mut row = stored_row(&index, position);
        match case {
            0 => row.payload.chunks += 1,
            1 => {
                let mut bytes =
                    index.store.read(&index.head.root, &row.chunk_key(0)).expect("piece proof").expect("piece");
                bytes.pop();
                rewrite(&mut index, vec![(row.chunk_key(0), Some(bytes))]);
            }
            2 => {
                rewrite(&mut index, vec![(row.chunk_key(0), encoded(&deposit(B, 1)))]);
            }
            _ => {
                let event = Event::BadDebt { account: A, amount: Fixed::from_int(1) };
                let bytes = encoded(&event).expect("different-kind event");
                row.payload.length = u64::try_from(bytes.len()).expect("length");
                row.payload.chunks = 1;
                rewrite(&mut index, vec![(row.chunk_key(0), Some(bytes))]);
            }
        }
        rewrite(&mut index, vec![(position.key(), encoded(&row))]);
        assert!(index.page(query(A)).is_err(), "payload fault {case}");
    }
}

#[test]
fn complete_expiry_removes_every_large_payload_piece_while_an_old_reader_keeps_exact_bytes() {
    let (mut index, _) = fixture("payload-expiry", 1);
    let initial = command(0, 0, Some(vec![large_event(A, 2 * 1024 * 1024 + 7)]));
    index.ingest(initial.clone()).expect("initial");
    let old_head = index.head();
    let old = Index::resume(index.store.clone(), old_head.clone()).expect("old reader with same-lineage pin");
    let list = meta(&index, A, None);
    let row = stored_row(&index, list.position(0));
    index.ingest(command(1, 2, None)).expect("complete large-payload expiry");
    assert_eq!((index.head().queue_lo, index.head().queue_hi), (1, 1));
    assert_eq!(index.page(query(A)).expect("current empty"), Page { events: vec![], next: None });
    for ordinal in 0..row.payload.chunks {
        assert!(
            index
                .store
                .read(&index.head.root, &row.chunk_key(ordinal))
                .expect("proved expired piece absence")
                .is_none()
        );
    }
    assert_eq!(old.head(), old_head);
    assert_eq!(old.page(query(A)).expect("old complete payload"), expected(&[initial], &query(A), 1));
}

#[test]
fn a_touched_missing_expiry_piece_blocks_publication_and_requires_verified_rebuild() {
    let (mut index, path) = fixture("payload-expiry-missing", 1);
    index.ingest(command(0, 0, Some(vec![large_event(A, 3 * 1024 * 1024 + 123)]))).expect("initial");
    let list = meta(&index, A, None);
    let row = stored_row(&index, list.position(0));
    let last = row.payload.chunks - 1;
    let bytes = index.store.read(&index.head.root, &row.chunk_key(last)).expect("piece proof").expect("piece");
    tamper_payload(&path, &bytes, None);
    let before = index.head();
    let mut commits = 0;
    assert!(
        index
            .ingest_observed(command(1, 2, None), |visible, _, _| {
                commits += 1;
                assert_eq!(visible.head(), before);
                Ok(())
            })
            .is_err()
    );
    assert!(commits > 0, "failure follows actual durable private expiry work");
    assert!(index.requires_rebuild());
    assert_eq!(index.head(), before);
    assert!(index.ingest(command(1, 2, None)).is_err());
}
