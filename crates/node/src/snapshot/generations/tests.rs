//! Real-file lifecycle cases; the reference callback represents the parent's verified certificate set.

use super::*;
use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;

use crate::authenticated::Root;

#[test]
fn abandoned_pair_is_reclaimed_after_restart_before_another_attempt() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let attempt = manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.expect("allocation");
    let history = attempt.history.expect("history");
    let catalog = attempt.catalog.expect("catalog");
    assert_ne!(history.id, catalog.id);
    let old = [history.id, catalog.id];
    drop((history, catalog, manager));
    let mut manager = Manager::open_after_preflight(&dir.path, [])?;
    let next = allocate_history(&mut manager, &refs)?;
    assert!(old.iter().all(|id| !id.path(&dir.path).exists()));
    assert_eq!(database_count(&dir)?, 1);
    assert!(next.id.path(&dir.path).is_file());
    Ok(())
}

#[test]
fn retirement_intent_survives_last_certificate_prune_and_restart() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let old = allocate_history(&mut manager, &refs)?;
    let old_id = old.id;
    let root = put(&old, b"old")?;
    refs.borrow_mut().insert(old_id);
    manager.reconcile(&mut verifier(refs.clone()))?;
    let new = allocate_history(&mut manager, &refs)?;
    let new_id = new.id;
    put(&new, b"new")?;
    // Emulate termination after last-reference unlink, before in-process cleanup.
    let changed = refs.clone();
    let lifecycle_directory = dir.path.clone();
    assert!(
        manager
            .change_references([Some(old_id), None], &mut verifier(refs.clone()), move || {
                let durable_intent = Disk::new(&lifecycle_directory)?.load()?;
                assert_eq!(durable_intent.retiring[0], Some(old_id));
                changed.borrow_mut().insert(new_id);
                changed.borrow_mut().remove(&old_id);
                anyhow::bail!("interrupted after durable publication and prune")
            })
            .is_err()
    );
    drop((old, new, manager));
    let mut manager = Manager::open_after_preflight(&dir.path, [])?;
    let report = manager.reconcile(&mut verifier(refs.clone()))?;
    assert_eq!(report.deleted, 1);
    assert!(!old_id.path(&dir.path).exists());
    assert!(new_id.path(&dir.path).is_file());
    assert_ne!(root.namespace, new_id.history_namespace());
    assert!(manager.disk.load()?.is_empty());
    Ok(())
}

#[test]
fn forged_journal_cannot_remove_a_retained_generation_without_live_owners() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    let root = put(&generation, b"certified")?;
    refs.borrow_mut().insert(id);
    drop(generation);
    // Host-controlled, syntactically valid retirement contents carry no deletion authority.
    manager.disk.save(&State { staging: [None, None], retiring: [Some(id), None] })?;
    let report = manager.reconcile(&mut verifier(refs.clone()))?;
    assert_eq!(report.retained, 1);
    assert_eq!(report.deleted, 0);
    let reopened = Store::open_existing(id.path(&dir.path))?;
    assert_eq!(reopened.read(&root, b"key")?, Some(b"certified".to_vec()));
    Ok(())
}

#[test]
fn actual_cancelled_worker_lease_preserves_data_and_bounds_replacement_allocation() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    let root = put(&generation, b"reader")?;
    refs.borrow_mut().insert(id);
    manager.reconcile(&mut verifier(refs.clone()))?;
    let lease = generation.store.lease(&root)?;
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let (finish_tx, finish_rx) = mpsc::channel();
    let worker = thread::spawn(move || -> Result<Option<Vec<u8>>> {
        admitted_tx.send(())?;
        finish_rx.recv()?;
        lease.read(b"key")
    });
    admitted_rx.recv()?;
    drop(generation);
    let refs_for_change = refs.clone();
    let report = manager.change_references([Some(id), None], &mut verifier(refs.clone()), move || {
        refs_for_change.borrow_mut().remove(&id);
        Ok(())
    })?;
    assert!(report.waiting_for_readers);
    let replacement = manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.expect("one finite replacement");
    assert_eq!(database_count(&dir)?, 3);
    for _ in 0..3 {
        assert!(manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.is_none());
        assert_eq!(database_count(&dir)?, 3);
        assert!(id.path(&dir.path).is_file(), "cancelled waiter must not retire the actual reader");
    }
    finish_tx.send(())?;
    assert_eq!(worker.join().expect("reader worker")?, Some(b"reader".to_vec()));
    let next = allocate_history(&mut manager, &refs)?;
    assert!(!id.path(&dir.path).exists());
    assert_ne!(next.id, id);
    drop(replacement);
    Ok(())
}

#[test]
fn every_store_clone_and_lease_clone_delays_retirement() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    let root = put(&generation, b"clones")?;
    let store_clone = generation.store.clone();
    let lease = generation.store.lease(&root)?;
    let lease_clone = lease.clone();
    drop((generation, lease));
    assert!(manager.reconcile(&mut verifier(refs.clone()))?.waiting_for_readers);
    drop(store_clone);
    assert!(manager.reconcile(&mut verifier(refs.clone()))?.waiting_for_readers);
    assert_eq!(lease_clone.read(b"key")?, Some(b"clones".to_vec()));
    drop(lease_clone);
    assert_eq!(manager.reconcile(&mut verifier(refs.clone()))?.deleted, 1);
    assert!(!id.path(&dir.path).exists());
    Ok(())
}

