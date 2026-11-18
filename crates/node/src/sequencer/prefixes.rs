//! The cumulative command prefix after each of the latest commands this sequencer applied: a
//! handover checks a successor's tip against it before it stops sequencing, so a tip that is not
//! on this log, or older than these, is refused while sequencing goes on.

use crate::log::PrefixRing;

/// How many of the latest commands' prefixes the sequencer retains: a follower's tip is a few
/// seconds behind the head when its successor asks for the tail, which this covers at a thousand
/// commands a second for over a minute. A restart's replay and a replica's start seed all of them.
pub(crate) const RETAINED_PREFIXES: usize = PrefixRing::CAPACITY;

/// A ring of `RETAINED_PREFIXES` pairs, seeded with what recovery's replay retained.
pub(super) fn retained(replayed: &PrefixRing) -> PrefixRing {
    let mut ring = PrefixRing::with_capacity(RETAINED_PREFIXES);
    ring.extend(replayed);
    ring
}
