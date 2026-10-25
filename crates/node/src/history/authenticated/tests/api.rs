//! Public API equivalence, cursor boundaries, and root pinning.

use super::*;

use crate::history::KINDS;
use crate::test_support::every_event;

#[test]
fn all_known_kinds_and_original_event_indices_match_the_input_prefix() {
    let (mut index, _) = fixture("all-kinds", 10_000);
    let commands = vec![
        command(0, 0, None),
        command(1, 100, Some(every_event())),
        command(2, 100, Some(vec![Event::AdminApplied, deposit(A, 2), deposit(B, 3), deposit(A, 4)])),
        command(3, 101, Some(vec![deposit(A, 5)])),
    ];
    for entry in &commands {
        index.ingest(entry.clone()).expect("ingest");
    }
    let mut accounts = vec![A, B, AccountId([0xff; 20])];
    accounts.extend(every_event().iter().filter_map(account_of));
    accounts.sort();
    accounts.dedup();
    for account in accounts {
        for kind in std::iter::once(None).chain(KINDS.into_iter().map(|kind| Some(kind.to_owned()))) {
            let mut request = query(account);
            request.kind = kind;
            let want = expected(&commands, &request, 10_000);
            assert_eq!(index.page(request).expect("proved page"), want);
        }
    }
    let events = index.page(query(A)).expect("A page").events;
    let cursors: Vec<_> = events.iter().map(|event| event.cursor).collect();
    assert!(cursors.contains(&Offset { seq: 2, index: 1 }));
    assert!(cursors.contains(&Offset { seq: 2, index: 3 }));
    assert!(!cursors.contains(&Offset { seq: 2, index: 0 }));
}

#[test]
fn exclusive_binary_search_bounds_and_lookahead_match_for_all_page_sizes() {
    let (mut index, _) = fixture("bounds", 10_000);
    let commands: Vec<_> = (0..73)
        .map(|seq| {
            command(
                seq,
                i64::try_from(seq).expect("fixture time"),
                Some(vec![deposit(A, 1), Event::AdminApplied, deposit(A, 2), deposit(B, 3), deposit(A, 4)]),
            )
        })
        .collect();
    for entry in &commands {
        index.ingest(entry.clone()).expect("ingest");
    }
    let cases = [
        (None, None),
        (Some(Offset { seq: u64::MAX, index: u32::MAX }), None),
        (Some(Offset { seq: 40, index: 2 }), Some(Offset { seq: 1, index: 0 })),
        (Some(Offset { seq: 40, index: 1 }), Some(Offset { seq: 1, index: 1 })),
        (Some(Offset { seq: 1, index: 0 }), Some(Offset { seq: 40, index: 2 })),
        (Some(Offset { seq: 40, index: 2 }), Some(Offset { seq: 40, index: 2 })),
        (None, Some(Offset { seq: u64::MAX, index: 0 })),
        (Some(Offset { seq: 0, index: 0 }), None),
    ];
    for kind in [None, Some("deposited".to_owned()), Some("financing".to_owned())] {
        for limit in [1, 50, 200] {
            for (before, after) in cases {
                let request = PageQuery { account: A, kind: kind.clone(), before, after, limit };
                let want = expected(&commands, &request, 10_000);
                assert_eq!(index.page(request).expect("proved bounded page"), want);
            }
        }
    }
    let first = index.page(PageQuery { limit: 200, ..query(A) }).expect("first page");
    assert_eq!(first.events.len(), 200);
    assert_eq!(first.next, first.events.last().map(|event| event.cursor));
    let final_page = index.page(PageQuery { before: first.next, limit: 200, ..query(A) }).expect("second page");
    assert_eq!(final_page.events.len(), 19);
    assert_eq!(final_page.next, None);
    assert!(first.events.last().expect("page").cursor > final_page.events[0].cursor);
}

#[test]
fn old_certified_root_survives_later_appends_and_is_never_reopened_at_latest() {
    let (mut index, path) = fixture("pinned", 1_000);
    let first = command(0, 0, Some(vec![deposit(A, 1)]));
    index.ingest(first.clone()).expect("first");
    let head = index.head();
    let want = expected(&[first], &query(A), 1_000);
    index.ingest(command(1, 2_000, Some(vec![deposit(B, 2)]))).expect("later expiry and append");
    assert!(index.page(query(A)).expect("new root").events.is_empty());
    drop(index);
    let old = Index::resume(open_store(&path), head.clone()).expect("resume pinned old root");
    assert_eq!(old.head(), head);
    assert_eq!(old.page(query(A)).expect("old verified page"), want);
}

#[test]
fn empty_generation_and_proven_kind_absence_return_exact_empty_pages() {
    let (mut index, path) = fixture("empty", 100);
    let empty_head = index.head();
    assert_eq!(empty_head, Head::empty(NAMESPACE, 100).expect("head"));
    assert_eq!(index.page(query(A)).expect("proved empty root"), Page { events: vec![], next: None });
    drop(index);
    index = Index::resume(open_writer(&path), empty_head).expect("resume canonical empty root");
    index.ingest(command(0, 0, Some(vec![deposit(A, 1)]))).expect("first command");
    for request in [query(B), PageQuery { kind: Some("fill".to_owned()), ..query(A) }] {
        assert_eq!(index.page(request).expect("proved metadata absence"), Page { events: vec![], next: None });
    }
    let head = index.head();
    let decoded: Head = serde_json::from_slice(&serde_json::to_vec(&head).expect("encode head")).expect("decode head");
    assert_eq!(decoded, head);
}

#[test]
fn invalid_limit_and_kind_are_rejected_without_a_partial_result() {
    let (index, _) = fixture("invalid-query", 100);
    for request in [
        PageQuery { limit: 0, ..query(A) },
        PageQuery { limit: 201, ..query(A) },
        PageQuery { limit: usize::MAX, ..query(A) },
        PageQuery { kind: Some("forged_kind".to_owned()), ..query(A) },
    ] {
        assert!(index.page(request).is_err());
    }
    assert!(Head::empty(NAMESPACE, 0).is_err());
    assert!(Head::empty(NAMESPACE, -1).is_err());
}
