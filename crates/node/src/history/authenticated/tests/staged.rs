//! Complete-publication and finite-staging tests use the real file-backed proof store.

use super::*;

use crate::history::KINDS;
use crate::history::authenticated::model::PAYLOAD_CHUNK_BYTES;
use crate::history::authenticated::staging::STAGED_KEYS;
use crate::history::authenticated::state::{STATE_KEY, State};

/// Mixes accounts and kinds while preserving non-account gaps in the original cursor indices.
fn fanout(seq: u64, ts: Millis, count: usize) -> CommandEntry {
    let events = (0..count)
        .map(|index| match index % 5 {
            0 => deposit(A, 1),
            1 => deposit(B, 2),
            2 => Event::Financing { account: A, market: 1, amount: Fixed::from_int(-1) },
            3 => Event::BadDebt { account: B, amount: Fixed::from_int(1) },
            _ => Event::AdminApplied,
        })
        .collect();
    command(seq, ts, Some(events))
}

/// Builds an attempted Head matching every pending field; only its complete marker is unavailable.
fn pending_candidate(root: &crate::authenticated::Root, state: &State) -> Head {
    Head {
        root: root.clone(),
        last_command: state.last_command,
        command_prefix: state.command_prefix,
        queue_lo: state.queue_lo,
        queue_hi: state.queue_hi,
        next_generation: state.next_generation,
        retention_ms: state.retention_ms,
    }
}

#[test]
fn thousand_event_command_uses_bounded_private_batches_and_publishes_only_complete_pages() {
    let (mut index, _) = fixture("thousand-fanout", 1_000);
    let entry = fanout(0, 0, 1_257);
    let before = index.head();
    let mut commits = 0;
    index
        .ingest_observed(entry.clone(), |visible, root, info| {
            commits += 1;
            assert_eq!(visible.head(), before, "head() cannot acknowledge partial fanout");
            assert_eq!(visible.page(query(A)).expect("old public view"), Page { events: vec![], next: None });
            assert!(info.keys <= STAGED_KEYS + 16);
            assert!(info.buffered_bytes <= (STAGED_KEYS + 16) * PAYLOAD_CHUNK_BYTES * 4);
            let bytes = visible.store.read(root, STATE_KEY)?.expect("private state");
            let state: State = serde_json::from_slice(&bytes)?;
            assert!(!state.complete);
            assert_eq!(state.command_prefix, prefix(std::slice::from_ref(&entry)));
            let candidate = pending_candidate(root, &state);
            let err = Index::resume(visible.store.clone(), candidate).err().expect("pending root refused");
            assert!(format!("{err:#}").contains("pending"));
            Ok(())
        })
        .expect("whole large command");
    assert!(commits > 20);
    assert_eq!(index.head().last_command, Some(CommandId::of(&entry).expect("identity")));
    assert_eq!(index.head().command_prefix, prefix(std::slice::from_ref(&entry)));
    assert_eq!(index.head().queue_hi, 1_006);
    for account in [A, B, AccountId([0xee; 20])] {
        for kind in std::iter::once(None).chain(KINDS.iter().copied().map(Some)) {
            assert_all_pages(&index, std::slice::from_ref(&entry), account, kind, 50);
            assert_all_pages(&index, std::slice::from_ref(&entry), account, kind, 200);
        }
    }
}

#[test]
fn thousand_event_expiry_on_no_events_keeps_equality_then_expires_every_row_and_kind() {
    let (mut index, _) = fixture("thousand-expiry", 100);
    let initial = fanout(0, 0, 1_257);
    let equal = command(1, 100, None);
    let expired = command(2, 101, None);
    index.ingest(initial.clone()).expect("fanout");
    index.ingest(equal.clone()).expect("horizon equality on legacy-none command");
    for account in [A, B] {
        assert_all_pages(&index, &[initial.clone(), equal.clone()], account, None, 200);
    }
    let before = index.head();
    let mut commits = 0;
    index
        .ingest_observed(expired.clone(), |visible, _, info| {
            commits += 1;
            assert_eq!(visible.head(), before);
            assert!(info.keys <= STAGED_KEYS + 16);
            Ok(())
        })
        .expect("all exact expiry pieces");
    assert!(commits > 20);
    assert_eq!((index.head().queue_lo, index.head().queue_hi), (1_006, 1_006));
    assert_eq!(index.head().command_prefix, prefix(&[initial, equal, expired]));
    for account in [A, B] {
        for kind in std::iter::once(None).chain(KINDS.iter().copied().map(Some)) {
            assert_eq!(
                index.page(PageQuery { kind: kind.map(str::to_owned), ..query(account) }).expect("empty"),
                Page { events: vec![], next: None }
            );
        }
    }
}