#[test]
fn restored_preflight_owner_protects_an_uncertified_live_root() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    let root = put(&generation, b"preflight")?;
    drop((generation, manager));
    let store = Store::open_existing(id.path(&dir.path))?;
    let lease = store.lease(&root)?;
    let mut manager = Manager::open_after_preflight(&dir.path, [(id, store.lifetime())])?;
    drop(store);
    let next = manager.allocate(Allocate::History, &mut verifier(refs.clone()))?.expect("bounded replacement");
    assert!(id.path(&dir.path).is_file());
    assert_eq!(lease.read(b"key")?, Some(b"preflight".to_vec()));
    drop(lease);
    manager.reconcile(&mut verifier(refs.clone()))?;
    assert!(next.history.is_some());
    assert!(next.catalog.is_none());
    assert!(!id.path(&dir.path).exists());
    Ok(())
}

#[test]
fn first_private_stage_failure_can_replace_an_owner_held_uncertified_generation() -> Result<()> {
    use crate::history::authenticated::Index;
    use crate::log::CommandEntry;
    use engine_core::command::{Command, Sequenced};

    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let original = manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.expect("original pair");
    let history = original.history.expect("history");
    let catalog = original.catalog.expect("catalog");
    let mut index = Index::empty(history.store.clone(), history.id.history_namespace(), 1000)?;
    let old_head = index.head();
    let private =
        history.store.update(&old_head.root, vec![(b"private-stage".to_vec(), Some(b"unacknowledged".to_vec()))])?;
    let tick = CommandEntry { sequenced: Sequenced { seq: 0, ts: 0, cmd: Command::Tick }, events: Some(Vec::new()) };
    assert!(index.ingest(tick).is_err(), "an unacknowledged native version requires fresh repair");
    assert!(index.requires_rebuild());
    assert_eq!(index.head(), old_head, "private failure must leave the complete public head unchanged");
    let replacement = allocate_history(&mut manager, &refs)?;
    assert_ne!(replacement.id, history.id);
    assert_eq!(database_count(&dir)?, 3);
    assert_eq!(history.store.read(&private, b"private-stage")?, Some(b"unacknowledged".to_vec()));
    for _ in 0..10 {
        assert!(manager.allocate(Allocate::History, &mut verifier(refs.clone()))?.is_none());
        assert_eq!(database_count(&dir)?, 3, "the four pending slots cannot grow with failed retries");
    }
    drop((index, history, catalog));
    assert_eq!(manager.reconcile(&mut verifier(refs.clone()))?.deleted, 2);
    assert!(replacement.id.path(&dir.path).is_file());
    Ok(())
}

#[test]
fn retained_root_leases_are_restored_on_the_same_owner_before_utility_gc() -> Result<()> {
    use crate::authenticated::COMPLETE_RECEIPT_BUDGET;

    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    let paired_root = put(&generation, b"paired")?;
    let head_root = generation.store.update(&paired_root, vec![(b"key".to_vec(), Some(b"head".to_vec()))])?;
    refs.borrow_mut().insert(id);
    manager.reconcile(&mut verifier(refs.clone()))?;
    drop((generation, manager));
    // These roots are supplied by verified retained certificates, never a SQL "latest" query.
    let store = Store::open_existing(id.path(&dir.path))?;
    let pins = store.pin_versions(&[paired_root.clone(), head_root.clone()])?;
    let mut manager = Manager::open_after_preflight(&dir.path, [(id, store.lifetime())])?;
    manager.reconcile(&mut verifier(refs.clone()))?;
    store.enable_writes()?;
    let (same, report) = store.gc(&head_root, COMPLETE_RECEIPT_BUDGET)?;
    assert!(report.blocked_by_pins);
    assert_eq!(same, head_root);
    assert_eq!(pins[0].read(b"key")?, Some(b"paired".to_vec()));
    assert_eq!(pins[1].read(b"key")?, Some(b"head".to_vec()));
    drop(pins);
    let (maintained, report) = store.gc(&head_root, COMPLETE_RECEIPT_BUDGET)?;
    assert!(report.deleted > 0);
    assert_eq!(store.read(&maintained, b"key")?, Some(b"head".to_vec()));
    Ok(())
}

