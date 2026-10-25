//! Finite overlays commit exact verified updates to private pending roots before one final Head.

use anyhow::Result;

use crate::authenticated::{Lease, Root, Store};
use crate::history::authenticated::overlay::Overlay;
use crate::history::authenticated::state::{STATE_KEY, State};

/// A physical staging preference, not a bound on commands, payloads, accounts, or expiry.
pub(super) const STAGED_KEYS: usize = 32;

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
        Ok(Self {
            store: store.clone(),
            root: root.clone(),
            state,
            overlay: Overlay::new(store, root),
            _pin: store.lease(root)?,
        })
    }

    /// Commits after finite work; the caller finishes one small header operation before this check.
    /// Error cases:
    /// - any exact update proof, durable commit, lease, or observer failure
    pub fn flush_if_full(&mut self, observe: &mut impl FnMut(&Root, StageInfo) -> Result<()>) -> Result<()> {
        if self.overlay.len() >= STAGED_KEYS {
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
        let root = self.store.update(&self.root, updates)?;
        let pin = self.store.lease(&root)?;
        self.root = root;
        self._pin = pin;
        self.overlay = Overlay::new(&self.store, &self.root);
        if !self.state.complete {
            observe(&self.root, info)?;
        }
        Ok(())
    }
}
