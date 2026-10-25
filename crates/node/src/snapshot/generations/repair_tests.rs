//! Bounded replacement preserves abandoned generations and their actual owner/reader lifetimes.

use super::*;
use std::fs;

use crate::authenticated::Root;
use crate::snapshot::generations::tests::{allocate_history, database_count, fresh, put, references, verifier};

#[test]
fn live_pair_moves_to_retiring_and_one_replacement_fills_exactly_four_slots() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let old = manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.expect("old pair");
    let old_history = old.history.as_ref().expect("history");
    let old_catalog = old.catalog.as_ref().expect("catalog");
    let root = put(old_history, b"still owned")?;
    let replacement = manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.expect("replacement");
    let state = manager.disk.load()?;
    assert_eq!(state.retiring, [Some(old_history.id), Some(old_catalog.id)]);
    assert_eq!(state.staging, [replacement.history.as_ref().map(|g| g.id), replacement.catalog.as_ref().map(|g| g.id)]);
    assert_eq!(database_count(&dir)?, 4);
    let bytes = fs::read(dir.path.join("authenticated-generations.v1"))?;
    for _ in 0..8 {
        assert!(manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.is_none());
        assert_eq!(database_count(&dir)?, 4);
        assert_eq!(fs::read(dir.path.join("authenticated-generations.v1"))?, bytes);
        assert_eq!(old_history.store.read(&root, b"key")?, Some(b"still owned".to_vec()));
    }
    let old_ids = [old_history.id, old_catalog.id];
    drop(old);
    let next = allocate_history(&mut manager, &refs)?;
    assert!(old_ids.iter().all(|id| !id.path(&dir.path).exists()));
    assert_eq!(database_count(&dir)?, 3);
    assert!(next.id.path(&dir.path).is_file());
    drop(replacement);
    Ok(())
}

#[test]
fn insufficient_retirement_space_does_not_partially_move_a_live_staging_pair() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let old = allocate_history(&mut manager, &refs)?;
    let pair = manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.expect("replacement pair");
    let before = manager.disk.load()?;
    assert_eq!(before.retiring, [Some(old.id), None]);
    assert!(manager.allocate(Allocate::History, &mut verifier(refs.clone()))?.is_none());
    let after = manager.disk.load()?;
    assert!(before == after, "both staging IDs must remain discoverable without partial relocation");
    assert_eq!(database_count(&dir)?, 3);
    drop((pair, old));
    Ok(())
}

#[test]
fn unknown_references_prevent_abandoned_stage_transfer_and_replacement() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let old = manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.expect("old pair");
    let before = fs::read(dir.path.join("authenticated-generations.v1"))?;
    let mut unknown =
        |_locators: &[StoreId]| -> Result<VerifiedReferences> { anyhow::bail!("unknown retained reference format") };
    assert!(manager.allocate(Allocate::Pair, &mut unknown).is_err());
    assert_eq!(fs::read(dir.path.join("authenticated-generations.v1"))?, before);
    assert_eq!(database_count(&dir)?, 2);
    let new = manager.allocate(Allocate::Pair, &mut verifier(refs))?.expect("verified replacement");
    assert_eq!(database_count(&dir)?, 4);
    drop((old, new));
    Ok(())
}

#[test]
fn each_transfer_barrier_failure_keeps_old_intents_and_allows_a_bounded_retry() -> Result<()> {
    for point in [
        Point::JournalCreate,
        Point::JournalWrite,
        Point::JournalSync,
        Point::JournalRename,
        Point::DirectoryOpen,
        Point::DirectorySync,
    ] {
        let (dir, mut manager) = fresh()?;
        let refs = references([]);
        let old = manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.expect("old pair");
        let history = old.history.as_ref().expect("history");
        let catalog = old.catalog.as_ref().expect("catalog");
        let root = put(history, b"protected during transfer")?;
        let skip = usize::from(matches!(point, Point::DirectoryOpen | Point::DirectorySync));
        manager.disk.fail_at(point, skip);
        assert!(manager.allocate(Allocate::Pair, &mut verifier(refs.clone())).is_err(), "{point:?}");
        assert_eq!(database_count(&dir)?, 2, "no create before durable transfer: {point:?}");
        let interrupted = manager.disk.load()?;
        let locators: Vec<_> = interrupted.staging.iter().chain(&interrupted.retiring).flatten().copied().collect();
        for id in [history.id, catalog.id] {
            assert_eq!(locators.iter().filter(|entry| **entry == id).count(), 1, "{point:?}");
        }
        assert_eq!(history.store.read(&root, b"key")?, Some(b"protected during transfer".to_vec()));
        let new = manager.allocate(Allocate::Pair, &mut verifier(refs))?.expect("bounded retry");
        assert_eq!(database_count(&dir)?, 4);
        drop((new, old));
    }
    Ok(())
}

#[test]
fn moved_retirement_intents_survive_reopen_and_actual_leases_guard_cleanup() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let old = manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.expect("old pair");
    let history = old.history.as_ref().expect("history");
    let catalog = old.catalog.as_ref().expect("catalog");
    let old_ids = [history.id, catalog.id];
    let h_root = put(history, b"history")?;
    let c_root = catalog
        .store
        .update(&Root::empty(catalog.id.catalog_namespace()), vec![(b"key".to_vec(), Some(b"catalog".to_vec()))])?;
    let h_lease = history.store.lease(&h_root)?;
    let c_lease = catalog.store.lease(&c_root)?;
    let mut owners = vec![(history.id, history.store.lifetime()), (catalog.id, catalog.store.lifetime())];
    let new = manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.expect("new pair");
    for generation in [&new.history, &new.catalog].into_iter().flatten() {
        owners.push((generation.id, generation.store.lifetime()));
        refs.borrow_mut().insert(generation.id);
    }
    drop((old, manager));
    let mut manager = Manager::open_after_preflight(&dir.path, owners)?;
    assert!(manager.reconcile(&mut verifier(refs.clone()))?.waiting_for_readers);
    assert_eq!(h_lease.read(b"key")?, Some(b"history".to_vec()));
    assert_eq!(c_lease.read(b"key")?, Some(b"catalog".to_vec()));
    drop((h_lease, c_lease));
    assert_eq!(manager.reconcile(&mut verifier(refs.clone()))?.deleted, 2);
    assert_eq!(manager.reconcile(&mut verifier(refs))?, Maintenance::default());
    assert!(old_ids.iter().all(|id| !id.path(&dir.path).exists()));
    assert_eq!(database_count(&dir)?, 2);
    drop(new);
    Ok(())
}

#[test]
fn retained_reference_clears_staging_without_moving_or_deleting_its_store() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let old = allocate_history(&mut manager, &refs)?;
    let root = put(&old, b"retained signed reference")?;
    let id = old.id;
    refs.borrow_mut().insert(id);
    drop(old);
    let replacement = manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.expect("replacement");
    let state = manager.disk.load()?;
    assert!(!state.staging.contains(&Some(id)) && !state.retiring.contains(&Some(id)));
    assert_eq!(
        Store::open_existing(id.path(&dir.path))?.read(&root, b"key")?,
        Some(b"retained signed reference".to_vec())
    );
    for _ in 0..4 {
        manager.reconcile(&mut verifier(refs.clone()))?;
        assert!(id.path(&dir.path).is_file());
    }
    drop(replacement);
    manager.reconcile(&mut verifier(refs))?;
    assert!(id.path(&dir.path).is_file());
    assert_eq!(database_count(&dir)?, 1);
    Ok(())
}
