//! Owned resident workspace of one statement.
//!
//! Query peak bytes are the high-water mark of this workspace. The formula
//! charges containers the statement allocates: vector headers and
//! [`reserved_len`] capacity, one shared row schema per distinct schema,
//! inline values at their slot size, a shallow node or relationship
//! wrapper, and deep payload only for an allocation this statement created.
//! A cloned graph `Arc` contributes its slot, not the payload again.
//! [`Value::logical_bytes`](cypherlite_core::Value::logical_bytes) stays the
//! deep payload function and is not applied to borrowed values.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::mem::size_of;
use std::sync::Arc;

use cypherlite_core::{str_logical_bytes, Node, NodeId, RelId, Relationship, Value};
#[cfg(test)]
use indexmap::IndexMap;

use crate::eval::Row;
use crate::group_key::GroupKey;
use crate::memory::{reserved_len, OVERHEAD_MAP_ENTRY, VEC_HEADER};

/// Header plus reserved capacity of a vector of `len` elements. An empty
/// vector reserves nothing.
pub(crate) fn buffer_bytes(len: usize, stride: u64) -> u64 {
    if len == 0 {
        0
    } else {
        VEC_HEADER + reserved_len(len) as u64 * stride
    }
}

/// Heap this value owns. Inline values sit in their enum slot. A shared
/// `Arc` is borrowed from the graph or from another row and adds nothing.
pub(crate) fn owned_heap(value: &Value) -> u64 {
    match value {
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Temporal(_)
        | Value::Point(_)
        | Value::Uuid(_) => 0,
        Value::String(text) => {
            if Arc::strong_count(text) == 1 {
                str_logical_bytes(text)
            } else {
                0
            }
        }
        Value::Bytes(bytes) => {
            if Arc::strong_count(bytes) == 1 {
                16 + bytes.len() as u64
            } else {
                0
            }
        }
        Value::List(items) => unique_list(items),
        Value::Vector { values, .. } => unique_list(values),
        Value::Map(entries) => {
            if Arc::strong_count(entries) == 1 {
                map_value_heap(entries)
            } else {
                0
            }
        }
        Value::Node(node) => {
            if Arc::strong_count(node) == 1 {
                size_of::<Node>() as u64 + node_heap(node)
            } else {
                0
            }
        }
        Value::Relationship(rel) => {
            if Arc::strong_count(rel) == 1 {
                size_of::<Relationship>() as u64 + rel_heap(rel)
            } else {
                0
            }
        }
        Value::Path(path) => {
            if Arc::strong_count(path) != 1 {
                return 0;
            }
            let mut bytes = size_of::<cypherlite_core::Path>() as u64;
            bytes += buffer_bytes(path.nodes.len(), size_of::<Node>() as u64);
            bytes += buffer_bytes(path.rels.len(), size_of::<Relationship>() as u64);
            for node in &path.nodes {
                bytes += node_heap(node);
            }
            for rel in &path.rels {
                bytes += rel_heap(rel);
            }
            bytes
        }
    }
}

fn unique_list(items: &Arc<Vec<Value>>) -> u64 {
    if Arc::strong_count(items) != 1 {
        return 0;
    }
    buffer_bytes(items.len(), size_of::<Value>() as u64) + items.iter().map(owned_heap).sum::<u64>()
}

fn map_value_heap(entries: &BTreeMap<String, Value>) -> u64 {
    let mut bytes = 0;
    for (key, value) in entries {
        bytes += OVERHEAD_MAP_ENTRY;
        bytes += str_logical_bytes(key);
        bytes += owned_heap(value);
    }
    bytes
}

fn property_heap(entries: &cypherlite_core::PropertyMap) -> u64 {
    created_property_bytes(entries)
}

fn node_heap(node: &Node) -> u64 {
    let mut bytes = 0;
    if Arc::strong_count(&node.labels) == 1 {
        bytes += buffer_bytes(node.labels.len(), size_of::<String>() as u64);
        for label in node.labels.iter() {
            bytes += str_logical_bytes(label);
        }
    }
    if Arc::strong_count(&node.properties) == 1 {
        bytes += property_heap(&node.properties);
    }
    bytes
}

fn rel_heap(rel: &Relationship) -> u64 {
    let mut bytes = str_logical_bytes(&rel.rel_type);
    if Arc::strong_count(&rel.properties) == 1 {
        bytes += property_heap(&rel.properties);
    }
    bytes
}

fn row_contents(row: &Row, schemas: &mut HashSet<usize>) -> u64 {
    let mut bytes = 0;
    if schemas.insert(row.schema_ptr() as usize) {
        bytes += row.schema_bytes();
    }
    bytes += buffer_bytes(row.values().len(), size_of::<Value>() as u64);
    for value in row.values() {
        bytes += owned_heap(value);
    }
    bytes
}

