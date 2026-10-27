//! A failed first ingest leaves the generation for replacement while old readers finish.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;

use anyhow::Result;

use super::{A, command, deposit, expected, query};
use crate::history::authenticated::Index;
use crate::history::authenticated::state::{CommandId, STATE_KEY, State};
use crate::snapshot::generations::{Allocate, Manager, VerifiedReferences};
use crate::snapshot::head::StoreId;
use crate::snapshot::pins::Pins;

#[test]
fn failed_first_private_ingest_replaces_live_actor_owners_and_preserves_cancelled_actual_reader() -> Result<()> {
    let dir = LifecycleDir::new()?;
    let mut manager = Manager::initialize_new_after_preflight(&dir.path)?;
    // No first signed complete H exists. The old public view is the complete virtual empty head.
    let references = Rc::new(RefCell::new(BTreeSet::<StoreId>::new()));
    let mut verify =
        |_locators: &[StoreId]| Ok(VerifiedReferences::from_verified_certificates(references.borrow().iter().copied()));
    let old = manager.allocate(Allocate::History, &mut verify)?.expect("first generation");
    let old = old.history.expect("history");
    let old_id = old.id;
    let actor_store = old.store.clone();
    let actual_lifetime = actor_store.lifetime();
    let mut actor_index = Index::empty(actor_store.clone(), old_id.history_namespace(), 1_000)?;
    let virtual_head = actor_index.head();
    let mut actor_pins = Pins::new(old_id, actor_store.clone(), Default::default());
    actor_pins.protect(&virtual_head.root)?;
    let lease = actor_store.lease(&virtual_head.root)?;
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let (producer, waiter) = mpsc::channel();
    let worker = thread::spawn(move || -> Result<Option<Vec<u8>>> {
        admitted_tx.send(())?;
        finish_rx.recv()?;
        let value = lease.read(STATE_KEY)?;
        drop(lease);
        // A cancelled receiver does not cancel the actual blocking read or own its Lease.
        let _ = producer.send(value.clone());
        Ok(value)
    });
    admitted_rx.recv()?;

    let entry = command(0, 1, Some((0..96).map(|_| deposit(A, 1)).collect()));
    let mut private_commits = 0;
    assert!(
        actor_index
            .ingest_observed(entry.clone(), |visible, root, _| {
                private_commits += 1;
                assert_eq!(visible.head(), virtual_head);
                assert!(root.version.is_some(), "failure follows a real acknowledged private store commit");
                let bytes = actor_store.read(root, STATE_KEY)?.expect("proved private state");
                let pending: State = serde_json::from_slice(&bytes)?;
                assert!(!pending.complete);
                assert!(pending.head(root.clone()).is_err(), "a partial stage cannot become complete H");
                anyhow::bail!("first command failed after its first durable private stage")
            })
            .is_err()
    );
    assert_eq!(private_commits, 1);
    assert!(actor_index.requires_rebuild());
    assert_eq!(actor_index.head(), virtual_head);
    assert_eq!(virtual_head.last_seq(), None);
    assert_eq!(virtual_head.root.version, None);
    assert!(references.borrow().is_empty(), "no failed/private head was published");

    // The factory runs before Actor replaces its old Index/Store/Pins; none may be dropped for space.
    let replacement = manager.allocate(Allocate::Pair, &mut verify)?.expect("replacement despite old owners");
    let history = replacement.history.as_ref().expect("new history");
    let mut repaired = Index::empty(history.store.clone(), history.id.history_namespace(), 1_000)?;
    assert_eq!(repaired.head().last_seq(), virtual_head.last_seq());
    assert_eq!(repaired.head().command_prefix, virtual_head.command_prefix);
    assert!(actual_lifetime.alive());
    repaired.ingest(entry.clone())?;
    assert_eq!(repaired.head().last_command, Some(CommandId::of(&entry)?));
    assert_eq!(repaired.page(query(A))?, expected(&[entry], &query(A), 1_000));
    assert_eq!(actor_index.head(), virtual_head, "replacement cannot acknowledge the failed old head");
    assert_eq!(database_count(&dir)?, 3);
    for _ in 0..4 {
        assert!(manager.allocate(Allocate::Pair, &mut verify)?.is_none());
        assert_eq!(database_count(&dir)?, 3);
    }

    drop((old, actor_store, actor_index, actor_pins, waiter));
    assert!(actual_lifetime.alive(), "only the actual cancelled worker still owns the old Store");
    for _ in 0..4 {
        let report = manager.reconcile(&mut verify)?;
        assert!(report.waiting_for_readers);
        assert_eq!(report.deleted, 0);
        assert!(old_id.path(&dir.path).is_file());
        assert!(manager.allocate(Allocate::Pair, &mut verify)?.is_none());
        assert_eq!(database_count(&dir)?, 3);
    }
    finish_tx.send(())?;
    assert_eq!(worker.join().expect("actual worker completion")?, None, "the old virtual head proves absence");
    assert!(!actual_lifetime.alive());
    assert_eq!(manager.reconcile(&mut verify)?.deleted, 1);
    assert_eq!(manager.reconcile(&mut verify)?.deleted, 0, "cleanup is idempotent");
    assert!(!old_id.path(&dir.path).exists());
    let later = manager.allocate(Allocate::History, &mut verify)?.expect("later bounded progress");
    assert!(later.history.is_some());
    assert!(database_count(&dir)? <= 4);
    drop((later, repaired, replacement));
    Ok(())
}

struct LifecycleDir {
    path: PathBuf,
}

impl LifecycleDir {
    /// Creates private storage for this cross-module real-file case.
    /// Error cases:
    /// - test directory creation fails
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("engine-first-ingest-lifecycle-{:032x}", rand::random::<u128>()));
        fs::create_dir(&path)?;
        Ok(Self { path })
    }
}

impl Drop for LifecycleDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Measures actual retry file growth independently of the fixed journal's slot count.
/// Error cases:
/// - test directory enumeration fails
fn database_count(dir: &LifecycleDir) -> Result<usize> {
    fs::read_dir(&dir.path)?
        .try_fold(0, |count, entry| Ok(count + usize::from(entry?.file_name().to_string_lossy().ends_with(".sqlite"))))
}
