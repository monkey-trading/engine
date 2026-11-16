//! Which alert episodes a polling check has open, so it logs a resolution once per episode.

use std::collections::HashMap;
use std::sync::Mutex;

/// The raised or clear state of each named alert a repeated check reports.
///
/// A check that runs every few seconds calls `raise` where it logs the alert and `clear` where it
/// would log the resolution, logging only when `clear` returns true. The first `clear` after a
/// start returns true whatever came before, because a previous run may have left its episode open
/// in the alert outbox; every later one returns true only after a `raise`.
#[derive(Debug, Default)]
pub(crate) struct Episodes(Mutex<HashMap<&'static str, bool>>);

impl Episodes {
    /// Records that `name`'s episode is open.
    pub(crate) fn raise(&self, name: &'static str) {
        self.lock().insert(name, true);
    }

    /// Records that `name` is clear, and whether its resolution is due.
    pub(crate) fn clear(&self, name: &'static str) -> bool {
        self.lock().insert(name, false) != Some(false)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<&'static str, bool>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::Episodes;

    #[test]
    fn a_resolution_is_due_after_a_raise_and_once_after_a_start() {
        let episodes = Episodes::default();
        assert!(episodes.clear("a"), "a previous run may have left it open");
        assert!(!episodes.clear("a"));
        episodes.raise("a");
        episodes.raise("a");

        assert!(episodes.clear("a"));
        assert!(!episodes.clear("a"));
    }
}