fn rows_accounted<'a>(len: usize, rows: impl Iterator<Item = &'a Row>) -> u64 {
    let mut bytes = buffer_bytes(len, size_of::<Row>() as u64);
    let mut schemas = HashSet::new();
    for row in rows {
        bytes += row_contents(row, &mut schemas);
    }
    bytes
}

/// The row vector, each distinct schema once, each value vector, and the
/// heap those values own.
pub(crate) fn row_buffer_bytes(rows: &[Row]) -> u64 {
    rows_accounted(rows.len(), rows.iter())
}

/// Retained heap of a fully materialized public result. Shared payloads are
/// counted once by allocation identity, including payloads referenced by
/// more than one output row.
pub fn result_rows_bytes(rows: &[Vec<Value>]) -> u64 {
    let mut seen = HashSet::new();
    let mut bytes = buffer_bytes(rows.len(), size_of::<Vec<Value>>() as u64);
    for row in rows {
        bytes += buffer_bytes(row.len(), size_of::<Value>() as u64);
        for value in row {
            bytes += result_value_heap(value, &mut seen);
        }
    }
    bytes
}

fn mark_arc<T: ?Sized>(value: &Arc<T>, seen: &mut HashSet<usize>) -> bool {
    seen.insert(Arc::as_ptr(value) as *const () as usize)
}

fn result_value_heap(value: &Value, seen: &mut HashSet<usize>) -> u64 {
    match value {
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Temporal(_)
        | Value::Point(_)
        | Value::Uuid(_) => 0,
        Value::String(text) => {
            if mark_arc(text, seen) {
                str_logical_bytes(text)
            } else {
                0
            }
        }
        Value::Bytes(bytes) => {
            if mark_arc(bytes, seen) {
                16 + bytes.len() as u64
            } else {
                0
            }
        }
        Value::List(items) | Value::Vector { values: items, .. } => {
            if !mark_arc(items, seen) {
                return 0;
            }
            buffer_bytes(items.len(), size_of::<Value>() as u64)
                + items
                    .iter()
                    .map(|item| result_value_heap(item, seen))
                    .sum::<u64>()
        }
        Value::Map(entries) => {
            if !mark_arc(entries, seen) {
                return 0;
            }
            entries
                .iter()
                .map(|(key, value)| {
                    OVERHEAD_MAP_ENTRY + str_logical_bytes(key) + result_value_heap(value, seen)
                })
                .sum()
        }
        Value::Node(node) => {
            if !mark_arc(node, seen) {
                return 0;
            }
            size_of::<Node>() as u64 + result_node_heap(node, seen)
        }
        Value::Relationship(rel) => {
            if !mark_arc(rel, seen) {
                return 0;
            }
            size_of::<Relationship>() as u64 + result_rel_heap(rel, seen)
        }
        Value::Path(path) => {
            if !mark_arc(path, seen) {
                return 0;
            }
            size_of::<cypherlite_core::Path>() as u64
                + buffer_bytes(path.nodes.len(), size_of::<Node>() as u64)
                + buffer_bytes(path.rels.len(), size_of::<Relationship>() as u64)
                + path
                    .nodes
                    .iter()
                    .map(|node| result_node_heap(node, seen))
                    .sum::<u64>()
                + path
                    .rels
                    .iter()
                    .map(|rel| result_rel_heap(rel, seen))
                    .sum::<u64>()
        }
    }
}

fn result_node_heap(node: &Node, seen: &mut HashSet<usize>) -> u64 {
    let labels = if mark_arc(&node.labels, seen) {
        buffer_bytes(node.labels.len(), size_of::<String>() as u64)
            + node
                .labels
                .iter()
                .map(|label| str_logical_bytes(label))
                .sum::<u64>()
    } else {
        0
    };
    labels + result_property_heap(&node.properties, seen)
}

fn result_rel_heap(rel: &Relationship, seen: &mut HashSet<usize>) -> u64 {
    str_logical_bytes(&rel.rel_type) + result_property_heap(&rel.properties, seen)
}

fn result_property_heap(
    entries: &Arc<cypherlite_core::PropertyMap>,
    seen: &mut HashSet<usize>,
) -> u64 {
    if !mark_arc(entries, seen) {
        return 0;
    }
    let stride = size_of::<(String, Value)>() as u64;
    let mut bytes = if entries.slot_capacity() == 0 {
        0
    } else {
        VEC_HEADER + entries.slot_capacity() as u64 * stride
    };
    for (key, value) in entries.iter() {
        bytes += str_logical_bytes(key);
        bytes += result_value_heap(value, seen);
    }
    bytes
}

/// Input rows an aggregation still owns. Releasing a row drops that row's
/// payload; the vector's reserved slots stay until the charge is dropped,
/// because draining the vector does not free its buffer.
pub(crate) struct RetainedRows {
    bytes: u64,
    schemas: HashMap<usize, SchemaHold>,
}

