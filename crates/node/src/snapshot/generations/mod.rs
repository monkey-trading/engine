//! Post-preflight generation allocation and reclamation; signed references supply deletion authority.

mod disk;
use crate::snapshot::generations::disk::{Disk, Point, State};

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use rand::{RngCore, rngs::OsRng};

use crate::authenticated::{Lifetime, Store};
use crate::snapshot::head::StoreId;

/// Complete verified retained references, including uncertain publications and both head families.
/// This type cannot be deserialized from the allocation journal or an unsigned inventory.
pub(crate) struct VerifiedReferences {
    stores: BTreeSet<StoreId>,
}

/// Repair replaces the history cache; migration creates both caches before their first pair.
#[derive(Clone, Copy)]
pub(crate) enum Allocate {
    History,
    Pair,
}

/// One freshly created database; namespace selection continues to use StoreId's role methods.
pub(crate) struct Generation {
    pub id: StoreId,
    pub store: Store,
}

/// Both stores belong to one bounded attempt, even if its certificate publication fails.
pub(crate) struct Generations {
    pub history: Option<Generation>,
    pub catalog: Option<Generation>,
}

/// One finite reconciliation pass; a reader wait is idle, not a reason to poll in a tight loop.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct Maintenance {
    pub deleted: usize,
    pub retained: usize,
    pub waiting_for_readers: bool,
}

/// Single storage-actor owner. Every admitted Store lineage must be registered before maintenance.
pub(crate) struct Manager {
    disk: Disk,
    _lock: File,
    owners: BTreeMap<StoreId, Lifetime>,
}

impl VerifiedReferences {
    /// Accepts only a complete, freshly verified parent enumeration, never a selected-cache subset.
    /// The parent verifies signer/domain/codec/full roots and rejects unknown or unreadable references.
    pub(crate) fn from_verified_certificates(stores: impl IntoIterator<Item = StoreId>) -> Self {
        Self { stores: stores.into_iter().collect() }
    }
}

impl Manager {
    /// Bootstraps once after #24 in a directory containing no generation files; never a recovery reset.
    /// Error cases:
    /// - any generation already exists, the journal exists, locking, enumeration or durability fails
    pub(crate) fn initialize_new_after_preflight(directory: &Path) -> Result<Self> {
        let disk = Disk::new(directory)?;
        let lock = disk.lock()?;
        disk.initialize()?;
        Ok(Self { disk, _lock: lock, owners: BTreeMap::new() })
    }

    /// Opens the existing journal after #24, with actual shared-lineage lifetime tokens restored.
    /// Restore retained root Pins on these same Store owners before enabling writes or utility GC.
    /// Error cases:
    /// - missing/malformed journal, duplicate owner, lock contention, or directory IO failure
    pub(crate) fn open_after_preflight(
        directory: &Path,
        owners: impl IntoIterator<Item = (StoreId, Lifetime)>,
    ) -> Result<Self> {
        let disk = Disk::new(directory)?;
        let lock = disk.lock()?;
        disk.load()?;
        let mut registered = BTreeMap::new();
        for (id, lifetime) in owners {
            ensure!(registered.insert(id, lifetime).is_none(), "duplicate generation owner; reuse one Store lineage");
        }
        Ok(Self { disk, _lock: lock, owners: registered })
    }

    /// Reconciles before allocation; the verifier receives pending untrusted locators after directory sync.
    /// Those IDs help locate old metadata but cannot establish the completeness of signed references.
    /// Error cases:
    /// - incomplete references, malformed journal/paths, deletion, journal or directory durability fails
    pub(crate) fn reconcile(
        &mut self,
        verify: &mut impl FnMut(&[StoreId]) -> Result<VerifiedReferences>,
    ) -> Result<Maintenance> {
        self.reconcile_inner(verify).map(|(report, _)| report)
    }

