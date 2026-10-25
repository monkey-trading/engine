//! Damaged pages withhold the generation; busy storage and bad requests do not.

use super::*;

use std::fs;

use engine_core::Fixed;
use engine_core::command::{Command, Sequenced};
use engine_core::event::Event;
use engine_core::types::AccountId;
use rusqlite::Connection;

use crate::history::HistoryEvent;
use crate::log::{CommandEntry, Offset};
use crate::test_support::scratch_dir;

const A: AccountId = AccountId([1; 20]);
const B: AccountId = AccountId([2; 20]);

struct Fixture {
    store: Store,
    head: Head,
    reader: Reader,
    generation: Arc<Generation>,
    repair: mpsc::Receiver<StoreId>,
    path: PathBuf,
    events: Vec<Event>,
}

fn fixture(label: &str) -> Fixture {
    let dir = scratch_dir(label);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("history.sqlite");
    let store = Store::create(&path).unwrap();
    let id = StoreId([37; 32]);
    let mut index = Index::empty(store.clone(), id.history_namespace(), 1_000).unwrap();
    let events = vec![
        Event::Deposited { account: A, amount: Fixed::from_int(3) },
        Event::Deposited { account: B, amount: Fixed::from_int(7) },
    ];
    index
        .ingest(CommandEntry {
            seal: None,
            sequenced: Sequenced { seq: 0, ts: 10, cmd: Command::Tick },
            events: Some(events.clone()),
        })
        .unwrap();
    let head = index.head();
    let generation = Generation::new(id);
    let (_, ready) = watch::channel(true);
    let (_, published) = watch::channel(Some(Published::new(store.clone(), head.clone(), generation.clone()).unwrap()));
    let (repair_tx, repair) = mpsc::channel(1);
    let reader = Reader::authenticated(ready, published, repair_tx);
    Fixture { store, head, reader, generation, repair, path, events }
}

fn query(account: AccountId) -> PageQuery {
    PageQuery { account, kind: None, before: None, after: None, limit: 200 }
}

#[tokio::test]
async fn unread_payload_damage_is_diagnosed_by_first_dependent_query_then_withholds_the_generation() {
    let mut f = fixture("auth-reader-lazy-diagnosis");
    let bytes = serde_json::to_vec(&f.events[0]).unwrap();
    let changed =
        Connection::open(&f.path).unwrap().execute("DELETE FROM value_rows WHERE data = ?1", [bytes]).unwrap();
    assert!(changed > 0, "a real packed payload is unavailable");
    Index::resume(f.store.clone(), f.head.clone()).expect("fixed metadata readiness does not scan unread payloads");
    let other = f.reader.page(query(B)).await.unwrap().unwrap();
    assert_eq!(
        other.events,
        [HistoryEvent { cursor: Offset { seq: 0, index: 1 }, ts: 10, event: f.events[1].clone() }]
    );
    assert!(f.generation.valid());
    assert!(f.reader.page(query(A)).await.is_err(), "a missing proved payload cannot become a false-empty success");
    assert!(!f.generation.valid());
    assert_eq!(f.repair.try_recv().unwrap(), f.generation.store, "supervised owner receives a bounded repair wakeup");
    assert!(f.reader.page(query(B)).await.unwrap().is_none(), "every page waits for a fresh verified generation");
}

#[tokio::test]
async fn native_sqlite_busy_is_an_io_error_without_invalidating_or_requesting_repair() {
    let mut f = fixture("auth-reader-native-busy");
    let lock = Connection::open(&f.path).unwrap();
    lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let error = f.reader.page(query(A)).await.unwrap_err();
    assert!(transient(&error), "{error:#}");
    assert!(f.generation.valid(), "native contention is not proof of damaged events");
    assert!(f.repair.try_recv().is_err());
    lock.execute_batch("ROLLBACK").unwrap();
    let page = f.reader.page(query(A)).await.unwrap().unwrap();
    assert_eq!(page.events, [HistoryEvent { cursor: Offset::start_of(0), ts: 10, event: f.events[0].clone() }]);
}

#[tokio::test]
async fn a_store_the_process_cannot_open_is_transient_and_requests_no_repair() {
    use std::os::unix::fs::PermissionsExt;
    let mut f = fixture("auth-reader-cannot-open");
    let mode = std::fs::metadata(&f.path).unwrap().permissions();
    std::fs::set_permissions(&f.path, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(&f.path).is_ok() {
        // File permissions do not bind this user, so the open cannot be made to fail.
        std::fs::set_permissions(&f.path, mode).unwrap();
        return;
    }
    let error = f.reader.page(query(A)).await.unwrap_err();
    std::fs::set_permissions(&f.path, mode).unwrap();
    assert!(transient(&error), "{error:#}");
    assert!(f.generation.valid(), "a file the process cannot open is not proof of damaged events");
    assert!(f.repair.try_recv().is_err());
}

/// Memory the process cannot get, for a returned value or inside SQLite, is transient like an
/// unopenable file; damaged data is not.
#[test]
fn allocation_and_open_failures_are_transient_and_damage_is_not() {
    let reserve = Vec::<u8>::new().try_reserve_exact(usize::MAX).unwrap_err();
    assert!(transient(&anyhow::Error::from(reserve).context("allocating returned history event")));
    let sqlite = |code| rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
    assert!(transient(&anyhow::Error::from(sqlite(rusqlite::ffi::SQLITE_NOMEM))));
    assert!(!transient(&anyhow::Error::from(sqlite(rusqlite::ffi::SQLITE_CORRUPT))));
    assert!(!transient(&anyhow::anyhow!("missing history payload chunk")));
}

#[tokio::test]
async fn bad_caller_bounds_and_kinds_do_not_invalidate_verified_history() {
    let mut f = fixture("auth-reader-invalid-query");
    for invalid in [PageQuery { limit: 0, ..query(A) }, PageQuery { kind: Some("typo".into()), ..query(A) }] {
        assert!(f.reader.page(invalid).await.is_err());
        assert!(f.generation.valid());
        assert!(f.repair.try_recv().is_err());
    }
    assert_eq!(f.reader.page(query(A)).await.unwrap().unwrap().events.len(), 1);
}
