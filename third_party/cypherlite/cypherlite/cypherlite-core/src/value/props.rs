//! Fitted property map. A `BTreeMap` leaf reserves eleven key/value slots,
//! so a two-property node keeps about 800 bytes of empty capacity. Create
//! scratch retains one map per new node for the whole statement, and that
//! slack dominated the create token-to-RSS ratio. This map stores the
//! occupied entries in sorted order and fits the buffer to the next power
//! of two, the same reservation the workspace meter charges.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::de::Deserializer;
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Serialize};

use super::Value;

/// Sorted `(key, value)` entries with capacity fitted to
/// [`reserved_slot_count`].
#[derive(Clone, Debug, PartialEq, Default)]
pub struct PropertyMap {
    entries: Vec<(String, Value)>,
}

impl PropertyMap {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Buffer slots actually reserved. Empty maps reserve nothing.
    pub fn slot_capacity(&self) -> usize {
        self.entries.capacity()
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.index_of(key).map(|i| &self.entries[i].1)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.index_of(key).is_some()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.entries.iter().map(|(key, value)| (key, value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.entries.iter().map(|(key, _)| key)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Value> {
        self.entries.iter_mut().map(|(_, value)| value)
    }

    pub fn insert(&mut self, key: String, value: Value) -> Option<Value> {
        match self.index_of(&key) {
            Some(i) => Some(std::mem::replace(&mut self.entries[i].1, value)),
            None => {
                let i = self
                    .entries
                    .partition_point(|(have, _)| have.as_str() < key.as_str());
                self.entries.insert(i, (key, value));
                fit_entries(&mut self.entries);
                None
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let i = self.index_of(key)?;
        let (_, value) = self.entries.remove(i);
        fit_entries(&mut self.entries);
        Some(value)
    }

    fn index_of(&self, key: &str) -> Option<usize> {
        self.entries
            .binary_search_by(|(have, _)| have.as_str().cmp(key))
            .ok()
    }
}

impl From<PropertyMap> for BTreeMap<String, Value> {
    fn from(map: PropertyMap) -> Self {
        map.entries.into_iter().collect()
    }
}

impl PartialEq<BTreeMap<String, Value>> for PropertyMap {
    fn eq(&self, other: &BTreeMap<String, Value>) -> bool {
        self.len() == other.len()
            && self
                .iter()
                .all(|(key, value)| other.get(key) == Some(value))
    }
}

impl std::ops::Index<&str> for PropertyMap {
    type Output = Value;

    fn index(&self, key: &str) -> &Self::Output {
        self.get(key)
            .unwrap_or_else(|| panic!("missing property {key}"))
    }
}

impl From<BTreeMap<String, Value>> for PropertyMap {
    fn from(map: BTreeMap<String, Value>) -> Self {
        let mut entries: Vec<(String, Value)> = map.into_iter().collect();
        fit_entries(&mut entries);
        Self { entries }
    }
}

/// `Arc` is a fundamental type, so `From<BTreeMap> for Arc<PropertyMap>`
/// is an orphan impl. Callers wrap with [`PropertyMap::share`].
impl PropertyMap {
    pub fn share(map: impl Into<PropertyMap>) -> Arc<Self> {
        Arc::new(map.into())
    }
}

impl<'a> IntoIterator for &'a PropertyMap {
    type Item = (&'a String, &'a Value);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (String, Value)>,
        fn(&'a (String, Value)) -> (&'a String, &'a Value),
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.iter().map(|(key, value)| (key, value))
    }
}

impl Serialize for PropertyMap {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, value) in &self.entries {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for PropertyMap {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(PropertyMap::from(BTreeMap::<String, Value>::deserialize(
            deserializer,
        )?))
    }
}

/// `0` for an empty map, otherwise the next power of two. Matches the
/// workspace meter's vector reservation.
pub fn reserved_slot_count(len: usize) -> usize {
    if len == 0 {
        0
    } else {
        len.next_power_of_two()
    }
}

fn fit_entries(entries: &mut Vec<(String, Value)>) {
    let want = reserved_slot_count(entries.len());
    if entries.capacity() == want {
        return;
    }
    let mut fitted = Vec::with_capacity(want);
    fitted.append(entries);
    if fitted.capacity() > want {
        fitted.shrink_to(want);
    }
    *entries = fitted;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_entries_reserve_two_slots() {
        let map = PropertyMap::from(BTreeMap::from([
            ("payload".to_string(), Value::Int(1)),
            ("id".to_string(), Value::Int(2)),
        ]));
        assert_eq!(map.len(), 2);
        assert_eq!(map.slot_capacity(), 2);
        assert_eq!(map.get("id"), Some(&Value::Int(2)));
        assert_eq!(map.get("payload"), Some(&Value::Int(1)));
        let keys: Vec<_> = map.keys().map(String::as_str).collect();
        assert_eq!(keys, ["id", "payload"]);
    }

    #[test]
    fn growing_past_two_slots_fits_to_four() {
        let mut map = PropertyMap::from(BTreeMap::from([
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Int(2)),
        ]));
        map.insert("c".into(), Value::Int(3));
        assert_eq!(map.slot_capacity(), 4);
        assert_eq!(map.remove("b"), Some(Value::Int(2)));
        assert_eq!(map.slot_capacity(), 2);
    }
}
