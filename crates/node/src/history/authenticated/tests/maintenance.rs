//! Maintenance preserves prefix identity and actual blocking-reader leases outlive cancellation.

use super::*;

use crate::authenticated::COMPLETE_RECEIPT_BUDGET;

#[test]
fn maintenance_advances_only_the_storage_root_and_preserves_exact_prefix_and_pages() {
    let (mut index, _) = fixture("maintenance-prefix", 100);
    let commands = [
        command(0, 0, Some(vec![deposit(A, 1)])),
        command(1, 1, Some(vec![deposit(A, 2), deposit(B, 3)])),
        command(2, 2, None),
    ];
    for entry in &commands {
        index.ingest(entry.clone()).expect("ingest");
    }
    let before = index.head();
    let report = index.maintenance(COMPLETE_RECEIPT_BUDGET).expect("authenticated maintenance");
    let after = index.head();
    assert_eq!(after.state(), before.state());
    assert_eq!(after.command_prefix, prefix(&commands));
    assert_eq!(after.last_seq(), Some(2));
    assert_eq!(after.last_ts(), Some(2));
    if report.receipts_consumed != 0 {
        assert_ne!(after.root, before.root);
    }
    assert_all_pages(&index, &commands, A, None, 1);
    assert_all_pages(&index, &commands, B, Some("deposited"), 50);
    let next = command(3, 3, Some(vec![deposit(A, 4)]));
    index.ingest(next.clone()).expect("subsequent ingest must use maintenance output root");
    let mut all = commands.to_vec();
    all.push(next);
    assert_eq!(index.head().command_prefix, prefix(&all));
    assert_all_pages(&index, &all, A, None, 200);
}

#[test]
fn reopened_preflight_view_is_read_only_until_the_same_store_owner_is_authorized() {
    let (mut index, path) = fixture("preflight-resume", 100);
    let first = command(0, 0, Some(vec![deposit(A, 1)]));
    index.ingest(first.clone()).expect("seed");
    let head = index.head();
    drop(index);
    let store = open_store(&path);
    let pin = store.lease(&head.root).expect("restore certificate pin during preflight");
    let view = Index::resume(store.clone(), head.clone()).expect("read-only fixed metadata verification");
    assert_eq!(view.page(query(A)).expect("read-only page"), expected(std::slice::from_ref(&first), &query(A), 100));
    store.enable_writes().expect("parent authorizes same lineage after whole preflight");
    let mut live = Index::resume(store, head).expect("authorized live owner");
    let second = command(1, 1, None);
    live.ingest(second.clone()).expect("authorized write");
    assert_eq!(live.head().command_prefix, prefix(&[first, second]));
    drop(pin);
    drop(view);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_async_waiter_does_not_release_the_blocking_querys_old_root_pin() {
    let (mut index, _) = fixture("cancelled-history-reader", 1);
    let original = command(0, 0, Some(vec![large_event(A, 1024 * 1024 + 17)]));
    index.ingest(original.clone()).expect("original");
    let old_head = index.head();
    let pin = index.store.lease(&old_head.root).expect("pin moved into actual blocking closure");
    let store = index.store.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    let worker = tokio::task::spawn_blocking(move || {
        let _pin = pin;
        ready_tx.send(()).expect("worker started");
        go_rx.recv().expect("release actual blocking read");
        let reader = Index::resume(store, old_head).expect("worker still has a valid old pin");
        result_tx.send(reader.page(query(A))).expect("send worker result");
    });
    let waiter = tokio::spawn(worker);
    ready_rx.await.expect("worker owns pin");
    waiter.abort();
    index.ingest(command(1, 2, None)).expect("expire while old worker is still blocked");
    let report = index.maintenance(COMPLETE_RECEIPT_BUDGET).expect("maintenance respects blocking reader");
    assert!(report.blocked_by_pins);
    go_tx.send(()).expect("release worker after GC attempt");
    let page = result_rx.await.expect("cancelled waiter did not cancel blocking work").expect("proved old page");
    assert_eq!(page, expected(&[original], &query(A), 1));
    assert_eq!(index.page(query(A)).expect("new complete view"), Page { events: vec![], next: None });
}