#[test]
fn midstage_append_failure_leaves_old_complete_head_pinned_and_refuses_any_stale_retry() {
    let (mut index, _) = fixture("midstage-append", 100);
    let original = command(0, 0, Some(vec![deposit(A, 7)]));
    index.ingest(original.clone()).expect("seed");
    let before = index.head();
    let entry = fanout(1, 1, 1_257);
    let mut stages = 0;
    let mut failed_root = None;
    assert!(
        index
            .ingest_observed(entry.clone(), |visible, root, _| {
                stages += 1;
                assert_eq!(visible.head(), before);
                assert_eq!(visible.page(query(A))?, expected(std::slice::from_ref(&original), &query(A), 100));
                if stages == 3 {
                    failed_root = Some(root.clone());
                    anyhow::bail!("injected failure after three real durable private commits");
                }
                Ok(())
            })
            .is_err()
    );
    assert_eq!(stages, 3);
    assert_eq!(index.head(), before);
    assert!(index.requires_rebuild());
    assert!(index.ingest(entry).is_err());
    assert!(index.ingest(original.clone()).is_err(), "no retry acknowledgements from a failed generation");
    assert!(index.maintenance(crate::authenticated::COMPLETE_RECEIPT_BUDGET).is_err());
    let root = failed_root.expect("last acknowledged private root");
    let _ =
        index.store.gc(&root, crate::authenticated::COMPLETE_RECEIPT_BUDGET).expect("authenticated test maintenance");
    assert_eq!(index.page(query(A)).expect("old pin survives maintenance"), expected(&[original], &query(A), 100));
}

#[test]
fn mid_payload_failure_never_exposes_a_header_for_an_incomplete_event() {
    let (mut index, _) = fixture("mid-payload", 100);
    let before = index.head();
    let entry = command(0, 0, Some(vec![large_event(A, 6 * 1024 * 1024 + 31)]));
    let mut commits = 0;
    assert!(
        index
            .ingest_observed(entry.clone(), |visible, _, info| {
                commits += 1;
                assert_eq!(visible.head(), before);
                assert_eq!(visible.page(query(A))?, Page { events: vec![], next: None });
                assert!(info.buffered_bytes < 8 * 1024 * 1024);
                if commits == 2 {
                    anyhow::bail!("payload-stage failure");
                }
                Ok(())
            })
            .is_err()
    );
    assert_eq!(commits, 2);
    assert!(index.requires_rebuild());
    assert_eq!(index.head(), before);
    let mut fresh = Index::empty(index.store.clone(), [0x38; 32], 100).expect("fresh verified namespace");
    fresh.ingest(entry.clone()).expect("full verified rebuild");
    assert_eq!(fresh.page(query(A)).expect("complete large body"), expected(&[entry], &query(A), 100));
}

#[test]
fn pending_roots_and_stale_public_heads_never_become_recovery_authority() {
    let (mut index, _) = fixture("stale-public", 100);
    let first = command(0, 0, Some(vec![deposit(A, 1)]));
    index.ingest(first.clone()).expect("first");
    let mut old = Index::resume(index.store.clone(), index.head()).expect("retained complete reader");
    let second = command(1, 1, Some(vec![deposit(B, 2)]));
    index.ingest(second.clone()).expect("latest complete H");
    assert!(old.ingest(second).is_err(), "generic fork from an older C root is not supported");
    assert!(old.requires_rebuild());
    assert_eq!(old.page(query(A)).expect("old complete reads remain valid"), expected(&[first], &query(A), 100));
    assert_eq!(index.head().last_seq(), Some(1));
}
