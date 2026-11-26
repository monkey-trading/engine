//! The command links and checkpoints a replica keeps beyond its replay: the latest commands' cumulative
//! prefixes, the posted head it held when it passed it, and the links and checkpoints at the seqs it was asked
//! to keep. A replica from a daily snapshot starts with the links the snapshot carries.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::log::Checkpoint;
use crate::sequencer::replay::live::{Link, RETAINED_LINKS};

/// What a replica keeps of the commands and checkpoints it passes.
#[derive(Default)]
pub(super) struct Retained {
    /// The latest commands' links, oldest first, at consecutive seqs ending at the last command.
    pub(super) links: VecDeque<Link>,
    /// The newest posted log head the replica held when it passed, kept past `links`.
    pub(super) pinned: Option<Link>,
    /// The seqs whose command link and checkpoint the replica keeps past `links` and its checkpoints as it
    /// passes them (a promotion's fixed head and checkpoint), with what it kept.
    pub(super) keeping: BTreeSet<u64>,
    pub(super) kept: BTreeMap<u64, Link>,
    pub(super) kept_checkpoints: BTreeMap<u64, Checkpoint>,
}

impl Retained {
    /// Takes the link after the next command.
    pub(super) fn command(&mut self, link: Link) {
        self.links.push_back(link);
        if self.keeping.contains(&link.seq) {
            self.kept.insert(link.seq, link);
        }
        if self.links.len() > RETAINED_LINKS {
            self.links.pop_front();
        }
    }

    /// Takes a checkpoint the replica verified, or anchors at: kept when its seq is one to keep, the first at
    /// that seq staying.
    pub(super) fn checkpoint(&mut self, checkpoint: &Checkpoint) {
        if self.keeping.contains(&checkpoint.seq) {
            self.kept_checkpoints.entry(checkpoint.seq).or_insert_with(|| checkpoint.clone());
        }
    }

    /// The cumulative prefix after the command at `seq`, with its stamp, while it is kept: one of the latest
    /// `RETAINED_LINKS` commands, the pinned posted head, or a seq to keep.
    pub(super) fn link_at(&self, seq: u64) -> Option<Link> {
        let index = self.links.front().and_then(|first| seq.checked_sub(first.seq));
        let retained = index.and_then(|index| self.links.get(usize::try_from(index).ok()?));
        retained.copied().or(self.pinned.filter(|pinned| pinned.seq == seq)).or_else(|| self.kept.get(&seq).copied())
    }

    /// Keeps the posted head `(seq, link)` once the link held at `seq` is that one (see `LiveReplay::pin`).
    pub(super) fn pin(&mut self, seq: u64, link: engine_core::eip712::B256) -> Option<bool> {
        let held = self.link_at(seq)?;
        if held.link == link {
            self.pinned = Some(held);
        }
        Some(held.link == link)
    }
}
