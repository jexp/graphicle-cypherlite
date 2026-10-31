//! Compact live records and typed adjacency chunks (ADR-0033).
//! Public `Node` / `Relationship` maps are materialized at the API boundary.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use cypherlite_core::{Error, Node, NodeId, RelId, Relationship, Result, Value};
use cypherlite_ir::Direction;
use dashmap::DashMap;

/// Other endpoint of `rid` given `current` and direction.
pub(crate) fn hop_other(
    current: NodeId,
    start: NodeId,
    end: NodeId,
    dir: Direction,
) -> Option<NodeId> {
    match dir {
        Direction::Outgoing => (start == current).then_some(end),
        Direction::Incoming => (end == current).then_some(start),
        Direction::Both => {
            if start == current {
                Some(end)
            } else if end == current {
                Some(start)
            } else {
                None
            }
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct Interner {
    to_id: DashMap<Arc<str>, u32>,
    /// Dense, append-only id→string table. Entries are never removed or
    /// reassigned, so a read for a committed id is a plain shard lookup; only
    /// `intern` mutates (on first sight of a string).
    to_str: DashMap<u32, Arc<str>>,
    /// Monotonic id source. Separate from `to_str.len()` so two concurrent
    /// first-time interns of distinct strings cannot observe the same len.
    next_id: AtomicU32,
    /// Bytes of distinct strings. `Arc` clones are not added again.
    bytes: std::sync::atomic::AtomicU64,
    /// One label vector per distinct label-id list. Walk stubs clone the
    /// `Arc` instead of rebuilding the strings.
    label_vecs: DashMap<Vec<u32>, Arc<Vec<String>>>,
}

/// The empty property map every node and relationship stub shares.
pub(crate) fn empty_properties() -> Arc<cypherlite_core::PropertyMap> {
    static EMPTY: OnceLock<Arc<cypherlite_core::PropertyMap>> = OnceLock::new();
    Arc::clone(EMPTY.get_or_init(|| Arc::new(cypherlite_core::PropertyMap::new())))
}

impl Interner {
    pub(crate) fn intern(&self, s: &str) -> u32 {
        if let Some(id) = self.to_id.get(s) {
            return *id;
        }
        // `entry` holds the shard write lock, so a concurrent `intern` of the
        // same string takes the occupied arm and both callers get one id.
        match self.to_id.entry(Arc::from(s)) {
            dashmap::mapref::entry::Entry::Occupied(e) => *e.get(),
            dashmap::mapref::entry::Entry::Vacant(e) => {
                let arc = e.key().clone();
                let id = self.next_id.fetch_add(1, Ordering::Relaxed);
                // Publish id→str before the id is reachable via `to_id`, so a
                // `get(id)` that observes the id always finds the string.
                self.to_str.insert(id, arc);
                e.insert(id);
                self.bytes.fetch_add(
                    crate::memory::interned_string_bytes(s.len()),
                    Ordering::Relaxed,
                );
                id
            }
        }
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    pub(crate) fn get_id(&self, s: &str) -> Option<u32> {
        self.to_id.get(s).map(|v| *v)
    }

    /// Reverse lookup of an interned id. Every reachable id was published
    /// by [`Interner::intern`] before use, so a miss is a bug, not data:
    /// panic instead of silently returning the empty string.
    pub(crate) fn get(&self, id: u32) -> Arc<str> {
        self.to_str
            .get(&id)
            .map(|v| Arc::clone(&*v))
            .expect("interned id is published before use")
    }

    /// The label strings for `ids`, shared with every other stub of the
    /// same label list.
    pub(crate) fn cached_labels(&self, ids: &[u32]) -> Arc<Vec<String>> {
        if let Some(hit) = self.label_vecs.get(ids) {
            return Arc::clone(hit.value());
        }
        let labels = Arc::new(
            ids.iter()
                .map(|id| String::from(&*self.get(*id)))
                .collect::<Vec<_>>(),
        );
        match self.label_vecs.entry(ids.to_vec()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => Arc::clone(entry.get()),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let string_bytes = labels.iter().map(|label| label.len() as u64).sum();
                self.bytes.fetch_add(
                    crate::memory::label_vec_cache_bytes(ids.len(), string_bytes),
                    Ordering::Relaxed,
                );
                entry.insert(Arc::clone(&labels));
                labels
            }
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CompactProps {
    keys: Vec<u32>,
    vals: Vec<Value>,
}

impl CompactProps {
    pub(crate) fn from_map(interner: &Interner, map: BTreeMap<String, Value>) -> Self {
        let mut keys = Vec::with_capacity(map.len());
        let mut vals = Vec::with_capacity(map.len());
        for (k, v) in map {
            keys.push(interner.intern(&k));
            vals.push(v);
        }
        let mut props = Self { keys, vals };
        props.fit();
        props
    }

    pub(crate) fn to_map(&self, interner: &Interner) -> BTreeMap<String, Value> {
        let mut m = BTreeMap::new();
        for (k, v) in self.keys.iter().zip(self.vals.iter()) {
            m.insert(String::from(&*interner.get(*k)), v.clone());
        }
        m
    }

    pub(crate) fn get(&self, key: u32) -> Option<&Value> {
        self.keys
            .iter()
            .position(|&k| k == key)
            .map(|i| &self.vals[i])
    }

    pub(crate) fn len(&self) -> usize {
        self.keys.len()
    }

    #[cfg(test)]
    pub(crate) fn key_capacity(&self) -> usize {
        self.keys.capacity()
    }

    #[cfg(test)]
    pub(crate) fn val_capacity(&self) -> usize {
        self.vals.capacity()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (u32, &Value)> {
        self.keys.iter().copied().zip(self.vals.iter())
    }

    fn fit(&mut self) {
        crate::memory::fit_reserved(&mut self.keys);
        crate::memory::fit_reserved(&mut self.vals);
    }

    pub(crate) fn keys_as_strings(&self, interner: &Interner) -> Vec<String> {
        self.keys
            .iter()
            .map(|k| String::from(&*interner.get(*k)))
            .collect()
    }

    pub(crate) fn set(&mut self, key: u32, value: Value) {
        if let Some(i) = self.keys.iter().position(|&k| k == key) {
            self.vals[i] = value;
        } else {
            self.keys.push(key);
            self.vals.push(value);
            self.fit();
        }
    }

    pub(crate) fn remove(&mut self, key: u32) {
        if let Some(i) = self.keys.iter().position(|&k| k == key) {
            self.keys.remove(i);
            self.vals.remove(i);
            self.fit();
        }
    }

    /// Tagged JSON byte length of this map, borrowing each value.
    pub(crate) fn tagged_json_len(&self, interner: &Interner) -> Result<u64> {
        json_len(&BorrowedProps {
            interner,
            props: self,
        })
    }
}

struct ByteCount(u64);

impl Write for ByteCount {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(buf.len() as u64)
            .ok_or_else(|| std::io::Error::other("checkpoint byte count overflow"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_len(value: &impl serde::Serialize) -> Result<u64> {
    let mut sink = ByteCount(0);
    serde_json::to_writer(&mut sink, value).map_err(|e| Error::storage(e.to_string()))?;
    Ok(sink.0)
}

fn add_bytes(sum: u64, extra: u64) -> Result<u64> {
    sum.checked_add(extra)
        .ok_or_else(|| Error::storage("checkpoint byte count overflow"))
}

/// Property map serialized in stored order. JSON object length does not
/// depend on key order; values serialize through [`Value`]'s tagged form.
struct BorrowedProps<'a> {
    interner: &'a Interner,
    props: &'a CompactProps,
}

impl serde::Serialize for BorrowedProps<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.props.keys.len()))?;
        for (key, value) in self.props.iter() {
            map.serialize_entry(self.interner.get(key).as_ref(), value)?;
        }
        map.end()
    }
}

/// `{amount, id, ts}` in the same tagged JSON as a property map, without
/// cloning the triple fields. Key order does not change the byte length.
struct BorrowedTriple<'a> {
    id: &'a str,
    amount: &'a Value,
    ts: i64,
}

impl serde::Serialize for BorrowedTriple<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("amount", self.amount)?;
        map.serialize_entry("id", &cypherlite_core::tagged_str(self.id))?;
        map.serialize_entry("ts", &Value::Int(self.ts))?;
        map.end()
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct LiveNode {
    pub labels: Vec<u32>,
    pub props: CompactProps,
}

impl LiveNode {
    pub(crate) fn from_parts(
        interner: &Interner,
        labels: Vec<String>,
        properties: BTreeMap<String, Value>,
    ) -> Self {
        let mut labels: Vec<u32> = labels.iter().map(|l| interner.intern(l)).collect();
        crate::memory::fit_reserved(&mut labels);
        Self {
            labels,
            props: CompactProps::from_map(interner, properties),
        }
    }

    pub(crate) fn to_node(&self, id: NodeId, interner: &Interner) -> Node {
        Node {
            id,
            labels: Arc::new(self.label_strings(interner)),
            properties: cypherlite_core::PropertyMap::share(self.props.to_map(interner)),
        }
    }

    pub(crate) fn has_label(&self, label: u32) -> bool {
        self.labels.contains(&label)
    }

    pub(crate) fn label_strings(&self, interner: &Interner) -> Vec<String> {
        self.labels
            .iter()
            .map(|l| String::from(&*interner.get(*l)))
            .collect()
    }

    pub(crate) fn prop(&self, interner: &Interner, key: &str) -> Option<Value> {
        let kid = interner.get_id(key)?;
        self.props.get(kid).cloned()
    }

    /// Label bytes plus tagged property JSON. The Parquet codec adds the
    /// fixed width of its Int64 columns.
    pub(crate) fn checkpoint_bytes(&self, interner: &Interner) -> Result<u64> {
        let mut bytes = 0u64;
        for label in &self.labels {
            bytes = add_bytes(bytes, interner.get(*label).len() as u64)?;
        }
        add_bytes(bytes, self.props.tagged_json_len(interner)?)
    }

    pub(crate) fn stub(&self, id: NodeId, interner: &Interner) -> Node {
        Node {
            id,
            labels: interner.cached_labels(&self.labels),
            properties: empty_properties(),
        }
    }
}

/// Common fraud/identity transfer payload. Any other shape uses `Other`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RelPayload {
    Triple {
        id: Arc<str>,
        amount: Value,
        ts: i64,
    },
    Other(CompactProps),
}

/// The triple-or-map choice without interned key ids.
///
/// `RelPayload::from_map` stores a `Triple` only when the map is created in
/// this shape. `Other` never turns back into a `Triple`, so a later `SET`
/// that happens to produce `{id, amount, ts}` still occupies property
/// vectors. Accounting has to follow this form, not re-read the map.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PayloadForm {
    Triple {
        id: Arc<str>,
        amount: Value,
        ts: i64,
    },
    Props(BTreeMap<String, Value>),
}

impl PayloadForm {
    pub(crate) fn from_map(mut map: BTreeMap<String, Value>) -> Self {
        if map.len() == 3
            && matches!(
                (map.get("id"), map.get("amount"), map.get("ts")),
                (
                    Some(Value::String(_)),
                    Some(Value::Int(_) | Value::Float(_)),
                    Some(Value::Int(_))
                )
            )
        {
            let Value::String(id) = map.remove("id").expect("id checked") else {
                unreachable!("id checked as a string");
            };
            let amount = map.remove("amount").expect("amount checked");
            let Value::Int(ts) = map.remove("ts").expect("ts checked") else {
                unreachable!("ts checked as an int");
            };
            return PayloadForm::Triple { id, amount, ts };
        }
        PayloadForm::Props(map)
    }

    pub(crate) fn set(&mut self, key: &str, value: Value) {
        if value.is_null() {
            self.remove(key);
            return;
        }
        match self {
            PayloadForm::Triple { id, amount, ts } => match (key, &value) {
                ("id", Value::String(s)) => *id = s.clone(),
                ("amount", Value::Int(_) | Value::Float(_)) => *amount = value,
                ("ts", Value::Int(i)) => *ts = *i,
                _ => {
                    self.promote();
                    self.set(key, value);
                }
            },
            PayloadForm::Props(props) => {
                props.insert(key.to_string(), value);
            }
        }
    }

    pub(crate) fn remove(&mut self, key: &str) {
        if matches!(self, PayloadForm::Triple { .. }) {
            self.promote();
        }
        if let PayloadForm::Props(props) = self {
            props.remove(key);
        }
    }

    fn promote(&mut self) {
        let PayloadForm::Triple { id, amount, ts } = self else {
            return;
        };
        let mut props = BTreeMap::new();
        props.insert("id".into(), Value::String(std::mem::take(id)));
        props.insert("amount".into(), std::mem::replace(amount, Value::Null));
        props.insert("ts".into(), Value::Int(*ts));
        *self = PayloadForm::Props(props);
    }
}

impl RelPayload {
    pub(crate) fn from_map(interner: &Interner, map: BTreeMap<String, Value>) -> Self {
        match PayloadForm::from_map(map) {
            PayloadForm::Triple { id, amount, ts } => RelPayload::Triple { id, amount, ts },
            PayloadForm::Props(map) => RelPayload::Other(CompactProps::from_map(interner, map)),
        }
    }

    /// The storage choice the byte account has to follow. `Other` stays
    /// `Props` even when its keys are `id`, `amount`, and `ts`.
    pub(crate) fn form(&self, interner: &Interner) -> PayloadForm {
        match self {
            RelPayload::Triple { id, amount, ts } => PayloadForm::Triple {
                id: id.clone(),
                amount: amount.clone(),
                ts: *ts,
            },
            RelPayload::Other(props) => PayloadForm::Props(props.to_map(interner)),
        }
    }

    pub(crate) fn to_map(&self, interner: &Interner) -> BTreeMap<String, Value> {
        match self {
            RelPayload::Triple { id, amount, ts } => {
                let mut m = BTreeMap::new();
                m.insert("id".into(), Value::String(id.clone()));
                m.insert("amount".into(), amount.clone());
                m.insert("ts".into(), Value::Int(*ts));
                m
            }
            RelPayload::Other(p) => p.to_map(interner),
        }
    }

    fn promote(&mut self, interner: &Interner) {
        let (id, amount, ts) = match self {
            RelPayload::Triple { id, amount, ts } => (
                std::mem::take(id),
                std::mem::replace(amount, Value::Null),
                *ts,
            ),
            RelPayload::Other(_) => return,
        };
        let mut m = BTreeMap::new();
        m.insert("id".into(), Value::String(id));
        m.insert("amount".into(), amount);
        m.insert("ts".into(), Value::Int(ts));
        *self = RelPayload::Other(CompactProps::from_map(interner, m));
    }

    /// Storage twin of [`PayloadForm::set`]. The redo estimate follows
    /// `PayloadForm`; the live counter follows this payload. A change to
    /// either match arm has to land in both.
    pub(crate) fn set(&mut self, interner: &Interner, key: &str, value: Value) {
        if value.is_null() {
            self.remove(interner, key);
            return;
        }
        match self {
            RelPayload::Triple { id, amount, ts } => match (key, &value) {
                ("id", Value::String(s)) => *id = s.clone(),
                ("amount", Value::Int(_) | Value::Float(_)) => {
                    *amount = value;
                }
                ("ts", Value::Int(i)) => *ts = *i,
                _ => {
                    self.promote(interner);
                    self.set(interner, key, value);
                }
            },
            RelPayload::Other(p) => p.set(interner.intern(key), value),
        }
    }

    pub(crate) fn ts(&self, interner: &Interner) -> Option<i64> {
        match self {
            RelPayload::Triple { ts, .. } => Some(*ts),
            RelPayload::Other(p) => match interner.get_id("ts").and_then(|k| p.get(k)) {
                Some(Value::Int(i)) => Some(*i),
                _ => None,
            },
        }
    }

    pub(crate) fn get(&self, interner: &Interner, key: &str) -> Option<Value> {
        match self {
            RelPayload::Triple { id, amount, ts } => match key {
                "id" => Some(Value::String(id.clone())),
                "amount" => Some(amount.clone()),
                "ts" => Some(Value::Int(*ts)),
                _ => None,
            },
            RelPayload::Other(p) => p.get(interner.get_id(key)?).cloned(),
        }
    }

    pub(crate) fn keys(&self, interner: &Interner) -> Vec<String> {
        match self {
            RelPayload::Triple { .. } => vec!["amount".into(), "id".into(), "ts".into()],
            RelPayload::Other(p) => p.keys_as_strings(interner),
        }
    }

    /// One pass over every property as (interned key id, value) pairs
    /// (ADR-0185): `Other` zips its stored ids and values; `Triple`
    /// resolves its three fixed keys once and yields from its fields. No
    /// string de-interning or re-hashing.
    pub(crate) fn for_each_key_value(&self, interner: &Interner, mut f: impl FnMut(u32, &Value)) {
        match self {
            RelPayload::Triple { id, amount, ts } => {
                let pairs = [
                    ("amount", amount.clone()),
                    ("id", Value::String(id.clone())),
                    ("ts", Value::Int(*ts)),
                ];
                for (k, v) in pairs {
                    if let Some(kid) = interner.get_id(k) {
                        f(kid, &v);
                    }
                }
            }
            RelPayload::Other(p) => {
                for (k, v) in p.iter() {
                    f(k, v);
                }
            }
        }
    }

    pub(crate) fn tagged_json_len(&self, interner: &Interner) -> Result<u64> {
        match self {
            RelPayload::Triple { id, amount, ts } => json_len(&BorrowedTriple {
                id,
                amount,
                ts: *ts,
            }),
            RelPayload::Other(props) => props.tagged_json_len(interner),
        }
    }

    pub(crate) fn remove(&mut self, interner: &Interner, key: &str) {
        if matches!(self, RelPayload::Triple { .. }) {
            self.promote(interner);
        }
        if let RelPayload::Other(p) = self {
            p.remove(interner.intern(key));
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LiveRel {
    pub start: NodeId,
    pub end: NodeId,
    pub type_id: u32,
    type_name: Arc<str>,
    pub payload: RelPayload,
}

impl LiveRel {
    pub(crate) fn from_parts(
        interner: &Interner,
        start: NodeId,
        end: NodeId,
        rel_type: &str,
        properties: BTreeMap<String, Value>,
    ) -> Self {
        let type_id = interner.intern(rel_type);
        Self {
            start,
            end,
            type_id,
            type_name: interner.get(type_id),
            payload: RelPayload::from_map(interner, properties),
        }
    }

    pub(crate) fn to_rel(&self, id: RelId, interner: &Interner) -> Relationship {
        Relationship {
            id,
            start: self.start,
            end: self.end,
            rel_type: String::from(&*self.type_name),
            properties: cypherlite_core::PropertyMap::share(self.payload.to_map(interner)),
        }
    }

    pub(crate) fn stub(&self, id: RelId) -> Relationship {
        Relationship {
            id,
            start: self.start,
            end: self.end,
            rel_type: String::from(&*self.type_name),
            properties: empty_properties(),
        }
    }

    /// Type-name bytes plus tagged property JSON. The Parquet codec adds the
    /// fixed width of its Int64 columns.
    pub(crate) fn checkpoint_bytes(&self, interner: &Interner) -> Result<u64> {
        add_bytes(
            self.type_name.len() as u64,
            self.payload.tagged_json_len(interner)?,
        )
    }
}

/// One hop in a typed chunk. Ordered for `ORDER BY ts DESC` (ADR-0035).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdjHop {
    pub ts: Option<i64>,
    pub id: RelId,
}

/// Per-node typed adjacency chunks (ADR-0031 / ADR-0033). Superset lists.
/// Each chunk is `ts` DESC with missing integer `ts` first (ADR-0035);
/// equal-`ts` hops are newest relationship first (ADR-0203 — see
/// `cmp_ts_desc`).
#[derive(Clone, Debug, Default)]
pub(crate) struct TypedAdj {
    pub chunks: Vec<(u32, Vec<AdjHop>)>,
}

impl TypedAdj {
    pub(crate) fn insert(&mut self, type_id: u32, rid: RelId, ts: Option<i64>) {
        let hop = AdjHop { ts, id: rid };
        if let Some((_, v)) = self.chunks.iter_mut().find(|(t, _)| *t == type_id) {
            insert_hop(v, hop);
        } else {
            let mut hops = vec![hop];
            crate::memory::fit_reserved(&mut hops);
            self.chunks.push((type_id, hops));
            crate::memory::fit_reserved(&mut self.chunks);
        }
    }

    pub(crate) fn floor_bytes(&self) -> u64 {
        let hops: Vec<usize> = self.chunks.iter().map(|(_, v)| v.len()).collect();
        crate::memory::adjacency_bytes(&hops)
    }

    pub(crate) fn reposition(&mut self, type_id: u32, rid: RelId, ts: Option<i64>) {
        if let Some((_, v)) = self.chunks.iter_mut().find(|(t, _)| *t == type_id) {
            if let Some(i) = v.iter().position(|h| h.id == rid) {
                v.remove(i);
            }
            insert_hop(v, AdjHop { ts, id: rid });
        } else {
            self.insert(type_id, rid, ts);
        }
    }

    pub(crate) fn flatten(&self) -> Vec<RelId> {
        self.chunks
            .iter()
            .flat_map(|(_, v)| v.iter().map(|h| h.id))
            .collect()
    }

    pub(crate) fn chunk(&self, type_id: u32) -> &[AdjHop] {
        self.chunks
            .iter()
            .find(|(t, _)| *t == type_id)
            .map(|(_, v)| v.as_slice())
            .unwrap_or(&[])
    }
}

/// OpenCypher: NULL ranks after NUMBER in ASC, so first in DESC.
/// The `RelId` tie-break is DESCENDING (newest first): Neo4j inserts a new
/// relationship at the head of the node's relationship chain, so reference
/// expansions — and therefore its shortestPath tie-breaks and the row order
/// behind aggregation ties — see the newest relationship first (ADR-0203,
/// LABS-255/LABS-258).
fn cmp_ts_desc(a: &AdjHop, b: &AdjHop) -> std::cmp::Ordering {
    match (a.ts, b.ts) {
        (None, None) => b.id.cmp(&a.id),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(ta), Some(tb)) => tb.cmp(&ta).then(b.id.cmp(&a.id)),
    }
}

fn insert_hop(v: &mut Vec<AdjHop>, hop: AdjHop) {
    match v.binary_search_by(|h| cmp_ts_desc(h, &hop)) {
        Ok(_) => {}
        Err(pos) => {
            v.insert(pos, hop);
            crate::memory::fit_reserved(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_roundtrip() {
        let i = Interner::default();
        let a = i.intern("TRANSFER");
        let b = i.intern("TRANSFER");
        assert_eq!(a, b);
        assert_eq!(&*i.get(a), "TRANSFER");
        assert_eq!(i.get_id("OTHER"), None);
    }

    #[test]
    #[should_panic(expected = "interned id is published before use")]
    fn get_unknown_id_panics() {
        let i = Interner::default();
        let _ = i.get(0);
    }

    /// The id table is append-only and concurrent `intern` calls race to mint
    /// ids. Every distinct string must get exactly one id, repeated interns of
    /// the same string must agree, and no id may ever resolve to the wrong
    /// string (which the old `len()`-based counter could cause).
    #[test]
    fn intern_concurrent_ids_are_stable_and_unique() {
        use std::collections::{HashMap, HashSet};
        use std::sync::Arc as StdArc;

        let interner = StdArc::new(Interner::default());
        let words: Vec<String> = (0..64).map(|n| format!("label-{n}")).collect();

        let handles: Vec<_> = (0..8)
            .map(|t| {
                let interner = StdArc::clone(&interner);
                let words = words.clone();
                std::thread::spawn(move || {
                    let mut seen = HashMap::new();
                    for round in 0..32 {
                        for n in 0..words.len() {
                            // Rotate start so threads interleave differently.
                            let w = &words[(n + t + round) % words.len()];
                            let id = interner.intern(w);
                            // Idempotent: the same string always yields the
                            // same id, even under concurrent first-sight.
                            if let Some(prev) = seen.insert(w.clone(), id) {
                                assert_eq!(prev, id, "id changed for {w}");
                            }
                            // The reverse direction must agree immediately.
                            assert_eq!(&*interner.get(id), w.as_str());
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        // Exactly one id per distinct string, and the id space is dense from 0.
        let ids: HashSet<u32> = words.iter().map(|w| interner.intern(w)).collect();
        assert_eq!(ids.len(), words.len(), "duplicate ids for distinct strings");
        assert_eq!(*ids.iter().min().unwrap(), 0);
        assert_eq!(*ids.iter().max().unwrap(), (words.len() - 1) as u32);
    }

    #[test]
    fn triple_payload_preserves_int_amount() {
        let i = Interner::default();
        let mut m = BTreeMap::new();
        m.insert("id".into(), Value::String("tx-1".into()));
        m.insert("amount".into(), Value::Int(10));
        m.insert("ts".into(), Value::Int(99));
        let p = RelPayload::from_map(&i, m);
        assert!(matches!(
            p,
            RelPayload::Triple {
                amount: Value::Int(10),
                ts: 99,
                ..
            }
        ));
        let out = p.to_map(&i);
        assert_eq!(out.get("amount"), Some(&Value::Int(10)));
    }

    #[test]
    fn triple_promotes_on_extra_key() {
        let i = Interner::default();
        let mut m = BTreeMap::new();
        m.insert("id".into(), Value::String("tx-1".into()));
        m.insert("amount".into(), Value::Float(1.5));
        m.insert("ts".into(), Value::Int(1));
        let mut p = RelPayload::from_map(&i, m);
        p.set(&i, "note", Value::String("x".into()));
        assert!(matches!(p, RelPayload::Other(_)));
        assert_eq!(p.to_map(&i).get("note"), Some(&Value::String("x".into())));
    }

    #[test]
    fn typed_adj_chunks() {
        let mut a = TypedAdj::default();
        a.insert(1, 10, None);
        a.insert(2, 20, None);
        a.insert(1, 11, None);
        // Newest-first within a chunk (ADR-0203): the Neo4j reference
        // expands a node's relationship chain head-first, so the
        // highest-id relationship of a type comes first.
        assert_eq!(
            a.chunk(1).iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![11, 10]
        );
        assert_eq!(a.flatten(), vec![11, 10, 20]);
    }

    #[test]
    fn typed_adj_orders_ts_desc_nulls_first() {
        let mut a = TypedAdj::default();
        a.insert(1, 1, Some(10));
        a.insert(1, 2, Some(30));
        a.insert(1, 3, None);
        a.insert(1, 4, Some(20));
        let ids: Vec<_> = a.chunk(1).iter().map(|h| (h.ts, h.id)).collect();
        assert_eq!(
            ids,
            vec![(None, 3), (Some(30), 2), (Some(20), 4), (Some(10), 1)]
        );
        a.reposition(1, 1, Some(40));
        let ids: Vec<_> = a.chunk(1).iter().map(|h| (h.ts, h.id)).collect();
        assert_eq!(
            ids,
            vec![(None, 3), (Some(40), 1), (Some(30), 2), (Some(20), 4)]
        );
    }
}