    /// Uses the finite retirement slots for abandoned live attempts before allocating replacements.
    /// The parent must retain the returned Stores until publication or abandonment is finished.
    /// Error cases:
    /// - reconciliation, entropy, exclusive creation, store initialization or durable intent fails
    pub(crate) fn allocate(
        &mut self,
        which: Allocate,
        verify: &mut impl FnMut(&[StoreId]) -> Result<VerifiedReferences>,
    ) -> Result<Option<Generations>> {
        let (_, references) = self.reconcile_inner(verify)?;
        let mut state = self.disk.load()?;
        ensure!(
            state.staging.iter().flatten().all(|id| !references.stores.contains(id)),
            "staging references changed during allocation"
        );
        let abandoned = state.staging.iter().flatten().count();
        let available = state.retiring.iter().filter(|slot| slot.is_none()).count();
        if abandoned > available {
            return Ok(None);
        }
        for slot in &mut state.staging {
            if let Some(id) = slot.take() {
                let retiring =
                    state.retiring.iter_mut().find(|slot| slot.is_none()).expect("retirement capacity checked");
                *retiring = Some(id);
            }
        }
        if matches!(which, Allocate::History | Allocate::Pair) {
            state.staging[0] = Some(self.fresh_id(&references, &state)?);
        }
        if matches!(which, Allocate::Pair) {
            state.staging[1] = Some(self.fresh_id(&references, &state)?);
        }
        self.disk.save(&state)?;
        let history = state.staging[0].map(|id| self.create(id)).transpose()?;
        let catalog = state.staging[1].map(|id| self.create(id)).transpose()?;
        Ok(Some(Generations { history, catalog }))
    }

    /// Journals the old IDs before replacing/removing ONE certificate; a pair names at most two IDs.
    /// The callback must durably publish replacements before pruning, and must not prune other artifacts.
    /// Error cases:
    /// - prior retirement waits, reference verification, callback, journal or directory IO fails
    pub(crate) fn change_references(
        &mut self,
        mut outgoing: [Option<StoreId>; 2],
        verify: &mut impl FnMut(&[StoreId]) -> Result<VerifiedReferences>,
        change: impl FnOnce() -> Result<()>,
    ) -> Result<Maintenance> {
        self.reconcile_inner(verify)?;
        let mut state = self.disk.load()?;
        ensure!(state.retiring.iter().all(Option::is_none), "previous generation retirement still has readers");
        if outgoing[0] == outgoing[1] {
            outgoing[1] = None;
        }
        state.retiring = outgoing;
        self.disk.save(&state)?;
        // On an uncertain callback result, retain every locator until the next synced enumeration.
        change().context("changing retained generation references")?;
        self.reconcile(verify)
    }

    /// Checks a fixed number of journal locators against complete signed references and actual owners.
    /// Error cases:
    /// - as reconcile; no failed pass can clear an unresolved durable intent
    fn reconcile_inner(
        &mut self,
        verify: &mut impl FnMut(&[StoreId]) -> Result<VerifiedReferences>,
    ) -> Result<(Maintenance, VerifiedReferences)> {
        let mut state = self.disk.load()?;
        // Make any interrupted certificate rename/prune durable BEFORE asking which roots survive.
        self.disk.sync_directory()?;
        let locators: Vec<_> = state.staging.iter().chain(&state.retiring).flatten().copied().collect();
        let references = verify(&locators).context("complete retained generation reference verification")?;
        let mut report = Maintenance::default();
        let before = state.clone();
        for slot in state.staging.iter_mut().chain(state.retiring.iter_mut()) {
            let Some(id) = *slot else { continue };
            if references.stores.contains(&id) {
                report.retained += 1;
                *slot = None;
            } else if self.owners.get(&id).is_some_and(Lifetime::alive) {
                report.waiting_for_readers = true;
            } else {
                self.disk.remove_generation(id)?;
                report.deleted += 1;
                *slot = None;
            }
        }
        if state != before {
            // Deletions must survive a crash before their locators disappear from the journal.
            self.disk.sync_directory()?;
            self.disk.save(&state)?;
        }
        self.owners.retain(|_, lifetime| lifetime.alive());
        Ok((report, references))
    }

    /// Obtains one OS-random identifier and refuses any collision without overwriting old files.
    /// Error cases:
    /// - entropy failure, path IO, or an existing referenced/live/staged/filesystem identifier
    fn fresh_id(&self, references: &VerifiedReferences, state: &State) -> Result<StoreId> {
        let mut bytes = [0; 32];
        OsRng.try_fill_bytes(&mut bytes).context("generation OS entropy")?;
        let id = StoreId(bytes);
        ensure!(
            !references.stores.contains(&id)
                && !self.owners.contains_key(&id)
                && !state.staging.contains(&Some(id))
                && !state.retiring.contains(&Some(id))
                && !self.disk.generation_exists(id)?,
            "fresh generation identifier collision"
        );
        Ok(id)
    }

    /// Registers the exact newly created Arc lineage before returning any reader-capable object.
    /// Error cases:
    /// - exclusive Store creation, schema initialization or durability fails
    fn create(&mut self, id: StoreId) -> Result<Generation> {
        self.disk.check(Point::StoreCreate)?;
        let store = Store::create(id.path(self.disk.directory()))?;
        self.owners.insert(id, store.lifetime());
        Ok(Generation { id, store })
    }
}

#[cfg(test)]
mod failure_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod repair_tests;
