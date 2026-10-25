//! One per-node gate serializes generation allocation and all retained-certificate pruning.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Result, ensure};

use crate::authenticated::{Lifetime, Store};
use crate::config::SnapshotSection;
use crate::log::CheckpointDomain;
use crate::snapshot::generations::{Allocate, Generations, Manager};
use crate::snapshot::head::StoreId;
use crate::snapshot::payloads::Payloads;
use crate::snapshot::references::{Inventory, Kind};
use crate::snapshot::{Prune, sync_directory};

pub(crate) struct Lifecycle {
    state: Mutex<State>,
    payloads: Arc<Payloads>,
}

struct State {
    manager: Manager,
    inventory: Inventory,
    directory: PathBuf,
    live: BTreeMap<StoreId, Lifetime>,
}

/// Held across publication and all its final-file removals; Store mutations finish beforehand.
pub(crate) struct Publication<'a> {
    state: MutexGuard<'a, State>,
}

impl Lifecycle {
    /// Creates no file until the caller has completed independent engine/custody preflight.
    /// Error cases:
    /// - retained-reference verification, manager ownership, journal or directory durability fails
    pub(crate) fn after_preflight(
        cfg: SnapshotSection,
        domain: CheckpointDomain,
        owners: impl IntoIterator<Item = (StoreId, Store)>,
        active_catalog: Option<StoreId>,
    ) -> Result<Arc<Self>> {
        fs::create_dir_all(&cfg.dir)?;
        let mut lineages: BTreeMap<StoreId, Store> = BTreeMap::new();
        for (id, store) in owners {
            if let Some(previous) = lineages.get(&id) {
                ensure!(store.same_owner(previous), "one generation has independent Store owners");
            } else {
                lineages.insert(id, store);
            }
        }
        let live: BTreeMap<_, _> = lineages.iter().map(|(id, store)| (*id, store.lifetime())).collect();
        let payloads = Arc::new(Payloads::default());
        let inventory = Inventory::open(cfg.clone(), domain, active_catalog, payloads.clone())?;
        let manager = if cfg.dir.join("authenticated-generations.v1").try_exists()? {
            Manager::open_after_preflight(&cfg.dir, live.iter().map(|(id, lifetime)| (*id, lifetime.clone())))?
        } else {
            Manager::initialize_new_after_preflight(&cfg.dir)?
        };
        let lifecycle =
            Arc::new(Self { state: Mutex::new(State { manager, inventory, directory: cfg.dir, live }), payloads });
        lifecycle.publication()?.reconcile()?;
        Ok(lifecycle)
    }

    /// Allocates from a reconciled finite journal and registers actual reader-capable lifetimes.
    /// Error cases:
    /// - reference/IO reconciliation, entropy, file creation or ownership registration fails
    pub(crate) fn allocate(&self, which: Allocate) -> Result<Option<Generations>> {
        let mut publication = self.publication()?;
        publication.reconcile()?;
        let State { manager, inventory, live, .. } = &mut *publication.state;
        let generations = manager.allocate(which, &mut |locators| inventory.verify(locators))?;
        if let Some(generations) = &generations {
            for generation in generations.history.iter().chain(&generations.catalog) {
                live.insert(generation.id, generation.store.lifetime());
            }
        }
        Ok(generations)
    }

    /// Changes only the live-role selector; signed artifacts and actual lifetimes still protect data.
    pub(crate) fn active_catalog(&self, id: StoreId) -> Result<()> {
        self.publication()?.state.inventory.active_catalog = Some(id);
        Ok(())
    }

    /// Serializes signing-pointer allocation, fsync publication and generation-aware retirement.
    /// Error cases:
    /// - the per-node owner was poisoned
    pub(crate) fn publication(&self) -> Result<Publication<'_>> {
        let state = self.state.lock().map_err(|_| anyhow::anyhow!("generation publication owner poisoned"))?;
        Ok(Publication { state })
    }

    /// The node's shared table of authenticated cache payloads, for every pin set it keeps.
    pub(crate) fn payloads(&self) -> Arc<Payloads> {
        self.payloads.clone()
    }
}

impl Publication<'_> {
    /// Settles uncertain metadata before a caller releases root pins or allocates another pointer.
    /// Error cases:
    /// - directory durability, complete reference verification or journal reconciliation fails
    pub(crate) fn settle(&mut self) -> Result<()> {
        self.reconcile()
    }

    /// Reclaims obsolete catalog-head families and abandoned named data without enumerating history or
    /// catalog rows.
    /// Error cases:
    /// - incomplete references, a pending actual reader or any durable retirement operation fails
    fn reconcile(&mut self) -> Result<()> {
        {
            let State { manager, inventory, .. } = &mut *self.state;
            manager.reconcile(&mut |locators| inventory.verify(locators))?;
        }
        for path in self.state.inventory.obsolete_catalog_heads(None) {
            self.remove_one(&path)?;
        }
        let paths = fs::read_dir(&self.state.directory)?.collect::<std::io::Result<Vec<_>>>()?;
        for entry in paths {
            let Some(id) = generation_id(&entry.file_name().to_string_lossy()) else { continue };
            if self.state.inventory.referenced(id) || self.state.live.get(&id).is_some_and(Lifetime::alive) {
                continue;
            }
            let State { manager, inventory, .. } = &mut *self.state;
            manager.change_references([Some(id), None], &mut |locators| inventory.verify(locators), || Ok(()))?;
        }
        self.state.live.retain(|_, lifetime| lifetime.alive());
        Ok(())
    }

    /// Journals one artifact's outgoing IDs and keeps uncertain unlink attempts recoverable.
    /// Error cases:
    /// - reference verification, durable retirement intent, unlink, sync or cleanup fails
    fn remove_one(&mut self, path: &Path) -> Result<()> {
        ensure!(
            path.parent() == Some(self.state.directory.as_path()),
            "retirement path is outside the cache directory"
        );
        {
            let State { manager, inventory, .. } = &mut *self.state;
            manager.reconcile(&mut |locators| inventory.verify(locators))?;
        }
        let outgoing = self.state.inventory.record(path).map_or([None, None], |record| record.ids);
        let State { manager, inventory, directory, .. } = &mut *self.state;
        let inventory = RefCell::new(inventory);
        manager.change_references(outgoing, &mut |locators| inventory.borrow_mut().verify(locators), || {
            inventory.borrow_mut().removing(path);
            fs::remove_file(path)?;
            sync_directory(directory)
        })?;
        Ok(())
    }
}

impl Prune for Publication<'_> {
    /// Retires obsolete continuation slots while the outgoing C still protects its data file.
    /// Error cases:
    /// - a per-artifact journal/unlink/sync transaction fails
    fn remove(&mut self, path: &Path) -> Result<()> {
        {
            let State { manager, inventory, .. } = &mut *self.state;
            manager.reconcile(&mut |locators| inventory.verify(locators))?;
        }
        if self.state.inventory.record(path).is_some_and(|record| record.kind == Kind::Pair) {
            for auxiliary in self.state.inventory.obsolete_catalog_heads(Some(path)) {
                self.remove_one(&auxiliary)?;
            }
        }
        self.remove_one(path)
    }
}

fn generation_id(file: &str) -> Option<StoreId> {
    let hex = file.strip_prefix("authenticated-")?.strip_suffix(".sqlite")?;
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
        return None;
    }
    Some(StoreId(hex::decode(hex).ok()?.try_into().ok()?))
}
