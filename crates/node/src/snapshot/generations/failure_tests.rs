//! Fault boundaries preserve retained databases and stop allocation until reconciliation succeeds.

use super::*;
use std::fs::{self, OpenOptions};

use crate::snapshot::generations::tests::{allocate_history, database_count, fresh, put, references, verifier};

#[test]
fn unknown_or_unreadable_reference_inventory_blocks_cleanup_and_allocation() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let path = generation.id.path(&dir.path);
    drop(generation);
    let before = fs::read(&path)?;
    let mut unknown = |_locators: &[StoreId]| -> Result<VerifiedReferences> {
        anyhow::bail!("unreadable retained catalog-head certificate")
    };
    assert!(manager.reconcile(&mut unknown).is_err());
    assert!(manager.allocate(Allocate::Pair, &mut unknown).is_err());
    assert_eq!(fs::read(path)?, before);
    assert_eq!(database_count(&dir)?, 1);
    assert!(!manager.disk.load()?.is_empty());
    Ok(())
}

#[test]
fn each_allocation_failure_preserves_earlier_durable_cache() -> Result<()> {
    for point in [
        Point::DirectoryOpen,
        Point::DirectorySync,
        Point::JournalCreate,
        Point::JournalWrite,
        Point::JournalSync,
        Point::JournalRename,
        Point::StoreCreate,
    ] {
        let (dir, mut manager) = fresh()?;
        let refs = references([]);
        let old = allocate_history(&mut manager, &refs)?;
        let root = put(&old, b"earlier durable cache")?;
        refs.borrow_mut().insert(old.id);
        manager.reconcile(&mut verifier(refs.clone()))?;
        // Directory failures happen after the new intent rename, rather than at the entry barrier.
        let skip = usize::from(matches!(point, Point::DirectoryOpen | Point::DirectorySync));
        manager.disk.fail_at(point, skip);
        assert!(manager.allocate(Allocate::Pair, &mut verifier(refs.clone())).is_err(), "{point:?}");
        assert_eq!(old.store.read(&root, b"key")?, Some(b"earlier durable cache".to_vec()), "{point:?}");
        assert_eq!(database_count(&dir)?, 1, "failed attempt created no DB: {point:?}");
        let next = manager.allocate(Allocate::Pair, &mut verifier(refs.clone()))?.expect("reconciled retry");
        assert!(next.history.is_some() && next.catalog.is_some());
        assert_eq!(database_count(&dir)?, 3);
    }
    Ok(())
}

#[test]
fn second_store_creation_failure_reclaims_the_first_before_retry() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    manager.disk.fail_at(Point::StoreCreate, 1);
    assert!(manager.allocate(Allocate::Pair, &mut verifier(refs.clone())).is_err());
    assert_eq!(database_count(&dir)?, 1);
    let interrupted = manager.disk.load()?.staging;
    drop(manager);
    let mut manager = Manager::open_after_preflight(&dir.path, [])?;
    let next = allocate_history(&mut manager, &refs)?;
    assert_eq!(database_count(&dir)?, 1);
    for old in interrupted.into_iter().flatten() {
        assert_ne!(next.id, old);
        assert!(!old.path(&dir.path).exists());
    }
    Ok(())
}

#[test]
fn cleanup_failure_prevents_a_second_database_allocation() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    drop(generation);
    manager.disk.fail_at(Point::Remove, 0);
    assert!(manager.allocate(Allocate::Pair, &mut verifier(refs.clone())).is_err());
    assert!(id.path(&dir.path).is_file());
    assert_eq!(database_count(&dir)?, 1);
    assert_eq!(manager.disk.load()?.staging[0], Some(id));
    allocate_history(&mut manager, &refs)?;
    assert!(!id.path(&dir.path).exists());
    assert_eq!(database_count(&dir)?, 1);
    Ok(())
}

#[test]
fn interrupted_unlink_directory_sync_keeps_intent_replayable() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    drop(generation);
    manager.disk.fail_at(Point::DirectorySync, 1);
    assert!(manager.reconcile(&mut verifier(refs.clone())).is_err());
    assert!(!id.path(&dir.path).exists());
    assert_eq!(manager.disk.load()?.staging[0], Some(id));
    drop(manager);
    let mut manager = Manager::open_after_preflight(&dir.path, [])?;
    assert_eq!(manager.reconcile(&mut verifier(refs))?.deleted, 1);
    assert!(manager.disk.load()?.is_empty());
    Ok(())
}

#[test]
fn partial_sidecar_unlink_failure_still_blocks_new_allocation() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    drop(generation);
    manager.disk.fail_at(Point::Remove, 1);
    assert!(manager.allocate(Allocate::History, &mut verifier(refs.clone())).is_err());
    assert!(!id.path(&dir.path).exists());
    assert!(!manager.disk.load()?.is_empty());
    allocate_history(&mut manager, &refs)?;
    assert_eq!(database_count(&dir)?, 1);
    Ok(())
}

