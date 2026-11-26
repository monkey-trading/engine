//! The segments of a log that its readers hold: each reader of a writer's published view holds the segments from
//! the one it reads on, and the log's local window (`archive::window`) removes none of them. A removal checks the
//! holds and unlinks under one lock, so a reader that takes its hold after finds the segment gone and reads it from
//! the archive, and one that took it before keeps the segment. The lock is shared, not owned by one task, since
//! readers open segments on blocking threads and a removal must see every hold taken before it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The holds on one log's segments, shared by its writer's published views.
#[derive(Clone, Default)]
pub struct Holds(Arc<Mutex<Held>>);

/// Each hold by its id, at the first seq of the segment its reader reads.
#[derive(Default)]
struct Held {
    next: u64,
    from: BTreeMap<u64, u64>,
}

/// A reader's hold on the segments from one on, released when dropped.
pub struct Hold {
    holds: Holds,
    id: u64,
}

impl Holds {
    /// A hold on the segments from the one starting at `seq` on.
    pub fn hold(&self, seq: u64) -> Hold {
        let mut held = self.lock();
        let id = held.next;
        held.next += 1;
        held.from.insert(id, seq);
        Hold { holds: self.clone(), id }
    }

    /// Runs `remove` for a segment ending at `last_seq` unless a reader holds it, under the holds' lock, so no reader
    /// takes a hold on it meanwhile; `None` when one holds it.
    pub fn unless_held<T>(&self, last_seq: u64, remove: impl FnOnce() -> T) -> Option<T> {
        let held = self.lock();
        match held.from.values().any(|from| *from <= last_seq) {
            true => None,
            false => Some(remove()),
        }
    }

    /// The first seq of the oldest segment a reader holds, if any.
    pub fn oldest(&self) -> Option<u64> {
        self.lock().from.values().min().copied()
    }

    /// The holds, whose lock no holder panics under.
    fn lock(&self) -> std::sync::MutexGuard<'_, Held> {
        self.0.lock().expect("no reader panics while it takes or moves a hold")
    }
}

impl Hold {
    /// Moves the hold to the segments from the one starting at `seq` on, as its reader moves to that segment.
    pub fn at(&self, seq: u64) {
        self.holds.lock().from.insert(self.id, seq);
    }
}

impl Drop for Hold {
    fn drop(&mut self) {
        self.holds.lock().from.remove(&self.id);
    }
}

impl PartialEq for Holds {
    /// Always: holds are not part of what a view lists.
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Holds {}

impl std::fmt::Debug for Holds {
    /// The oldest held seq.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Holds").field(&self.oldest()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_segment_is_removed_only_while_no_reader_holds_it_or_one_before_it() {
        let holds = Holds::default();
        let reader = holds.hold(8);
        assert_eq!(holds.unless_held(7, || "removed"), Some("removed"), "a segment before the hold");
        assert_eq!(holds.unless_held(8, || "removed"), None, "the held segment");
        assert_eq!(holds.unless_held(11, || "removed"), None, "a segment holding a seq past the hold's start");
        reader.at(12);
        assert_eq!(holds.oldest(), Some(12));
        assert_eq!(holds.unless_held(11, || "removed"), Some("removed"), "the reader moved past it");
        let other = holds.hold(4);
        assert_eq!(holds.unless_held(5, || "removed"), None, "a second reader's hold");
        drop(other);
        drop(reader);
        assert_eq!(holds.oldest(), None);
        assert_eq!(holds.unless_held(u64::MAX, || "removed"), Some("removed"), "every hold released");
    }
}
