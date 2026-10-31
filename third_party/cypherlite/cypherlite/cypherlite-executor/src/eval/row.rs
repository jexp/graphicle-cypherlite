//! Compact query row: one shared name schema, one value per slot.
//!
//! Rows produced by the same clause share the schema `Arc`. Cloning a row
//! clones the value vector and the schema pointer, not a fresh copy of
//! every variable name.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use cypherlite_core::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
struct RowSchema {
    names: Vec<String>,
    index: HashMap<String, usize>,
}

impl RowSchema {
    fn empty() -> Arc<Self> {
        static EMPTY: OnceLock<Arc<RowSchema>> = OnceLock::new();
        Arc::clone(EMPTY.get_or_init(|| {
            Arc::new(RowSchema {
                names: Vec::new(),
                index: HashMap::new(),
            })
        }))
    }

    fn from_names(names: &[String]) -> Arc<Self> {
        let mut index = HashMap::with_capacity(names.len());
        for (slot, name) in names.iter().enumerate() {
            index.insert(name.clone(), slot);
        }
        Arc::new(Self {
            names: names.to_vec(),
            index,
        })
    }
}

/// The shared column names of one projected batch.
#[derive(Clone)]
pub struct RowNames(Arc<RowSchema>);

impl RowNames {
    pub fn new(names: &[String]) -> Self {
        Self(RowSchema::from_names(names))
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    schema: Arc<RowSchema>,
    values: Vec<Value>,
}

impl Default for Row {
    fn default() -> Self {
        Self::new()
    }
}

impl Row {
    pub fn new() -> Self {
        Self {
            schema: RowSchema::empty(),
            values: Vec::new(),
        }
    }

    /// Every value sits in `names`'s order. Rows filled from the same
    /// `RowNames` share the schema allocation.
    pub fn from_names(names: &RowNames, values: Vec<Value>) -> Self {
        debug_assert_eq!(names.0.names.len(), values.len());
        Self {
            schema: Arc::clone(&names.0),
            values,
        }
    }

    /// Remove `key` without preserving the order of the names after it.
    /// Matches `IndexMap::swap_remove`.
    pub fn swap_remove(&mut self, key: &str) -> Option<Value> {
        let slot = *self.schema.index.get(key)?;
        let last = self.values.len() - 1;
        self.schema_mut();
        let removed = self.values.swap_remove(slot);
        let name = self.schema_mut().names.swap_remove(slot);
        self.schema_mut().index.remove(&name);
        if slot != last {
            let swapped = self.schema.names[slot].clone();
            self.schema_mut().index.insert(swapped, slot);
        }
        Some(removed)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn values(&self) -> &[Value] {
        &self.values
    }

    /// Identity of the shared schema allocation. Rows from one clause
    /// compare equal here, so a workspace walk charges that schema once.
    pub(crate) fn schema_ptr(&self) -> *const () {
        Arc::as_ptr(&self.schema) as *const ()
    }

    /// Bytes of this row's schema: the name vector, the index map, and
    /// both copies of each column name.
    pub(crate) fn schema_bytes(&self) -> u64 {
        use crate::memory::{reserved_len, OVERHEAD_MAP_ENTRY, VEC_HEADER};

        let names = self.schema.names.len();
        let mut bytes = if names == 0 {
            0
        } else {
            VEC_HEADER + reserved_len(names) as u64 * std::mem::size_of::<String>() as u64
        };
        bytes += self.schema.index.len() as u64 * OVERHEAD_MAP_ENTRY;
        for name in &self.schema.names {
            bytes =
                bytes.saturating_add(cypherlite_core::str_logical_bytes(name).saturating_mul(2));
        }
        bytes
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.schema.index.get(key).map(|slot| &self.values[*slot])
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        let slot = *self.schema.index.get(key)?;
        Some(&mut self.values[slot])
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.schema.index.contains_key(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.schema.names.iter()
    }

    /// Point every row with the same column order at one schema.
    ///
    /// A match binds each output by cloning a seed and inserting a column,
    /// and that insert copies the schema. The copies are identical, so one
    /// map serves the whole batch. A later insert still copies on write.
    pub(crate) fn share_schemas(rows: &mut [Row]) {
        let Some(first) = rows.first() else {
            return;
        };
        let names = first.schema.names.clone();
        if rows.iter().skip(1).any(|row| row.schema.names != names) {
            return;
        }
        let shared = Arc::clone(&first.schema);
        for row in rows.iter_mut().skip(1) {
            row.schema = Arc::clone(&shared);
        }
    }

    pub fn insert(&mut self, key: String, value: Value) -> Option<Value> {
        if let Some(slot) = self.schema.index.get(&key).copied() {
            return Some(std::mem::replace(&mut self.values[slot], value));
        }
        let mut names = self.schema.names.clone();
        names.push(key);
        self.schema = interned_schema(&names);
        self.values.push(value);
        None
    }

    /// Remove `key`, keeping the order of the names that remain.
    pub fn shift_remove(&mut self, key: &str) -> Option<Value> {
        let slot = *self.schema.index.get(key)?;
        let removed = std::mem::replace(&mut self.values[slot], Value::Null);
        let keep: Vec<usize> = (0..self.values.len()).filter(|i| *i != slot).collect();
        self.rebuild(keep);
        Some(removed)
    }

    /// Move `key`'s value out, leaving null in the slot. The schema stays
    /// shared with every other row from the same clause.
    pub fn take_value(&mut self, key: &str) -> Option<Value> {
        let slot = *self.schema.index.get(key)?;
        Some(std::mem::replace(&mut self.values[slot], Value::Null))
    }

    pub fn retain(&mut self, mut keep: impl FnMut(&String, &mut Value) -> bool) {
        let mut kept = Vec::with_capacity(self.values.len());
        for (slot, value) in self.values.iter_mut().enumerate() {
            if keep(&self.schema.names[slot], value) {
                kept.push(slot);
            }
        }
        if kept.len() != self.values.len() {
            self.rebuild(kept);
        }
    }

    pub fn entry(&mut self, key: String) -> RowEntry<'_> {
        let slot = self.schema.index.get(&key).copied();
        RowEntry {
            row: self,
            key,
            slot,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.schema.names.iter().zip(self.values.iter())
    }

    fn schema_mut(&mut self) -> &mut RowSchema {
        if Arc::get_mut(&mut self.schema).is_none() {
            self.schema = Arc::new((*self.schema).clone());
        }
        Arc::get_mut(&mut self.schema).expect("schema was just made unique")
    }

    fn rebuild(&mut self, keep: Vec<usize>) {
        let mut names = Vec::with_capacity(keep.len());
        let mut values = Vec::with_capacity(keep.len());
        for slot in keep {
            names.push(self.schema.names[slot].clone());
            values.push(std::mem::replace(&mut self.values[slot], Value::Null));
        }
        self.schema = RowSchema::from_names(&names);
        self.values = values;
    }
}

/// One schema per column list on this thread. Match builds a row per
/// candidate by inserting the same columns, and a private map per row was
/// most of the create family's resident bytes. The cache is cleared past
/// 128 shapes so a long-lived worker cannot keep every list it has seen.
fn interned_schema(names: &[String]) -> Arc<RowSchema> {
    thread_local! {
        static CACHE: RefCell<HashMap<Vec<String>, Arc<RowSchema>>> =
            RefCell::new(HashMap::new());
    }
    CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        if let Some(hit) = cache.get(names) {
            return Arc::clone(hit);
        }
        let schema = RowSchema::from_names(names);
        if cache.len() >= 128 {
            cache.clear();
        }
        cache.insert(names.to_vec(), Arc::clone(&schema));
        schema
    })
}

pub struct RowEntry<'a> {
    row: &'a mut Row,
    key: String,
    slot: Option<usize>,
}