#[test]
fn uncertain_publication_retains_both_old_and_visible_new_certified_files() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let old = allocate_history(&mut manager, &refs)?;
    let old_id = old.id;
    let old_root = put(&old, b"old")?;
    refs.borrow_mut().insert(old_id);
    manager.reconcile(&mut verifier(refs.clone()))?;
    let new = allocate_history(&mut manager, &refs)?;
    let new_id = new.id;
    let new_root = put(&new, b"new")?;
    let changed = refs.clone();
    assert!(
        manager
            .change_references([Some(old_id), None], &mut verifier(refs.clone()), move || {
                changed.borrow_mut().insert(new_id);
                anyhow::bail!("rename visible but directory sync failed; old pointer not pruned")
            })
            .is_err()
    );
    drop((old, new, manager));
    let mut manager = Manager::open_after_preflight(&dir.path, [])?;
    assert_eq!(manager.reconcile(&mut verifier(refs))?.deleted, 0);
    for (id, root, expected) in [(old_id, old_root, b"old"), (new_id, new_root, b"new")] {
        assert_eq!(Store::open_existing(id.path(&dir.path))?.read(&root, b"key")?, Some(expected.to_vec()));
    }
    Ok(())
}

#[test]
fn failed_prune_then_restart_preserves_still_referenced_old_generation() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let old = allocate_history(&mut manager, &refs)?;
    let id = old.id;
    refs.borrow_mut().insert(id);
    manager.reconcile(&mut verifier(refs.clone()))?;
    assert!(
        manager
            .change_references([Some(id), None], &mut verifier(refs.clone()), || {
                anyhow::bail!("certificate unlink failed")
            })
            .is_err()
    );
    drop((old, manager));
    let mut manager = Manager::open_after_preflight(&dir.path, [])?;
    assert_eq!(manager.reconcile(&mut verifier(refs))?.retained, 1);
    assert!(id.path(&dir.path).is_file());
    Ok(())
}

#[test]
fn nonregular_sidecar_refuses_cleanup_before_any_file_is_removed() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    drop(generation);
    fs::create_dir(format!("{}-journal", id.path(&dir.path).display()))?;
    assert!(manager.allocate(Allocate::History, &mut verifier(refs)).is_err());
    assert!(id.path(&dir.path).is_file());
    assert!(!manager.disk.load()?.is_empty());
    Ok(())
}

#[test]
fn missing_journal_cannot_be_reinitialized_over_existing_generations() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    drop((generation, manager));
    fs::remove_file(dir.path.join("authenticated-generations.v1"))?;
    assert!(Manager::open_after_preflight(&dir.path, []).is_err());
    assert!(Manager::initialize_new_after_preflight(&dir.path).is_err());
    assert!(id.path(&dir.path).is_file());
    Ok(())
}

#[test]
fn oversized_torn_and_unknown_format_journals_block_allocation() -> Result<()> {
    for bytes in [vec![0; 4096], vec![0; 17], vec![0; 140]] {
        let (dir, mut manager) = fresh()?;
        let refs = references([]);
        fs::write(dir.path.join("authenticated-generations.v1"), bytes)?;
        assert!(manager.allocate(Allocate::Pair, &mut verifier(refs)).is_err());
        assert_eq!(database_count(&dir)?, 0);
    }
    Ok(())
}

#[test]
fn manager_lock_is_exclusive_and_does_not_survive_owner_drop() -> Result<()> {
    let (dir, manager) = fresh()?;
    assert!(Manager::open_after_preflight(&dir.path, []).is_err());
    drop(manager);
    let reopened = Manager::open_after_preflight(&dir.path, [])?;
    assert!(reopened.disk.load()?.is_empty());
    Ok(())
}

#[test]
fn existing_database_creation_collision_preserves_its_exact_bytes() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    let before = fs::read(id.path(&dir.path))?;
    assert!(manager.create(id).is_err());
    assert_eq!(fs::read(id.path(&dir.path))?, before);
    Ok(())
}

#[test]
fn partial_next_journal_never_replaces_a_valid_canonical_intent_on_restart() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    drop((generation, manager));
    fs::write(dir.path.join("authenticated-generations.v1.next"), b"partial")?;
    let mut manager = Manager::open_after_preflight(&dir.path, [])?;
    assert_eq!(manager.disk.load()?.staging[0], Some(id));
    assert_eq!(manager.reconcile(&mut verifier(refs))?.deleted, 1);
    assert!(!id.path(&dir.path).exists());
    assert!(manager.disk.load()?.is_empty());
    Ok(())
}

