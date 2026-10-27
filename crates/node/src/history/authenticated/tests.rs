//! Authenticated history tests against the real packed Store.

use std::fs;
use std::path::{Path, PathBuf};

use engine_core::Fixed;
use engine_core::command::{Command, Sequenced};
use engine_core::event::Event;
use engine_core::types::{AccountId, Millis};
use serde::Serialize;

use crate::authenticated::Store;
use crate::history::authenticated::blobs::read_event;
use crate::history::authenticated::model::{ListId, Metadata, Position, Row, event_kind};
use crate::history::authenticated::state::CommandId;
use crate::history::authenticated::{Head, Index};
use crate::history::{HistoryEvent, Page, PageQuery, account_of};
use crate::log::{CommandEntry, Offset, command_digest, extend_command_prefix};
use crate::test_support::scratch_dir;

mod api;
mod batch;
mod faults;
mod maintenance;
mod payloads;
mod physical;
mod retention;
mod staged;

const A: AccountId = AccountId([0xa; 20]);
const B: AccountId = AccountId([0xb; 20]);
const NAMESPACE: [u8; 32] = [0x37; 32];

/// Reopens the real packed Store without inventing a root from disk.
fn open_store(path: &Path) -> Store {
    Store::open_existing(path).expect("open actual packed authenticated store")
}

/// Creates private test storage without sharing a namespace or process-global mutable state.
fn fixture(name: &str, retention_ms: Millis) -> (Index, PathBuf) {
    let dir = scratch_dir(&format!("history-auth-{name}"));
    fs::create_dir_all(&dir).expect("private test directory");
    let path = dir.join("authenticated.sqlite");
    let store = Store::create(&path).expect("create actual packed authenticated store");
    (Index::empty(store, NAMESPACE, retention_ms).expect("empty index"), path)
}

/// Constructs the verified-input boundary's sequenced fixture.
fn command(seq: u64, ts: Millis, events: Option<Vec<Event>>) -> CommandEntry {
    CommandEntry { seal: None, sequenced: Sequenced { seq, ts, cmd: Command::Tick }, events }
}

/// Constructs a small typed account event with a distinct amount.
fn deposit(account: AccountId, n: i64) -> Event {
    Event::Deposited { account, amount: Fixed::from_int(n) }
}

/// Constructs an account query at the API maximum before individual tests add bounds/filters.
fn query(account: AccountId) -> PageQuery {
    PageQuery { account, kind: None, before: None, after: None, limit: 200 }
}

/// Independently derives the public result from original input, including original event indices.
fn expected(commands: &[CommandEntry], query: &PageQuery, retention_ms: Millis) -> Page {
    let horizon = commands.last().map_or(i64::MIN, |last| last.sequenced.ts.saturating_sub(retention_ms));
    let mut events: Vec<HistoryEvent> = commands
        .iter()
        .flat_map(|command| {
            command.events.iter().flatten().enumerate().filter_map(move |(index, event)| {
                let cursor = Offset { seq: command.sequenced.seq, index: u32::try_from(index).expect("fixture index") };
                let kind =
                    serde_json::to_value(event).expect("fixture event")["type"].as_str().expect("kind").to_owned();
                (account_of(event) == Some(query.account)
                    && command.sequenced.ts >= horizon
                    && query.kind.as_ref().is_none_or(|wanted| *wanted == kind)
                    && query.before.is_none_or(|before| cursor < before)
                    && query.after.is_none_or(|after| cursor > after))
                .then(|| HistoryEvent { cursor, ts: command.sequenced.ts, event: event.clone() })
            })
        })
        .collect();
    events.reverse();
    let next = if events.len() > query.limit {
        events.truncate(query.limit);
        events.last().map(|event| event.cursor)
    } else {
        None
    };
    Page { events, next }
}

/// Proves and decodes a fixture list's metadata using the actual Store boundary.
fn meta(index: &Index, account: AccountId, kind: Option<u8>) -> Metadata {
    let bytes = index
        .store
        .read(&index.head.root, &ListId { account, kind }.key())
        .expect("proved metadata")
        .expect("fixture populated list");
    serde_json::from_slice(&bytes).expect("fixture metadata")
}

