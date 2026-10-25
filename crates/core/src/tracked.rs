//! Collections that record every key handed out for change, so the state root rehashes only those
//! entries. Reads go through `Deref` to the inner collection; there is no `DerefMut`, so every
//! change goes through a method here and is recorded.

use std::collections::btree_map::{Entry, IterMut};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;

use serde::{Deserialize, Serialize};

/// A `BTreeMap` that records the keys of entries handed out for change. It serializes as the map.
#[derive(Debug, Serialize, Deserialize)]
#[serde(transparent, bound(deserialize = "K: Deserialize<'de>, V: Deserialize<'de>"))]
pub struct TrackedMap<K: Ord, V> {
    map: BTreeMap<K, V>,
    #[serde(skip)]
    touched: BTreeSet<K>,
}

/// A `BTreeSet` that records the values inserted into it. It serializes as the set.
#[derive(Debug, Serialize, Deserialize)]
#[serde(transparent, bound(deserialize = "T: Deserialize<'de>"))]
pub struct TrackedSet<T: Ord> {
    set: BTreeSet<T>,
    #[serde(skip)]
    touched: BTreeSet<T>,
}

impl<K: Ord, V> Default for TrackedMap<K, V> {
    /// An empty map.
    fn default() -> Self {
        Self { map: BTreeMap::new(), touched: BTreeSet::new() }
    }
}

impl<K: Ord, V> Deref for TrackedMap<K, V> {
    type Target = BTreeMap<K, V>;

    /// The map, read-only.
    fn deref(&self) -> &BTreeMap<K, V> {
        &self.map
    }
}

impl<T: Ord> Default for TrackedSet<T> {
    /// An empty set.
    fn default() -> Self {
        Self { set: BTreeSet::new(), touched: BTreeSet::new() }
    }
}

impl<T: Ord> Deref for TrackedSet<T> {
    type Target = BTreeSet<T>;

    /// The set, read-only.
    fn deref(&self) -> &BTreeSet<T> {
        &self.set
    }
}

impl<K: Ord + Clone, V> TrackedMap<K, V> {
    /// Removes an entry and records its key for deleting the committed leaf.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.touched.insert(key.clone());
        self.map.remove(key)
    }

    /// The entry at `key` for change, recording `key` whether or not it is present.
    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.touched.insert(key.clone());
        self.map.get_mut(key)
    }

    /// The entry at `key`, recording `key`.
    pub fn entry(&mut self, key: K) -> Entry<'_, K, V> {
        self.touched.insert(key.clone());
        self.map.entry(key)
    }

    /// Inserts `value` at `key`, recording `key`.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.touched.insert(key.clone());
        self.map.insert(key, value)
    }

    /// Removes an entry, recording the leaf that must disappear from the state tree.
    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.touched.insert(key.clone());
        self.map.remove(key)
    }

    /// Every entry for change, recording every key; for maps that stay small, such as markets.
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        self.touched.extend(self.map.keys().cloned());
        self.map.iter_mut()
    }

    /// Whether the map is empty, for `skip_serializing_if`.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Removes and returns every entry, recording every key.
    pub fn take_all(&mut self) -> BTreeMap<K, V> {
        self.touched.extend(self.map.keys().cloned());
        std::mem::take(&mut self.map)
    }

    /// The keys recorded since the last call, clearing the record.
    pub(crate) fn take_touched(&mut self) -> BTreeSet<K> {
        std::mem::take(&mut self.touched)
    }
}

impl<T: Ord + Clone> TrackedSet<T> {
    /// Whether the set is empty, for omitting inactive protocol state from serialization.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// Inserts `value`, recording it; returns whether it was new.
    pub fn insert(&mut self, value: T) -> bool {
        self.touched.insert(value.clone());
        self.set.insert(value)
    }

    /// The values recorded since the last call, clearing the record.
    pub(crate) fn take_touched(&mut self) -> BTreeSet<T> {
        std::mem::take(&mut self.touched)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_change_is_recorded_and_reads_are_not() {
        let mut map: TrackedMap<u32, u32> = TrackedMap::default();
        map.insert(1, 10);
        *map.entry(2).or_default() += 1;
        let _ = map.get_mut(&3);
        assert_eq!(map.get(&1), Some(&10));
        assert_eq!(map.take_touched(), BTreeSet::from([1, 2, 3]));
        assert_eq!(map.get(&2), Some(&1));
        assert!(map.take_touched().is_empty());
        map.iter_mut().for_each(|(_, value)| *value += 1);
        assert_eq!(map.take_touched(), BTreeSet::from([1, 2]));
        assert_eq!(map.take_all(), BTreeMap::from([(1, 11), (2, 2)]));
        assert!(map.is_empty());
        assert_eq!(map.take_touched(), BTreeSet::from([1, 2]));

        let mut set: TrackedSet<String> = TrackedSet::default();
        assert!(set.insert("a".into()));
        assert!(!set.insert("a".into()));
        assert!(set.contains("a"));
        assert_eq!(set.take_touched(), BTreeSet::from(["a".to_owned()]));
    }

    #[test]
    fn they_serialize_as_the_plain_collections() {
        let mut map: TrackedMap<u32, u32> = TrackedMap::default();
        map.insert(1, 10);
        let plain = BTreeMap::from([(1, 10)]);
        assert_eq!(serde_json::to_string(&map).unwrap(), serde_json::to_string(&plain).unwrap());
        let back: TrackedMap<u32, u32> = serde_json::from_str(r#"{"1":10}"#).unwrap();
        assert_eq!(*back, plain);

        let mut set: TrackedSet<String> = TrackedSet::default();
        set.insert("b".into());
        set.insert("a".into());
        assert_eq!(serde_json::to_string(&set).unwrap(), r#"["a","b"]"#);
    }
}