struct SchemaHold {
    bytes: u64,
    rows: usize,
}

impl RetainedRows {
    pub(crate) fn measure(rows: &[Row]) -> Self {
        let mut schemas = HashMap::new();
        for row in rows {
            let hold = schemas
                .entry(row.schema_ptr() as usize)
                .or_insert(SchemaHold {
                    bytes: row.schema_bytes(),
                    rows: 0,
                });
            hold.rows += 1;
        }
        Self {
            bytes: row_buffer_bytes(rows),
            schemas,
        }
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// `row` has moved out of the input vector.
    pub(crate) fn release_row(&mut self, row: &Row) {
        let ptr = row.schema_ptr() as usize;
        if let Some(hold) = self.schemas.get_mut(&ptr) {
            hold.rows = hold.rows.saturating_sub(1);
            if hold.rows == 0 {
                self.bytes = self.bytes.saturating_sub(hold.bytes);
            }
        }
        self.bytes = self.bytes.saturating_sub(row_payload(row));
    }
}

fn row_payload(row: &Row) -> u64 {
    buffer_bytes(row.values().len(), size_of::<Value>() as u64)
        + row.values().iter().map(owned_heap).sum::<u64>()
}

/// Same walk as [`row_buffer_bytes`] for rows that are not a contiguous slice.
pub(crate) fn row_refs_bytes<'a>(rows: impl IntoIterator<Item = &'a Row>) -> u64 {
    let rows: Vec<&Row> = rows.into_iter().collect();
    rows_accounted(rows.len(), rows.into_iter())
}

/// Sort-key auxiliary: the keyed vector plus each key's value buffer.
pub(crate) fn sort_key_columns<'a>(keys: impl IntoIterator<Item = &'a [Value]>) -> u64 {
    let keys: Vec<&[Value]> = keys.into_iter().collect();
    let stride = (size_of::<Vec<Value>>() + size_of::<usize>()) as u64;
    let mut bytes = buffer_bytes(keys.len(), stride);
    for key in keys {
        bytes += buffer_bytes(key.len(), size_of::<Value>() as u64);
        for value in key {
            bytes += owned_heap(value);
        }
    }
    bytes
}

/// One expand batch: the pair vector, the rows, and each used-edge set.
pub(crate) fn binding_bytes(rows: &[(Row, HashSet<RelId>)]) -> u64 {
    let stride = (size_of::<Row>() + size_of::<HashSet<RelId>>()) as u64;
    let mut bytes = buffer_bytes(rows.len(), stride);
    let mut schemas = HashSet::new();
    for (row, used) in rows {
        bytes += row_contents(row, &mut schemas);
        bytes += buffer_bytes(used.len(), size_of::<RelId>() as u64);
    }
    bytes
}

fn group_key_heap(key: &GroupKey) -> u64 {
    key.logical_bytes()
}

/// A `HashSet<GroupKey>`: its table plus each key's accounted heap.
pub(crate) fn group_key_set_bytes(keys: &HashSet<GroupKey>) -> u64 {
    let mut bytes = buffer_bytes(keys.len(), size_of::<GroupKey>() as u64);
    for key in keys {
        bytes += group_key_heap(key);
    }
    bytes
}

/// Integer fast path of a fold distinct set.
pub(crate) fn int_set_bytes(keys: &HashSet<i64>) -> u64 {
    buffer_bytes(keys.len(), size_of::<i64>() as u64)
}

/// `RETURN` / `WITH DISTINCT` set of row keys. The hot path adds
/// [`distinct_row_insert_delta`]; this walk is the oracle.
#[cfg(test)]
pub(crate) fn distinct_row_set_bytes(keys: &HashSet<Vec<GroupKey>>) -> u64 {
    let mut bytes = buffer_bytes(keys.len(), size_of::<Vec<GroupKey>>() as u64);
    for key in keys {
        bytes += buffer_bytes(key.len(), size_of::<GroupKey>() as u64);
        for part in key {
            bytes += group_key_heap(part);
        }
    }
    bytes
}

/// Live grouped aggregation: the map, the group keys, and the retained rows.
/// The hot path updates [`GroupMapMeter`]; this walk is the oracle.
#[cfg(test)]
pub(crate) fn group_map_bytes(groups: &IndexMap<Vec<GroupKey>, Vec<Row>>) -> u64 {
    let stride = (size_of::<Vec<GroupKey>>() + size_of::<Vec<Row>>()) as u64;
    let mut bytes = buffer_bytes(groups.len(), stride);
    for (key, rows) in groups {
        bytes += buffer_bytes(key.len(), size_of::<GroupKey>() as u64);
        for part in key {
            bytes += group_key_heap(part);
        }
        bytes += row_buffer_bytes(rows);
    }
    bytes
}