#[test]
fn remaining_reference_from_another_artifact_protects_the_whole_store() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    let first = put(&generation, b"paired")?;
    let second = generation.store.update(&first, vec![(b"key".to_vec(), Some(b"head".to_vec()))])?;
    refs.borrow_mut().insert(id);
    manager.reconcile(&mut verifier(refs.clone()))?;
    drop(generation);
    // The removed pair and the remaining latest-head certificate refer to different roots of id.
    let report = manager.change_references([Some(id), Some(id)], &mut verifier(refs.clone()), || Ok(()))?;
    assert_eq!(report.retained, 1);
    let store = Store::open_existing(id.path(&dir.path))?;
    assert_eq!(store.read(&first, b"key")?, Some(b"paired".to_vec()));
    assert_eq!(store.read(&second, b"key")?, Some(b"head".to_vec()));
    drop(store);
    let changed = refs.clone();
    manager.change_references([Some(id), None], &mut verifier(refs.clone()), move || {
        changed.borrow_mut().remove(&id);
        Ok(())
    })?;
    assert!(!id.path(&dir.path).exists());
    Ok(())
}

#[test]
fn normal_restart_does_not_scan_or_clean_unjournaled_directory_entries() -> Result<()> {
    let (dir, manager) = fresh()?;
    let unknown = StoreId([93; 32]).path(&dir.path);
    fs::write(&unknown, b"not a sqlite database")?;
    fs::write(dir.path.join("unrelated-snapshot.tmp"), b"evidence")?;
    drop(manager);
    let mut manager = Manager::open_after_preflight(&dir.path, [])?;
    assert_eq!(manager.reconcile(&mut verifier(references([])))?, Maintenance::default());
    assert_eq!(fs::read(unknown)?, b"not a sqlite database");
    assert_eq!(fs::read(dir.path.join("unrelated-snapshot.tmp"))?, b"evidence");
    Ok(())
}

#[test]
fn only_exact_owned_sidecars_are_removed() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    drop(generation);
    for suffix in ["-journal", "-wal", "-shm", ".journal"] {
        fs::write(format!("{}{suffix}", id.path(&dir.path).display()), b"untrusted")?;
    }
    let extra = format!("{}.keep", id.path(&dir.path).display());
    fs::write(&extra, b"unowned")?;
    assert_eq!(manager.reconcile(&mut verifier(refs))?.deleted, 1);
    assert!(!id.path(&dir.path).exists());
    assert_eq!(fs::read(extra)?, b"unowned");
    for suffix in [".lock", "-journal", "-wal", "-shm", ".journal"] {
        assert!(!PathBuf::from(format!("{}{suffix}", id.path(&dir.path).display())).exists());
    }
    Ok(())
}

#[test]
fn many_retained_references_have_no_manager_fanout_cap() -> Result<()> {
    let (_dir, mut manager) = fresh()?;
    let refs = references((0u64..16_385).map(|number| {
        let mut id = [0; 32];
        id[..8].copy_from_slice(&number.to_be_bytes());
        StoreId(id)
    }));
    let generation = allocate_history(&mut manager, &refs)?;
    assert!(!refs.borrow().contains(&generation.id));
    assert_eq!(refs.borrow().len(), 16_385);
    Ok(())
}

// ── Helpers ──

pub(super) type References = Rc<RefCell<BTreeSet<StoreId>>>;

pub(super) struct TestDir {
    pub path: PathBuf,
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Creates one exclusive directory and manager without sharing process-global state.
/// Error cases:
/// - directory creation or journal bootstrap fails
pub(super) fn fresh() -> Result<(TestDir, Manager)> {
    let path = std::env::temp_dir().join(format!("engine-generation-test-{:032x}", rand::random::<u128>()));
    fs::create_dir(&path)?;
    let dir = TestDir { path };
    let manager = Manager::initialize_new_after_preflight(&dir.path)?;
    Ok((dir, manager))
}

/// Models the parent-owned already authenticated reference inventory, independently of the journal.
pub(super) fn references(ids: impl IntoIterator<Item = StoreId>) -> References {
    Rc::new(RefCell::new(ids.into_iter().collect()))
}

/// Models successful complete verification; error-path tests supply an explicitly failing verifier.
pub(super) fn verifier(refs: References) -> impl FnMut(&[StoreId]) -> Result<VerifiedReferences> {
    move |_| Ok(VerifiedReferences::from_verified_certificates(refs.borrow().iter().copied()))
}

/// Obtains one generation, failing the assertion if live work unexpectedly defers allocation.
/// Error cases:
/// - lifecycle allocation fails
pub(super) fn allocate_history(manager: &mut Manager, refs: &References) -> Result<Generation> {
    Ok(manager
        .allocate(Allocate::History, &mut verifier(refs.clone()))?
        .expect("not waiting")
        .history
        .expect("history"))
}

/// Writes a real verified JMT value using the generation's history namespace.
/// Error cases:
/// - authenticated update or durable commit fails
pub(super) fn put(generation: &Generation, value: &[u8]) -> Result<Root> {
    generation
        .store
        .update(&Root::empty(generation.id.history_namespace()), vec![(b"key".to_vec(), Some(value.to_vec()))])
}

/// Counts actual database files for retry-growth assertions, outside production startup paths.
/// Error cases:
/// - test directory enumeration fails
pub(super) fn database_count(dir: &TestDir) -> Result<usize> {
    fs::read_dir(&dir.path)?.try_fold(0, |count, entry| {
        let name = entry?.file_name();
        Ok(count + usize::from(name.to_string_lossy().ends_with(".sqlite")))
    })
}
