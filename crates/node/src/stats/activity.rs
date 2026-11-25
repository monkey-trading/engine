//! The newest public MONKEY events, held in a bounded ring: loss allocations, stakes and unstakes.

use std::collections::VecDeque;

use engine_core::emissions::TokenUnits;
use engine_core::types::{AccountId, Millis};
use serde::{Deserialize, Deserializer, Serialize};

/// What an activity entry records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    /// An emission period's allocation to an account that lost in it.
    LossClose,
    Stake,
    Unstake,
}

/// One MONKEY event, as the log carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    /// Orders entries in time: the command's seq and the event's index in it. A client passes the
    /// last it has as `before` to page back. A decimal string, since the number passes what a
    /// JavaScript number holds exactly.
    #[serde(serialize_with = "as_text", deserialize_with = "from_text")]
    pub id: u64,
    pub seq: u64,
    pub ts: Millis,
    #[serde(rename = "type")]
    pub kind: Kind,
    pub account: AccountId,
    /// MONKEY base units.
    pub amount: TokenUnits,
    /// The emission period of a `loss_close`.
    pub period: Option<u64>,
}

/// The newest `capacity` entries. A daily snapshot carries the entries, and the restoring node's settings give
/// the capacity (`keep`).
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Activity {
    #[serde(skip)]
    capacity: usize,
    entries: VecDeque<Entry>,
    /// Whether the ring has dropped an entry for room.
    evicted: bool,
}

/// Serializes an id as a decimal string.
pub(crate) fn as_text<S: serde::Serializer>(id: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(id)
}

/// Reads an id `as_text` wrote.
/// Error cases:
/// - the value is not a decimal string of a `u64`
fn from_text<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let text = String::deserialize(deserializer)?;
    text.parse().map_err(serde::de::Error::custom)
}

/// The bits of an id that count the event within its command.
const INDEX_BITS: u32 = 24;

/// The id of event `index` of command `seq`.
pub(crate) fn id(seq: u64, index: usize) -> u64 {
    (seq << INDEX_BITS) | (index as u64).min((1 << INDEX_BITS) - 1)
}

impl Activity {
    pub(crate) fn new(capacity: usize) -> Self {
        Self { capacity, entries: VecDeque::new(), evicted: false }
    }

    /// Holds at most `capacity` entries from here, dropping the oldest past it.
    pub(super) fn keep(&mut self, capacity: usize) {
        self.capacity = capacity;
        while self.entries.len() > capacity {
            self.entries.pop_front();
            self.evicted = true;
        }
    }

    /// Adds the newest entry, dropping the oldest past capacity.
    pub(crate) fn push(&mut self, entry: Entry) {
        if self.entries.len() == self.capacity {
            self.entries.pop_front();
            self.evicted = true;
        }
        self.entries.push_back(entry);
    }

    /// Whether older entries have been dropped, and the time of the oldest one held.
    pub(crate) fn retained(&self) -> (bool, Option<Millis>) {
        (self.evicted, self.entries.front().map(|entry| entry.ts))
    }

    /// Up to `limit` entries older than `before`, newest first, and the id to pass as `before` for
    /// the page after them when more are held. Entries are held in id order, so this seeks.
    pub(crate) fn page(&self, before: Option<u64>, limit: usize) -> (Vec<Entry>, Option<u64>) {
        let end = before.map_or(self.entries.len(), |before| self.entries.partition_point(|entry| entry.id < before));
        let mut older = self.entries.range(..end).rev();
        let page: Vec<Entry> = older.by_ref().take(limit).cloned().collect();
        let next = older.next().and(page.last()).map(|last| last.id);
        (page, next)
    }
}