/// Scratch the statement created. Property values use [`owned_heap`], so a
/// string still owned by the graph is not billed again. The property map
/// and the redo log are charged for the slots they reserve, not for a
/// `BTreeMap` leaf or a catalog-sized record. Redo shares the map `Arc`;
/// the container is still this statement's, so it is charged without a
/// strong-count check.
pub(crate) fn scratch_created_bytes(
    nodes: &HashMap<NodeId, Node>,
    rels: &HashMap<RelId, Relationship>,
    created_nodes: &HashSet<NodeId>,
    created_rels: &HashSet<RelId>,
    out_adj: &HashMap<NodeId, Vec<RelId>>,
    in_adj: &HashMap<NodeId, Vec<RelId>>,
    redo_bytes: u64,
) -> u64 {
    let mut bytes = buffer_bytes(created_nodes.len(), size_of::<NodeId>() as u64);
    bytes += buffer_bytes(created_rels.len(), size_of::<RelId>() as u64);
    for id in created_nodes {
        if let Some(node) = nodes.get(id) {
            bytes += created_node_bytes(node);
        }
    }
    for id in created_rels {
        if let Some(rel) = rels.get(id) {
            bytes += created_rel_bytes(rel);
        }
    }
    bytes += adj_map_bytes(out_adj);
    bytes += adj_map_bytes(in_adj);
    bytes += redo_bytes;
    bytes
}

fn created_node_bytes(node: &Node) -> u64 {
    let mut bytes = OVERHEAD_MAP_ENTRY + size_of::<Node>() as u64;
    bytes += buffer_bytes(node.labels.len(), size_of::<String>() as u64);
    for label in node.labels.iter() {
        bytes += str_logical_bytes(label);
    }
    bytes += created_property_bytes(&node.properties);
    bytes
}

fn created_rel_bytes(rel: &Relationship) -> u64 {
    OVERHEAD_MAP_ENTRY
        + size_of::<Relationship>() as u64
        + str_logical_bytes(&rel.rel_type)
        + created_property_bytes(&rel.properties)
}

fn created_property_bytes(entries: &cypherlite_core::PropertyMap) -> u64 {
    let stride = size_of::<(String, Value)>() as u64;
    let mut bytes = if entries.slot_capacity() == 0 {
        0
    } else {
        VEC_HEADER + entries.slot_capacity() as u64 * stride
    };
    for (key, value) in entries {
        bytes += str_logical_bytes(key);
        bytes += owned_heap(value);
    }
    bytes
}

/// Byte change of one buffer when its length moves from `old_len` to `new_len`.
pub(crate) fn buffer_growth(old_len: usize, new_len: usize, stride: u64) -> u64 {
    buffer_bytes(new_len, stride).saturating_sub(buffer_bytes(old_len, stride))
}

/// Running total for a binding batch. `push` adds one row; the total equals
/// [`binding_bytes`] of the same rows.
#[derive(Debug, Default)]
pub(crate) struct BindingMeter {
    pub bytes: u64,
    len: usize,
    schemas: HashSet<usize>,
}

/// Snapshot taken before a [`BindingMeter::push`], so a refused resize can
/// put the meter back.
pub(crate) struct BindingMark {
    bytes: u64,
    len: usize,
    schema: Option<usize>,
}

impl BindingMeter {
    pub(crate) fn push(&mut self, row: &Row, used_len: usize) -> BindingMark {
        let mark = BindingMark {
            bytes: self.bytes,
            len: self.len,
            schema: None,
        };
        let ptr = row.schema_ptr() as usize;
        let fresh = !self.schemas.contains(&ptr);
        let stride = (size_of::<Row>() + size_of::<HashSet<RelId>>()) as u64;
        let old = self.len;
        self.len += 1;
        self.bytes += buffer_growth(old, self.len, stride);
        self.bytes += row_contents(row, &mut self.schemas);
        self.bytes += buffer_bytes(used_len, size_of::<RelId>() as u64);
        BindingMark {
            schema: fresh.then_some(ptr),
            ..mark
        }
    }

    pub(crate) fn unpush(&mut self, mark: BindingMark) {
        self.bytes = mark.bytes;
        self.len = mark.len;
        if let Some(ptr) = mark.schema {
            self.schemas.remove(&ptr);
        }
    }
}

#[derive(Debug, Default)]
struct GroupSlot {
    rows: usize,
    schemas: HashSet<usize>,
}

impl GroupSlot {
    fn add_row(&mut self, row: &Row) -> u64 {
        let old = self.rows;
        self.rows += 1;
        let mut delta = buffer_growth(old, self.rows, size_of::<Row>() as u64);
        if self.schemas.insert(row.schema_ptr() as usize) {
            delta += row.schema_bytes();
        }
        delta += buffer_bytes(row.values().len(), size_of::<Value>() as u64);
        for value in row.values() {
            delta += owned_heap(value);
        }
        delta
    }
}