#[test]
fn unknown_inventory_after_prune_leaves_intents_and_all_cache_files_untouched() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let old = allocate_history(&mut manager, &refs)?;
    let id = old.id;
    refs.borrow_mut().insert(id);
    manager.reconcile(&mut verifier(refs.clone()))?;
    let mut calls = 0;
    let mut scan = |_locators: &[StoreId]| -> Result<VerifiedReferences> {
        calls += 1;
        if calls == 1 {
            return Ok(VerifiedReferences::from_verified_certificates([id]));
        }
        anyhow::bail!("cannot enumerate certificates after pruning")
    };
    assert!(manager.change_references([Some(id), None], &mut scan, || Ok(())).is_err());
    drop(old);
    assert_eq!(manager.disk.load()?.retiring[0], Some(id));
    assert!(id.path(&dir.path).is_file());
    Ok(())
}

#[test]
fn sparse_huge_journal_is_rejected_by_length_without_reading_its_body() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    OpenOptions::new().write(true).open(dir.path.join("authenticated-generations.v1"))?.set_len(1 << 32)?;
    assert!(manager.allocate(Allocate::History, &mut verifier(references([]))).is_err());
    assert_eq!(database_count(&dir)?, 0);
    Ok(())
}

#[test]
fn malformed_locator_tags_padding_and_duplicates_fail_closed() -> Result<()> {
    for mutation in 0..3 {
        let (dir, mut manager) = fresh()?;
        let refs = references([]);
        let generation = allocate_history(&mut manager, &refs)?;
        let id = generation.id;
        drop(generation);
        let path = dir.path.join("authenticated-generations.v1");
        let mut bytes = fs::read(&path)?;
        match mutation {
            0 => bytes[8] = 2,
            1 => bytes[41 + 1] = 1,
            _ => {
                let duplicate = bytes[8..41].to_vec();
                bytes[41..74].copy_from_slice(&duplicate);
            }
        }
        fs::write(path, bytes)?;
        assert!(manager.allocate(Allocate::Pair, &mut verifier(refs)).is_err());
        assert!(id.path(&dir.path).is_file());
        assert_eq!(database_count(&dir)?, 1);
    }
    Ok(())
}

#[test]
fn reader_blocked_retirement_rejects_another_reference_change_without_calling_it() -> Result<()> {
    let (_dir, mut manager) = fresh()?;
    let refs = references([]);
    let old = allocate_history(&mut manager, &refs)?;
    refs.borrow_mut().insert(old.id);
    manager.reconcile(&mut verifier(refs.clone()))?;
    let changed = refs.clone();
    let id = old.id;
    let waiting = manager.change_references([Some(id), None], &mut verifier(refs.clone()), move || {
        changed.borrow_mut().remove(&id);
        Ok(())
    })?;
    assert!(waiting.waiting_for_readers);
    let mut called = false;
    assert!(
        manager
            .change_references([Some(StoreId([71; 32])), None], &mut verifier(refs), || {
                called = true;
                Ok(())
            })
            .is_err()
    );
    assert!(!called);
    assert_eq!(manager.disk.load()?.retiring[0], Some(id));
    drop(old);
    Ok(())
}

#[cfg(unix)]
#[test]
fn symlink_sidecar_preserves_its_target_and_database() -> Result<()> {
    use std::os::unix::fs::symlink;

    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    drop(generation);
    let target = dir.path.join("unowned-target");
    fs::write(&target, b"preserved")?;
    symlink(&target, format!("{}-journal", id.path(&dir.path).display()))?;
    assert!(manager.allocate(Allocate::History, &mut verifier(refs)).is_err());
    assert!(id.path(&dir.path).is_file());
    assert_eq!(fs::read(target)?, b"preserved");
    Ok(())
}

#[test]
fn failed_entry_directory_barrier_does_not_consult_or_act_on_a_reference_view() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    drop(generation);
    manager.disk.fail_at(Point::DirectorySync, 0);
    let mut consulted = false;
    let mut verify = |_locators: &[StoreId]| {
        consulted = true;
        Ok(VerifiedReferences::from_verified_certificates([]))
    };
    assert!(manager.reconcile(&mut verify).is_err());
    assert!(!consulted);
    assert!(id.path(&dir.path).is_file());
    Ok(())
}

#[test]
fn restart_verifier_receives_pending_ids_as_locators_but_still_protects_retained_files() -> Result<()> {
    let (dir, mut manager) = fresh()?;
    let refs = references([]);
    let generation = allocate_history(&mut manager, &refs)?;
    let id = generation.id;
    drop((generation, manager));
    let mut manager = Manager::open_after_preflight(&dir.path, [])?;
    let mut seen = Vec::new();
    let mut verify = |locators: &[StoreId]| {
        seen.extend_from_slice(locators);
        Ok(VerifiedReferences::from_verified_certificates([id]))
    };
    let report = manager.reconcile(&mut verify)?;
    assert_eq!(seen, vec![id]);
    assert_eq!(report.retained, 1);
    assert_eq!(report.deleted, 0);
    assert!(id.path(&dir.path).is_file());
    Ok(())
}
