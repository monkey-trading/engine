//! Finite overlays commit exact verified updates to private pending roots before one final Head.

use anyhow::Result;

use crate::authenticated::{Lease, Root, Store};
use crate::history::authenticated::model::PAYLOAD_CHUNK_BYTES;
use crate::history::authenticated::overlay::Overlay;
use crate::history::authenticated::state::{STATE_KEY, State};

/// A physical staging preference, not a bound on commands, payloads, accounts, or expiry: about the
/// keys one update of launch flow's commands writes, so such an update commits about once.
pub(super) const STAGED_KEYS: usize = 128;

/// The value bytes a stage holds before it commits, proved old values and staged ones, whatever its keys:
/// 32 payload chunks, which a large event's chunks, written or expired, reach well before `STAGED_KEYS`.
pub(super) const STAGED_BYTES: usize = 32 * PAYLOAD_CHUNK_BYTES;

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(super) struct StageInfo {
    pub keys: usize,
    pub buffered_bytes: usize,
}

#[cfg(not(test))]
pub(super) type StageInfo = ();

pub(super) struct Staged {
    store: Store,
    pub root: Root,
    pub state: State,
    pub overlay: Overlay,
    _pin: Lease,
}

impl Staged {
    /// Holds the complete old root through the Index and the current private root through this stage.
    /// Error cases:
    /// - private-root lease admission fails
    pub fn new(store: &Store, root: &Root, mut state: State) -> Result<Self> {
        state.complete = false;
        let pin = store.lease(root)?;
        Ok(Self { store: store.clone(), root: root.clone(), state, overlay: Overlay::session(store, &pin), _pin: pin })
    }

    /// Commits after finite work; the caller finishes one small header operation before this check.
    /// Error cases:
    /// - any exact update proof, private commit, lease, or observer failure
    pub fn flush_if_full(&mut self, observe: &mut impl FnMut(&Root, StageInfo) -> Result<()>) -> Result<()> {
        if self.overlay.len() >= STAGED_KEYS || self.overlay.held_bytes() >= STAGED_BYTES {
            self.flush(observe)?;
        }
        Ok(())
    }

    /// Marks complete only after all expiry, event chunks, dense ranks and queue changes succeeded.
    /// Error cases:
    /// - final verified durable update or complete-state validation fails
    pub fn finish(mut self) -> Result<(Root, State)> {
        self.state.complete = true;
        self.flush(&mut |_, _| Ok(()))?;
        Ok((self.root, self.state))
    }

    /// Writes the exact state with every bounded batch; no private root is an output Head.
    /// Error cases:
    /// - state encoding/proof, Store update/durability, pin, or test observer failure
    fn flush(&mut self, observe: &mut impl FnMut(&Root, StageInfo) -> Result<()>) -> Result<()> {
        self.state.validate()?;
        self.overlay.put(STATE_KEY.to_vec(), &self.state)?;
        #[cfg(test)]
        let info = StageInfo { keys: self.overlay.len(), buffered_bytes: self.overlay.buffered_bytes() };
        #[cfg(not(test))]
        let info = ();
        let fresh = Overlay::new(&self.store, &self.root);
        let updates = std::mem::replace(&mut self.overlay, fresh).finish();
        // A private stage is never a head: the complete stage's synced update makes it durable with
        // itself, and a crash before then resumes from the head the stages started at.
        let root = if self.state.complete {
            self.store.update(&self.root, updates)?
        } else {
            self.store.update_unsynced(&self.root, updates)?
        };
        let pin = self.store.lease(&root)?;
        self.root = root;
        self.overlay = Overlay::session(&self.store, &pin);
        self._pin = pin;
        if !self.state.complete {
            observe(&self.root, info)?;
        }
        Ok(())
    }
}