/// Running total for an aggregation map. Adding a group or a row matches
/// [`group_map_bytes`] of the same map.
#[derive(Debug, Default)]
pub(crate) struct GroupMapMeter {
    pub bytes: u64,
    groups: Vec<GroupSlot>,
    payload: u64,
}

impl GroupMapMeter {
    fn stride() -> u64 {
        (size_of::<Vec<GroupKey>>() + size_of::<Vec<Row>>()) as u64
    }

    fn recompute(&mut self) {
        self.bytes = buffer_bytes(self.groups.len(), Self::stride()) + self.payload;
    }

    pub(crate) fn add_group(&mut self, key: &[GroupKey], row: &Row) {
        let mut slot = GroupSlot::default();
        let row_bytes = slot.add_row(row);
        self.groups.push(slot);
        self.payload += key_account(key) + row_bytes;
        self.recompute();
    }

    pub(crate) fn add_row(&mut self, index: usize, row: &Row) {
        self.payload += self.groups[index].add_row(row);
        self.recompute();
    }

    /// The keyless group aggregation inserts before any input row arrives.
    pub(crate) fn add_empty_group(&mut self) {
        self.groups.push(GroupSlot::default());
        self.recompute();
    }
}

fn key_account(key: &[GroupKey]) -> u64 {
    buffer_bytes(key.len(), size_of::<GroupKey>() as u64)
        + key.iter().map(GroupKey::logical_bytes).sum::<u64>()
}

pub(crate) fn distinct_row_insert_delta(old_len: usize, key: &[GroupKey]) -> u64 {
    buffer_growth(old_len, old_len + 1, size_of::<Vec<GroupKey>>() as u64) + key_account(key)
}

pub(crate) fn group_key_insert_delta(old_len: usize, key: &GroupKey) -> u64 {
    buffer_growth(old_len, old_len + 1, size_of::<GroupKey>() as u64) + key.logical_bytes()
}

pub(crate) fn int_insert_delta(old_len: usize) -> u64 {
    buffer_growth(old_len, old_len + 1, size_of::<i64>() as u64)
}

pub(crate) fn created_node_insert_delta(existing: usize, node: &Node) -> u64 {
    buffer_growth(existing, existing + 1, size_of::<NodeId>() as u64) + created_node_bytes(node)
}

pub(crate) fn created_node_remove_delta(remaining: usize, node: &Node) -> u64 {
    buffer_growth(remaining, remaining + 1, size_of::<NodeId>() as u64) + created_node_bytes(node)
}

pub(crate) fn created_rel_insert_delta(
    existing: usize,
    rel: &Relationship,
    out_len: Option<usize>,
    in_len: Option<usize>,
) -> u64 {
    buffer_growth(existing, existing + 1, size_of::<RelId>() as u64)
        + created_rel_bytes(rel)
        + adj_len_delta(out_len)
        + adj_len_delta(in_len)
}

/// Deleting a created relationship drops its record and id-set slot. The
/// adjacency lists keep the id, matching [`delete_rel`].
pub(crate) fn created_rel_remove_delta(remaining: usize, rel: &Relationship) -> u64 {
    buffer_growth(remaining, remaining + 1, size_of::<RelId>() as u64) + created_rel_bytes(rel)
}

fn adj_len_delta(existing: Option<usize>) -> u64 {
    match existing {
        None => OVERHEAD_MAP_ENTRY + buffer_bytes(1, size_of::<RelId>() as u64),
        Some(len) => buffer_growth(len, len + 1, size_of::<RelId>() as u64),
    }
}

pub(crate) fn created_property_delta(entries: &cypherlite_core::PropertyMap) -> u64 {
    created_property_bytes(entries)
}

pub(crate) fn created_label_bytes(labels: &[String]) -> u64 {
    buffer_bytes(labels.len(), size_of::<String>() as u64)
        + labels
            .iter()
            .map(|label| str_logical_bytes(label))
            .sum::<u64>()
}