/// Proves and decodes one fixture rank.
fn stored_row(index: &Index, position: Position) -> Row {
    let bytes = index.store.read(&index.head.root, &position.key()).expect("proved rank").expect("fixture rank");
    serde_json::from_slice(&bytes).expect("fixture row")
}

/// Encodes a semantic fault without inventing cryptographic proof bytes.
fn encoded<T: Serialize>(value: &T) -> Option<Vec<u8>> {
    Some(serde_json::to_vec(value).expect("encode fixture mutation"))
}

/// Manufactures a logically inconsistent root to isolate semantic guards, not host-proof soundness.
fn rewrite(index: &mut Index, changes: Vec<(Vec<u8>, Option<Vec<u8>>)>) {
    let root = index.store.update(&index.head.root, changes).expect("construct fixture fault root");
    index._visible_pin = index.store.lease(&root).expect("pin fixture fault root");
    index.head.root = root;
}

/// Uses a populated single-kind list with exact known ranks for fault targeting.
fn ranked_fixture(name: &str) -> (Index, PathBuf) {
    let (mut index, path) = fixture(name, 1_000);
    for seq in 0..6 {
        index
            .ingest(command(seq, i64::try_from(seq).expect("fixture time"), Some(vec![deposit(A, 1)])))
            .expect("ingest fixture");
    }
    (index, path)
}

/// Rewrites or deletes exact payloads in the Store's `value_rows` table.
/// This independent host connection supplies fault injection only, never history authority.
fn tamper_payload(path: &Path, original: &[u8], replacement: Option<&[u8]>) {
    let conn = rusqlite::Connection::open(path).expect("host connection to private fixture");
    let changed = match replacement {
        Some(bytes) => {
            conn.execute("UPDATE value_rows SET data = ?1 WHERE data = ?2", rusqlite::params![bytes, original])
        }
        None => conn.execute("DELETE FROM value_rows WHERE data = ?1", [original]),
    }
    .expect("inject packed payload fault");
    assert!(changed > 0, "fault must alter actual packed payload bytes");
}

/// Opens a writer only after the fixture's explicit simulated preflight.
fn open_writer(path: &Path) -> Store {
    let store = open_store(path);
    store.enable_writes().expect("authorize fixture writes after preflight");
    store
}

/// Independently folds the parent-shared command-prefix chain over the full fixture input.
fn prefix(commands: &[CommandEntry]) -> [u8; 32] {
    commands.iter().fold([0; 32], |previous, command| {
        extend_command_prefix(previous, command_digest(command).expect("fixture command identity"))
    })
}

/// Reads the actual event body under this Index's exact trusted root.
fn stored_event(index: &Index, row: &Row) -> Event {
    read_event(&index.store, &index.head.root, row).expect("proved complete fixture event")
}

/// Checks every page and the exact accumulated output against original authenticated fixtures.
fn assert_all_pages(index: &Index, commands: &[CommandEntry], account: AccountId, kind: Option<&str>, limit: usize) {
    let mut request = PageQuery { account, kind: kind.map(str::to_owned), limit: usize::MAX, ..query(account) };
    let all = expected(commands, &request, index.head().retention_ms).events;
    request.limit = limit;
    let mut actual = Vec::new();
    let mut pages = 0;
    loop {
        pages += 1;
        assert!(pages <= all.len() + 1, "pagination must make strict progress");
        let want = expected(commands, &request, index.head().retention_ms);
        let page = index
            .page(PageQuery {
                account,
                kind: request.kind.clone(),
                before: request.before,
                after: request.after,
                limit,
            })
            .expect("proved complete page");
        assert_eq!(page, want);
        request.before = page.next;
        actual.extend(page.events);
        if request.before.is_none() {
            break;
        }
    }
    assert_eq!(actual, all);
}

/// A valid account event whose unbounded chain_ref crosses physical value and staging envelopes.
fn large_event(account: AccountId, bytes: usize) -> Event {
    Event::DepositRejected {
        account,
        amount: Fixed::from_int(1),
        chain_ref: format!("begin-{}-end", "x".repeat(bytes)),
        reason: engine_core::event::Reject::DuplicateReference,
    }
}

mod generation_lifecycle;
