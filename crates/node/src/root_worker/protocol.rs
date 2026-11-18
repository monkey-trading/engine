//! The handshakes between the sequencer and the root worker beside their channels: the worker's
//! progress, the sequencer's wake-up once progress reaches what a deferred checkpoint waits for, the
//! flag by which a commit the sequencer waits on goes ahead of every read, and how the worker picks
//! its next step. The code is generic over the atomic words it uses, so the node runs it on `std`'s
//! atomics and the tests run the same code under loom, which checks every interleaving.

use std::sync::atomic::{AtomicU64, Ordering};

/// The bit of `Progress::finished` that says a wake-up is armed; the rest count finished units.
const ARMED: u64 = 1 << 63;

/// The atomic operations the handshakes use. The wake-up's two sides each make one
/// read-modify-write of the same word, so their order is that word's own and needs no fence.
pub(crate) trait Word: Send + Sync {
    fn new(value: u64) -> Self;
    fn load(&self) -> u64;
    fn store(&self, value: u64);
    /// Adds `value`, returning the word before.
    fn fetch_add(&self, value: u64) -> u64;
    /// Sets the bits of `value`, returning the word before.
    fn fetch_or(&self, value: u64) -> u64;
    /// Clears the bits not in `value`, returning the word before.
    fn fetch_and(&self, value: u64) -> u64;
}

/// Which kind of step the worker takes next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lane {
    /// The sequencer's messages, in the order it sent them: leaf writes, commits, releases.
    Work,
    /// Proof reads.
    Read,
    /// What no root waits for: freeing, claim captures, payout proofs.
    Background,
}

/// What the sequencer and the worker share.
pub(crate) struct Progress<W: Word = AtomicU64> {
    /// The leaf writes the worker has finished, in the units the sequencer counts them sent in, with
    /// `ARMED` set while the sequencer wants waking once they reach `wake_at`.
    finished: W,
    /// The count the armed wake-up waits for; read only with `ARMED` seen set, after it is written.
    wake_at: W,
    /// One while the sequencer waits on a commit it sent: the worker starts no read or background step
    /// until the commit is answered.
    commit_waiting: W,
}

impl Word for AtomicU64 {
    fn new(value: u64) -> Self {
        AtomicU64::new(value)
    }

    fn load(&self) -> u64 {
        AtomicU64::load(self, Ordering::Acquire)
    }

    fn store(&self, value: u64) {
        AtomicU64::store(self, value, Ordering::Release);
    }

    fn fetch_add(&self, value: u64) -> u64 {
        AtomicU64::fetch_add(self, value, Ordering::AcqRel)
    }

    fn fetch_or(&self, value: u64) -> u64 {
        AtomicU64::fetch_or(self, value, Ordering::AcqRel)
    }

    fn fetch_and(&self, value: u64) -> u64 {
        AtomicU64::fetch_and(self, value, Ordering::AcqRel)
    }
}

impl<W: Word> Default for Progress<W> {
    fn default() -> Self {
        Self { finished: W::new(0), wake_at: W::new(0), commit_waiting: W::new(0) }
    }
}

impl<W: Word> Progress<W> {
    /// The worker's side: notes `units` more finished, and returns whether that reaches an armed
    /// wake-up, which this call then disarms, so the caller wakes the sequencer once for it.
    pub(crate) fn finish(&self, units: u64) -> bool {
        let before = self.finished.fetch_add(units);
        if before & ARMED == 0 || (before & !ARMED) + units < self.wake_at.load() {
            return false;
        }
        // Whoever clears the bit owns the wake-up: here, or the sequencer finding it reached.
        self.finished.fetch_and(!ARMED) & ARMED != 0
    }

    /// The sequencer's side: arms a wake-up for when the finished count reaches `target`, and returns
    /// whether it already has, in which case nothing stays armed. Arming reads the count in the same
    /// step, so progress made at any moment either wakes the caller or is seen here.
    pub(crate) fn wake_when(&self, target: u64) -> bool {
        self.wake_at.store(target);
        let before = self.finished.fetch_or(ARMED);
        if before & !ARMED < target {
            return false;
        }
        self.finished.fetch_and(!ARMED);
        true
    }

    /// Leaf writes the worker has finished.
    pub(crate) fn finished(&self) -> u64 {
        self.finished.load() & !ARMED
    }

    /// The sequencer's side, before it sends a commit and then after the commit's answer.
    pub(crate) fn set_commit_waiting(&self, waiting: bool) {
        self.commit_waiting.store(u64::from(waiting));
    }

    /// The worker's side: its next step, given what each lane holds and the lane it took last. While
    /// a commit is waited on, only the work lane, which leads to the commit, runs; otherwise the lanes
    /// take turns.
    pub(crate) fn next_lane(&self, last: Lane, work: bool, read: bool, background: bool) -> Option<Lane> {
        if self.commit_waiting.load() != 0 {
            return work.then_some(Lane::Work);
        }
        let order = match last {
            Lane::Work => [Lane::Read, Lane::Background, Lane::Work],
            Lane::Read => [Lane::Background, Lane::Work, Lane::Read],
            Lane::Background => [Lane::Work, Lane::Read, Lane::Background],
        };
        order.into_iter().find(|lane| match lane {
            Lane::Work => work,
            Lane::Read => read,
            Lane::Background => background,
        })
    }
}

#[cfg(test)]
mod tests;