fn adj_map_bytes(adj: &HashMap<NodeId, Vec<RelId>>) -> u64 {
    let mut bytes = adj.len() as u64 * OVERHEAD_MAP_ENTRY;
    for rels in adj.values() {
        bytes += buffer_bytes(rels.len(), size_of::<RelId>() as u64);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cypherlite_core::Value;

    use super::*;
    use crate::eval::RowNames;

    #[test]
    fn shared_string_adds_no_deep_heap_and_a_unique_string_does() {
        let shared = Value::String(Arc::from("payload-text"));
        let cloned = shared.clone();
        assert_eq!(owned_heap(&shared), 0);
        assert_eq!(owned_heap(&cloned), 0);
        let unique = Value::String(Arc::from("payload-text"));
        assert_eq!(owned_heap(&unique), str_logical_bytes("payload-text"));
        assert_eq!(owned_heap(&Value::Int(7)), 0);
    }

    #[test]
    fn shared_node_wrapper_adds_no_payload() {
        let node = Value::from_node(Node {
            id: 1,
            labels: Arc::new(vec!["Person".into()]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                "name".into(),
                Value::String(Arc::from("ada")),
            )])),
        });
        let cloned = node.clone();
        assert_eq!(owned_heap(&node), 0);
        assert_eq!(owned_heap(&cloned), 0);
    }

    #[test]
    fn row_buffer_counts_a_shared_schema_once_and_skips_shared_payload() {
        let names = RowNames::new(&["n".into()]);
        let payload = "x".repeat(80);
        let shared = Value::String(Arc::from(payload.as_str()));
        let row = Row::from_names(&names, vec![shared.clone()]);
        let shared_one = row_buffer_bytes(std::slice::from_ref(&row));
        let unique_one = row_buffer_bytes(&[Row::from_names(
            &names,
            vec![Value::String(Arc::from(payload.as_str()))],
        )]);
        assert_eq!(
            unique_one - shared_one,
            str_logical_bytes(&payload),
            "a unique string is workspace payload; a shared one is not"
        );
        let both = row_buffer_bytes(&[row.clone(), Row::from_names(&names, vec![shared.clone()])]);
        assert_eq!(row_buffer_bytes(&[row.clone(), row.clone()]), both);
        let other_names = RowNames::new(&["n".into()]);
        let different_schema =
            row_buffer_bytes(&[row.clone(), Row::from_names(&other_names, vec![shared])]);
        assert_eq!(different_schema - both, row.schema_bytes());
    }

    #[test]
    fn eight_hundred_inline_rows_exceed_one_token() {
        let names = RowNames::new(&["i".into()]);
        let rows: Vec<Row> = (0..800)
            .map(|i| Row::from_names(&names, vec![Value::Int(i)]))
            .collect();
        assert!(row_buffer_bytes(&rows) > cypherlite_core::tokens::CHARGE_QUANTUM_BYTES);
        assert_eq!(row_buffer_bytes(&rows), row_buffer_bytes(&rows));
    }

    #[test]
    fn distinct_duplicate_does_not_grow_the_set() {
        let mut keys = HashSet::new();
        keys.insert(GroupKey::new(&Value::Int(1)));
        let once = group_key_set_bytes(&keys);
        assert!(keys.insert(GroupKey::new(&Value::Int(2))));
        let two = group_key_set_bytes(&keys);
        assert!(two > once);
        assert!(!keys.insert(GroupKey::new(&Value::Int(2))));
        assert_eq!(group_key_set_bytes(&keys), two);

        let mut rows = HashSet::new();
        let key = vec![GroupKey::new(&Value::Int(1))];
        rows.insert(key.clone());
        let once = distinct_row_set_bytes(&rows);
        rows.insert(key);
        assert_eq!(distinct_row_set_bytes(&rows), once);
    }

    #[test]
    fn group_map_matches_a_second_walk() {
        let names = RowNames::new(&["i".into()]);
        let mut groups = IndexMap::new();
        groups.insert(
            vec![GroupKey::new(&Value::Int(1))],
            vec![Row::from_names(&names, vec![Value::Int(1)])],
        );
        let once = group_map_bytes(&groups);
        groups
            .entry(vec![GroupKey::new(&Value::Int(1))])
            .or_default()
            .push(Row::from_names(&names, vec![Value::Int(2)]));
        assert!(group_map_bytes(&groups) > once);
        assert_eq!(group_map_bytes(&groups), group_map_bytes(&groups));
    }

    #[test]
    fn retain_matches_the_row_walk_and_shrinks() {
        let names = RowNames::new(&["i".into()]);
        let rows: Vec<Row> = (0..4)
            .map(|i| Row::from_names(&names, vec![Value::Int(i)]))
            .collect();
        let walked = row_buffer_bytes(&rows);
        let quantum = cypherlite_core::tokens::CHARGE_QUANTUM_BYTES;
        let pool = Arc::new(Pool::new(100 * quantum));
        assert!(cypherlite_core::tokens::PeakPool::try_grow(
            pool.as_ref(),
            quantum
        ));
        let mut ledger =
            cypherlite_core::tokens::TokenLedger::new(100 * quantum, quantum, pool.clone());
        let _guard = cypherlite_core::tokens::LedgerGuard::install(&mut ledger);
        let mut charge = cypherlite_core::tokens::WorkspaceCharge::retain(walked).unwrap();
        let (current, peak, held) = cypherlite_core::tokens::workspace_meter().unwrap();
        assert_eq!(current, walked);
        assert_eq!(peak, walked);
        assert_eq!(
            held,
            cypherlite_core::tokens::rounded_logical_bytes(walked).max(quantum)
        );
        charge.resize(0).unwrap();
        let (current, peak, held) = cypherlite_core::tokens::workspace_meter().unwrap();
        assert_eq!(current, 0);
        assert_eq!(peak, walked);
        let after_release = if cypherlite_core::tokens::rounded_logical_bytes(walked) > quantum {
            0
        } else {
            quantum
        };
        assert_eq!(held, after_release);
        assert_eq!(pool.reserved(), after_release);
    }

    #[test]
    fn releasing_an_input_row_keeps_the_vector_reservation() {
        let names = RowNames::new(&["a".into(), "b".into()]);
        let rows: Vec<Row> = (0..17)
            .map(|i| Row::from_names(&names, vec![Value::Int(i), Value::Int(i + 1)]))
            .collect();
        let mut retained = RetainedRows::measure(&rows);
        assert_eq!(retained.bytes(), row_buffer_bytes(&rows));
        let slab = buffer_bytes(rows.len(), size_of::<Row>() as u64);
        for i in 0..rows.len() {
            retained.release_row(&rows[i]);
            let tail = row_buffer_bytes(&rows[i + 1..]);
            let tail_slab = buffer_bytes(rows.len() - i - 1, size_of::<Row>() as u64);
            assert_eq!(retained.bytes(), tail + slab - tail_slab);
        }
        assert_eq!(retained.bytes(), slab);
    }

    #[test]
    fn incremental_meters_match_full_walks() {
        let names = RowNames::new(&["i".into()]);
        let mut bindings = Vec::new();
        let mut binding_meter = BindingMeter::default();
        for i in 0..40 {
            let row = Row::from_names(&names, vec![Value::Int(i)]);
            let mut used = HashSet::new();
            if i % 2 == 0 {
                used.insert(i);
            }
            binding_meter.push(&row, used.len());
            bindings.push((row, used));
            assert_eq!(binding_meter.bytes, binding_bytes(&bindings));
        }

        let mut groups: IndexMap<Vec<GroupKey>, Vec<Row>> = IndexMap::new();
        let mut group_meter = GroupMapMeter::default();
        for i in 0..30 {
            let row = Row::from_names(&names, vec![Value::Int(i)]);
            let key = vec![GroupKey::new(&Value::Int(i % 3))];
            if let Some(index) = groups.get_index_of(&key) {
                group_meter.add_row(index, &row);
                groups.get_index_mut(index).expect("group").1.push(row);
            } else {
                group_meter.add_group(&key, &row);
                groups.insert(key, vec![row]);
            }
            assert_eq!(group_meter.bytes, group_map_bytes(&groups));
        }

        let mut empty_groups: IndexMap<Vec<GroupKey>, Vec<Row>> = IndexMap::new();
        let mut empty_meter = GroupMapMeter::default();
        empty_groups.insert(Vec::new(), Vec::new());
        empty_meter.add_empty_group();
        assert_eq!(empty_meter.bytes, group_map_bytes(&empty_groups));

        let mut distinct = HashSet::new();
        let mut distinct_total = 0;
        for i in 0..40 {
            let key = vec![
                GroupKey::new(&Value::Int(i)),
                GroupKey::new(&Value::Int(i / 2)),
            ];
            distinct_total += distinct_row_insert_delta(distinct.len(), &key);
            distinct.insert(key);
            assert_eq!(distinct_total, distinct_row_set_bytes(&distinct));
        }
        let duplicate = vec![GroupKey::new(&Value::Int(0)), GroupKey::new(&Value::Int(0))];
        assert!(!distinct.insert(duplicate));

        let mut keys = HashSet::new();
        let mut key_total = 0;
        let mut ints = HashSet::new();
        let mut int_total = 0;
        for i in 0..40 {
            let key = GroupKey::new(&Value::Int(i));
            key_total += group_key_insert_delta(keys.len(), &key);
            keys.insert(key);
            assert_eq!(key_total, group_key_set_bytes(&keys));
            int_total += int_insert_delta(ints.len());
            ints.insert(i);
            assert_eq!(int_total, int_set_bytes(&ints));
        }
    }

    #[test]
    fn scratch_deltas_match_the_created_walk() {
        let mut nodes = HashMap::new();
        let mut rels = HashMap::new();
        let mut created_nodes = HashSet::new();
        let mut created_rels = HashSet::new();
        let mut out_adj: HashMap<NodeId, Vec<RelId>> = HashMap::new();
        let mut in_adj: HashMap<NodeId, Vec<RelId>> = HashMap::new();
        let mut total = 0;
        for i in 0..12 {
            let node = Node {
                id: i,
                labels: Arc::new(vec!["N".into()]),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                    "i".into(),
                    Value::Int(i),
                )])),
            };
            total += created_node_insert_delta(created_nodes.len(), &node);
            nodes.insert(i, node);
            created_nodes.insert(i);
            assert_eq!(
                total,
                scratch_created_bytes(
                    &nodes,
                    &rels,
                    &created_nodes,
                    &created_rels,
                    &out_adj,
                    &in_adj,
                    0,
                )
            );
        }
        for i in 0..8 {
            let rel = Relationship {
                id: i,
                start: i,
                end: i + 1,
                rel_type: "R".into(),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                    "name".into(),
                    Value::String(Arc::from("edge")),
                )])),
            };
            total += created_rel_insert_delta(
                created_rels.len(),
                &rel,
                out_adj.get(&rel.start).map(Vec::len),
                in_adj.get(&rel.end).map(Vec::len),
            );
            let start = rel.start;
            let end = rel.end;
            rels.insert(i, rel);
            out_adj.entry(start).or_default().push(i);
            in_adj.entry(end).or_default().push(i);
            created_rels.insert(i);
            assert_eq!(
                total,
                scratch_created_bytes(
                    &nodes,
                    &rels,
                    &created_nodes,
                    &created_rels,
                    &out_adj,
                    &in_adj,
                    0,
                )
            );
        }
        let loop_rel = Relationship {
            id: 100,
            start: 0,
            end: 0,
            rel_type: "L".into(),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        };
        total += created_rel_insert_delta(
            created_rels.len(),
            &loop_rel,
            out_adj.get(&loop_rel.start).map(Vec::len),
            in_adj.get(&loop_rel.end).map(Vec::len),
        );
        rels.insert(100, loop_rel);
        out_adj.entry(0).or_default().push(100);
        in_adj.entry(0).or_default().push(100);
        created_rels.insert(100);
        assert_eq!(
            total,
            scratch_created_bytes(
                &nodes,
                &rels,
                &created_nodes,
                &created_rels,
                &out_adj,
                &in_adj,
                0,
            )
        );

        let before_props = created_property_delta(&nodes[&0].properties);
        std::sync::Arc::make_mut(&mut nodes.get_mut(&0).unwrap().properties)
            .insert("extra".into(), Value::String(Arc::from("created-here")));
        let after_props = created_property_delta(&nodes[&0].properties);
        total = total - before_props + after_props;
        assert_eq!(
            total,
            scratch_created_bytes(
                &nodes,
                &rels,
                &created_nodes,
                &created_rels,
                &out_adj,
                &in_adj,
                0,
            )
        );

        let before_labels = created_label_bytes(&nodes[&1].labels);
        std::sync::Arc::make_mut(&mut nodes.get_mut(&1).unwrap().labels).push("M".into());
        let after_labels = created_label_bytes(&nodes[&1].labels);
        total = total - before_labels + after_labels;
        assert_eq!(
            total,
            scratch_created_bytes(
                &nodes,
                &rels,
                &created_nodes,
                &created_rels,
                &out_adj,
                &in_adj,
                0,
            )
        );

        let rel = rels[&7].clone();
        total -= created_rel_remove_delta(created_rels.len() - 1, &rel);
        rels.remove(&7);
        created_rels.remove(&7);
        assert_eq!(
            total,
            scratch_created_bytes(
                &nodes,
                &rels,
                &created_nodes,
                &created_rels,
                &out_adj,
                &in_adj,
                0,
            )
        );

        let node = nodes[&11].clone();
        total -= created_node_remove_delta(created_nodes.len() - 1, &node);
        nodes.remove(&11);
        created_nodes.remove(&11);
        assert_eq!(
            total,
            scratch_created_bytes(
                &nodes,
                &rels,
                &created_nodes,
                &created_rels,
                &out_adj,
                &in_adj,
                0,
            )
        );
    }

    struct Pool {
        reserved: std::sync::atomic::AtomicU64,
        capacity: u64,
    }

    impl Pool {
        fn new(capacity: u64) -> Self {
            Self {
                reserved: std::sync::atomic::AtomicU64::new(0),
                capacity,
            }
        }

        fn reserved(&self) -> u64 {
            self.reserved.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl cypherlite_core::tokens::PeakPool for Pool {
        fn try_grow(&self, extra: u64) -> bool {
            let now = self.reserved();
            if now + extra > self.capacity {
                return false;
            }
            self.reserved
                .fetch_add(extra, std::sync::atomic::Ordering::SeqCst);
            true
        }

        fn release(&self, amount: u64) {
            if amount > 0 {
                self.reserved
                    .fetch_sub(amount, std::sync::atomic::Ordering::SeqCst);
            }
        }
    }
}