impl<'a> RowEntry<'a> {
    pub fn or_insert(self, value: Value) -> &'a mut Value {
        self.or_insert_with(|| value)
    }

    pub fn or_insert_with<F>(self, make: F) -> &'a mut Value
    where
        F: FnOnce() -> Value,
    {
        if let Some(slot) = self.slot {
            return &mut self.row.values[slot];
        }
        self.row.insert(self.key, make());
        let slot = self.row.values.len() - 1;
        &mut self.row.values[slot]
    }
}

impl FromIterator<(String, Value)> for Row {
    fn from_iter<I: IntoIterator<Item = (String, Value)>>(iter: I) -> Self {
        let mut row = Row::new();
        for (key, value) in iter {
            row.insert(key, value);
        }
        row
    }
}

impl<const N: usize> From<[(String, Value); N]> for Row {
    fn from(entries: [(String, Value); N]) -> Self {
        entries.into_iter().collect()
    }
}

impl std::ops::Index<&str> for Row {
    type Output = Value;

    fn index(&self, key: &str) -> &Value {
        self.get(key)
            .unwrap_or_else(|| panic!("no entry found for key {key:?}"))
    }
}

impl<'a> IntoIterator for &'a Row {
    type Item = (&'a String, &'a Value);
    type IntoIter = std::iter::Zip<std::slice::Iter<'a, String>, std::slice::Iter<'a, Value>>;

    fn into_iter(self) -> Self::IntoIter {
        self.schema.names.iter().zip(self.values.iter())
    }
}

impl IntoIterator for Row {
    type Item = (String, Value);
    type IntoIter = std::vec::IntoIter<(String, Value)>;

    fn into_iter(self) -> Self::IntoIter {
        self.schema
            .names
            .iter()
            .cloned()
            .zip(self.values)
            .collect::<Vec<_>>()
            .into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_inserted_with_the_same_columns_share_a_schema() {
        let mut rows = Vec::new();
        for i in 0..32 {
            let mut row = Row::new();
            row.insert("s".into(), Value::Int(i));
            rows.push(row);
        }
        let ptr = rows[0].schema_ptr();
        assert!(rows.iter().all(|row| row.schema_ptr() == ptr));
        assert_eq!(rows[7].get("s"), Some(&Value::Int(7)));
    }
}
