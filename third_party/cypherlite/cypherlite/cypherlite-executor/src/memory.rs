//! Logical live-graph byte account (ADR-0042, amended by ADR-0202;
//! string-posting formula ADR-0063).
//!
//! The payload terms stay. On top of them the floor charges the containers
//! that still own the payload: dashmap slots, version lists, property
//! vectors, and adjacency chunks. A vector of length `n` is charged for
//! [`reserved_len`]`(n)` elements, not for whatever the allocator rounded
//! the allocation up to.

use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::sync::Arc;

use cypherlite_core::{str_logical_bytes, Node, NodeId, RelId, Value};

use crate::group_key::GroupKey;
use crate::redo::RedoOp;
use crate::store::{Interner, LiveNode, LiveRel, PayloadForm, RelPayload};

pub const OVERHEAD_VERSION: u64 = 24;
pub const OVERHEAD_POSTING: u64 = 8;
pub const OVERHEAD_MAP_ENTRY: u64 = 24;
/// `Vec`'s three-word header. Inline in the parent value, so it is charged
/// once for the vector itself and not again for elements whose `size_of`
/// already includes an inner `Vec`.
pub const VEC_HEADER: u64 = 24;

/// Capacity the floor charges for a vector of length `len`.
///
/// `0` reserves nothing. Every other length rounds up to the next power of
/// two. `usize::next_power_of_two` maps `0` to `1`, so zero is special-cased.
pub fn reserved_len(len: usize) -> usize {
    if len == 0 {
        0
    } else {
        len.next_power_of_two()
    }
}

/// Heap bytes of a `Vec<T>` of length `len`: `reserved_len(len) * elem`.
/// The header is not included; callers add [`VEC_HEADER`] when the header
/// is not already inside a parent `size_of`.
pub fn vec_heap(len: usize, elem: u64) -> u64 {
    reserved_len(len) as u64 * elem
}

/// Grow `v` so `capacity == reserved_len(len)`.
pub(crate) fn fit_reserved<T>(v: &mut Vec<T>) {
    let want = reserved_len(v.len());
    if v.capacity() == want {
        return;
    }
    let mut fitted = Vec::with_capacity(want);
    fitted.append(v);
    if fitted.capacity() > want {
        fitted.shrink_to(want);
    }
    *v = fitted;
}

/// One dashmap slot plus the version-list header and its reserved buffer.
pub fn chain_bytes(versions: usize, stride: u64) -> u64 {
    if versions == 0 {
        0
    } else {
        OVERHEAD_MAP_ENTRY + VEC_HEADER + vec_heap(versions, stride)
    }
}

pub fn node_chain_bytes(versions: usize) -> u64 {
    chain_bytes(versions, crate::graph::node_version_stride())
}

pub fn rel_chain_bytes(versions: usize) -> u64 {
    chain_bytes(versions, crate::graph::rel_version_stride())
}

/// `to_id` slot, `to_str` slot, and one `Arc<str>` allocation. A second
/// `Arc` clone of the same string is not charged again.
pub fn interned_string_bytes(len: usize) -> u64 {
    2 * OVERHEAD_MAP_ENTRY + 16 + len as u64
}

/// One stub-label cache entry: the dashmap slot, the label-id key, the
/// `Vec<String>` header and its reserved `String` slots, and the copied
/// string heaps. Charged once per distinct label-id list. The interned
/// originals stay on [`interned_string_bytes`]; these are the cache's own
/// copies, and the entry is not evicted when the nodes go away.
pub fn label_vec_cache_bytes(label_count: usize, string_bytes: u64) -> u64 {
    OVERHEAD_MAP_ENTRY
        + VEC_HEADER
        + vec_heap(label_count, size_of::<u32>() as u64)
        + VEC_HEADER
        + vec_heap(label_count, size_of::<String>() as u64)
        + string_bytes
}

/// One adjacency map entry: the chunk vector plus each hop buffer.
/// Inner hop-vector headers sit inside `size_of<(u32, Vec<AdjHop>)>`.
pub fn adjacency_bytes(chunk_hops: &[usize]) -> u64 {
    if chunk_hops.is_empty() {
        return 0;
    }
    let chunk_stride = size_of::<(u32, Vec<crate::store::AdjHop>)>() as u64;
    let hop_stride = size_of::<crate::store::AdjHop>() as u64;
    OVERHEAD_MAP_ENTRY
        + VEC_HEADER
        + vec_heap(chunk_hops.len(), chunk_stride)
        + chunk_hops
            .iter()
            .map(|n| vec_heap(*n, hop_stride))
            .sum::<u64>()
}

fn prop_vec_heap(count: usize) -> u64 {
    vec_heap(count, size_of::<u32>() as u64) + vec_heap(count, size_of::<Value>() as u64)
}

/// `(label, property)` pairs that currently have a TEXT, RANGE, or POINT
/// index. Gram, prefix/equality, and point bytes are charged only for these
/// pairs — an unindexed string adds no gram bytes.
///
/// Properties are grouped by label so a membership test borrows `&str`
/// instead of allocating a `(String, String)` key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexCharge {
    pub text: HashMap<String, HashSet<String>>,
    pub range: HashMap<String, HashSet<String>>,
    pub point: HashMap<String, HashSet<String>>,
}

impl IndexCharge {
    pub fn from_details<'a>(
        details: impl IntoIterator<Item = &'a crate::redo::IndexDetailSpec>,
    ) -> Self {
        let mut charge = Self::default();
        for detail in details {
            for (label, property, kind) in detail.covered_pairs() {
                let bucket = match kind {
                    crate::redo::IndexKindSpec::Text => &mut charge.text,
                    crate::redo::IndexKindSpec::Range => &mut charge.range,
                    crate::redo::IndexKindSpec::Point => &mut charge.point,
                    _ => continue,
                };
                bucket
                    .entry(label.to_string())
                    .or_default()
                    .insert(property.to_string());
            }
        }
        charge
    }

    fn charges(bucket: &HashMap<String, HashSet<String>>, label: &str, property: &str) -> bool {
        bucket
            .get(label)
            .is_some_and(|props| props.contains(property))
    }
}

fn classify_prop<'a>(
    key: &'a str,
    v: &'a Value,
    record_props: &mut u64,
    indexable: &mut Vec<(&'a str, u64, u64)>,
    strings: &mut Vec<(&'a str, u64, u64, u64)>,
    points: &mut Vec<(&'a str, u64)>,
) {
    let klen = key.len() as u64;
    *record_props += 4 + v.logical_bytes();
    if let Some(gk) = GroupKey::for_property_index(v) {
        indexable.push((key, klen, gk.logical_bytes()));
    }
    if let Value::String(s) = v {
        strings.push((key, klen, s.len() as u64, s.chars().count() as u64));
    }
    if let Value::Point(_) = v {
        points.push((key, klen));
    }
}

pub fn live_node_account(n: &Node, charge: &IndexCharge) -> u64 {
    let labels: Vec<&str> = n.labels.iter().map(String::as_str).collect();
    let mut record_props = 0u64;
    let mut indexable = Vec::new();
    let mut strings = Vec::new();
    let mut points = Vec::new();
    for (k, v) in &*n.properties {
        classify_prop(
            k.as_str(),
            v,
            &mut record_props,
            &mut indexable,
            &mut strings,
            &mut points,
        );
    }
    node_account(
        &labels,
        n.properties.len(),
        record_props,
        &indexable,
        &strings,
        &points,
        charge,
    )
}

/// The same account computed from the compact live record (ADR-0063): the
/// bulk-create path charges straight from parts without materializing a
/// `Node`. Kept numerically identical to [`live_node_account`] by
/// construction — both delegate to `node_account`.
pub fn live_node_account_live(live: &LiveNode, interner: &Interner, charge: &IndexCharge) -> u64 {
    // Hold the interned arcs so the `&str` slices below stay valid. Cloning
    // an `Arc<str>` bumps a refcount; it does not copy the bytes.
    let label_arcs: Vec<Arc<str>> = live.labels.iter().map(|&l| interner.get(l)).collect();
    let labels: Vec<&str> = label_arcs.iter().map(|s| s.as_ref()).collect();
    let prop_arcs: Vec<(Arc<str>, &Value)> = live
        .props
        .iter()
        .map(|(kid, v)| (interner.get(kid), v))
        .collect();
    let mut record_props = 0u64;
    let mut indexable = Vec::new();
    let mut strings = Vec::new();
    let mut points = Vec::new();
    for (key, v) in &prop_arcs {
        classify_prop(
            key.as_ref(),
            v,
            &mut record_props,
            &mut indexable,
            &mut strings,
            &mut points,
        );
    }
    node_account(
        &labels,
        live.props.len(),
        record_props,
        &indexable,
        &strings,
        &points,
        charge,
    )
}

fn rel_record_account(props: u64, heaps: u64) -> u64 {
    OVERHEAD_VERSION + 16 + 4 + props + heaps
}

/// Bytes of the compact relationship record (ADR-0063).
///
/// A stored `Other` keeps its property vectors even when the keys are
/// `id`, `amount`, and `ts`. [`PayloadForm`] is that same choice without
/// interned key ids, so the redo estimate can follow a `SET` without
/// re-classifying the map. Adjacency is not included; the walk and the
/// estimate charge [`adjacency_bytes`] from the out/in maps.
pub(crate) fn payload_form_account(form: &PayloadForm) -> u64 {
    // Value::logical_bytes: String = 16 + len, Int/Float = 8.
    let (props, heaps) = match form {
        PayloadForm::Triple { id, amount, .. } => {
            (str_logical_bytes(id) + amount.logical_bytes() + 8, 0)
        }
        PayloadForm::Props(props) => (
            props.values().map(Value::logical_bytes).sum(),
            prop_vec_heap(props.len()),
        ),
    };
    rel_record_account(props, heaps)
}

/// [`payload_form_account`] from the compact live record.
pub fn live_rel_account_live(r: &LiveRel) -> u64 {
    let (props, heaps) = match &r.payload {
        RelPayload::Triple { id, amount, .. } => {
            (str_logical_bytes(id) + amount.logical_bytes() + 8, 0)
        }
        RelPayload::Other(p) => (
            p.iter().map(|(_, v)| v.logical_bytes()).sum(),
            prop_vec_heap(p.len()),
        ),
    };
    rel_record_account(props, heaps)
}

/// Single accounting core behind [`live_node_account`] and
/// [`live_node_account_live`]. `indexable` is `(key, key_len, group_key_bytes)`
/// per property-indexed property; `strings` is `(key, key_len, byte_len, char_len)`;
/// `points` is `(key, key_len)`. A RANGE index charges one posting per
/// indexable value (the group-key term below). TEXT, POINT, and that
/// RANGE term are charged only when `charge` names that `(label, key)`.
/// Keys are borrowed so the hot path does not allocate a `String` per label
/// or property.
fn node_account(
    labels: &[&str],
    prop_count: usize,
    record_props: u64,
    indexable: &[(&str, u64, u64)],
    strings: &[(&str, u64, u64, u64)],
    points: &[(&str, u64)],
    charge: &IndexCharge,
) -> u64 {
    let mut cost = OVERHEAD_VERSION + 8 + 4 * labels.len() as u64 + record_props;
    // Label-presence postings (the empty-label bucket keys on "").
    if labels.is_empty() {
        cost += OVERHEAD_MAP_ENTRY + OVERHEAD_POSTING;
    }
    for l in labels {
        cost += OVERHEAD_MAP_ENTRY + l.len() as u64 + OVERHEAD_POSTING;
    }
    for l in labels {
        let llen = l.len() as u64;
        for (k, klen, gk) in indexable {
            if IndexCharge::charges(&charge.range, l, k) {
                cost += OVERHEAD_MAP_ENTRY + llen + klen + gk + OVERHEAD_POSTING;
            }
        }
        for (k, _klen, _byte_len, char_len) in strings {
            if IndexCharge::charges(&charge.text, l, k) {
                // One posting id per width-3 window (ADR-0207). The source
                // string is the property already counted in `record_props`.
                cost += crate::graph::trigram_count(*char_len) * OVERHEAD_POSTING;
            }
        }
        for (k, klen) in points {
            if IndexCharge::charges(&charge.point, l, k) {
                cost += OVERHEAD_MAP_ENTRY + llen + klen + 24 + OVERHEAD_POSTING;
            }
        }
    }
    // Label-id and property vectors. Their headers live inside the version
    // stride; this is only the reserved heap.
    cost += vec_heap(labels.len(), size_of::<u32>() as u64);
    cost += prop_vec_heap(prop_count);
    cost
}

/// In-place edits replace the record. The first edit of a committed record
/// copies it: the old version stays, and the chain grows by one.
fn version_edit(
    in_batch: bool,
    before: u64,
    after: u64,
    committed_len: usize,
    chain: impl Fn(usize) -> u64,
) -> i64 {
    if in_batch {
        after as i64 - before as i64
    } else {
        let len = committed_len.max(1);
        after as i64 + chain(len + 1) as i64 - chain(len) as i64
    }
}

/// Sequential live-size delta of `ops` against `lookup` (committed graph).
///
/// Every node is accounted under `charge`, the index charge in force after
/// `ops` (DDL in the same batch included). When `ops` also change the
/// charge, [`Graph::estimate_redo_delta`](crate::graph::Graph::estimate_redo_delta)
/// re-accounts the committed live set from the old charge to the new one;
/// together the two are exact for created, modified, deleted, and untouched
/// nodes alike. A delete keeps the version until GC, so its delta is zero.
/// A set on a committed node copies the version and adds the new record;
/// a set on a node this batch already created edits that record in place.
pub fn estimate_redo_delta(
    ops: &[RedoOp],
    lookup_node: impl Fn(NodeId) -> Option<Node>,
    lookup_rel: impl Fn(RelId) -> Option<PayloadForm>,
    node_versions: impl Fn(NodeId) -> usize,
    rel_versions: impl Fn(RelId) -> usize,
    charge: &IndexCharge,
) -> i64 {
    let mut nodes: HashMap<NodeId, Option<Node>> = HashMap::new();
    let mut rels: HashMap<RelId, Option<PayloadForm>> = HashMap::new();
    let mut delta = 0i64;

    let get_node = |nodes: &HashMap<NodeId, Option<Node>>, id: NodeId| -> Option<Node> {
        if let Some(v) = nodes.get(&id) {
            return v.clone();
        }
        lookup_node(id)
    };
    let get_rel = |rels: &HashMap<RelId, Option<PayloadForm>>, id: RelId| -> Option<PayloadForm> {
        if let Some(v) = rels.get(&id) {
            return v.clone();
        }
        lookup_rel(id)
    };

    for op in ops {
        match op {
            RedoOp::CreateNode {
                id,
                labels,
                properties,
            } => {
                let n = Node {
                    id: *id,
                    labels: labels.clone(),
                    properties: properties.clone(),
                };
                delta += live_node_account(&n, charge) as i64 + node_chain_bytes(1) as i64;
                nodes.insert(*id, Some(n));
            }
            RedoOp::CreateRel {
                id,
                start: _,
                end: _,
                rel_type: _,
                properties,
            } => {
                let form = PayloadForm::from_map((**properties).clone().into());
                delta += payload_form_account(&form) as i64 + rel_chain_bytes(1) as i64;
                rels.insert(*id, Some(form));
            }
            RedoOp::DeleteNode { id } => {
                // Retained until GC. Mark it so a later op in this batch
                // does not edit the dropped record.
                nodes.insert(*id, None);
            }
            RedoOp::DeleteRel { id } => {
                rels.insert(*id, None);
            }
            RedoOp::SetNodeProp { id, key, value } => {
                let in_batch = nodes.contains_key(id);
                let mut n = match get_node(&nodes, *id) {
                    Some(n) => n,
                    None => continue,
                };
                let before = live_node_account(&n, charge);
                Arc::make_mut(&mut n.properties).insert(key.clone(), value.clone());
                let after = live_node_account(&n, charge);
                delta += version_edit(
                    in_batch,
                    before,
                    after,
                    node_versions(*id),
                    node_chain_bytes,
                );
                nodes.insert(*id, Some(n));
            }
            RedoOp::RemoveNodeProp { id, key } => {
                let in_batch = nodes.contains_key(id);
                let mut n = match get_node(&nodes, *id) {
                    Some(n) => n,
                    None => continue,
                };
                let before = live_node_account(&n, charge);
                Arc::make_mut(&mut n.properties).remove(key);
                let after = live_node_account(&n, charge);
                delta += version_edit(
                    in_batch,
                    before,
                    after,
                    node_versions(*id),
                    node_chain_bytes,
                );
                nodes.insert(*id, Some(n));
            }
            RedoOp::SetRelProp { id, key, value } => {
                let in_batch = rels.contains_key(id);
                let mut form = match get_rel(&rels, *id) {
                    Some(form) => form,
                    None => continue,
                };
                let before = payload_form_account(&form);
                form.set(key, value.clone());
                let after = payload_form_account(&form);
                delta += version_edit(in_batch, before, after, rel_versions(*id), rel_chain_bytes);
                rels.insert(*id, Some(form));
            }
            RedoOp::RemoveRelProp { id, key } => {
                let in_batch = rels.contains_key(id);
                let mut form = match get_rel(&rels, *id) {
                    Some(form) => form,
                    None => continue,
                };
                let before = payload_form_account(&form);
                form.remove(key);
                let after = payload_form_account(&form);
                delta += version_edit(in_batch, before, after, rel_versions(*id), rel_chain_bytes);
                rels.insert(*id, Some(form));
            }
            RedoOp::AddLabels { id, labels } => {
                let in_batch = nodes.contains_key(id);
                let mut n = match get_node(&nodes, *id) {
                    Some(n) => n,
                    None => continue,
                };
                let before = live_node_account(&n, charge);
                for l in labels {
                    if !n.labels.iter().any(|x| x == l) {
                        Arc::make_mut(&mut n.labels).push(l.clone());
                    }
                }
                let after = live_node_account(&n, charge);
                delta += version_edit(
                    in_batch,
                    before,
                    after,
                    node_versions(*id),
                    node_chain_bytes,
                );
                nodes.insert(*id, Some(n));
            }
            RedoOp::RemoveLabels { id, labels } => {
                let in_batch = nodes.contains_key(id);
                let mut n = match get_node(&nodes, *id) {
                    Some(n) => n,
                    None => continue,
                };
                let before = live_node_account(&n, charge);
                Arc::make_mut(&mut n.labels).retain(|l| !labels.contains(l));
                let after = live_node_account(&n, charge);
                delta += version_edit(
                    in_batch,
                    before,
                    after,
                    node_versions(*id),
                    node_chain_bytes,
                );
                nodes.insert(*id, Some(n));
            }
            RedoOp::CreateIndex { .. }
            | RedoOp::DropIndex { .. }
            | RedoOp::CreateConstraint { .. }
            | RedoOp::DropConstraint { .. }
            | RedoOp::CatalogIndexDetail { .. }
            | RedoOp::CatalogDerivedConstraint { .. }
            | RedoOp::PutDatabase { .. }
            | RedoOp::DropDatabase { .. }
            | RedoOp::PutGraphType { .. }
            | RedoOp::SetCurrentGraphType { .. }
            | RedoOp::PullIdentityPut { .. }
            | RedoOp::OntologySet { .. }
            | RedoOp::OntologyDrop { .. } => {}
        }
    }
    delta
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn reserved_len_is_zero_or_the_next_power_of_two() {
        assert_eq!(reserved_len(0), 0);
        assert_eq!(reserved_len(1), 1);
        assert_eq!(reserved_len(2), 2);
        assert_eq!(reserved_len(3), 4);
        assert_eq!(reserved_len(20), 32);
    }

    #[test]
    fn other_that_gains_the_triple_keys_keeps_its_vectors() {
        let partial = BTreeMap::from([
            ("id".into(), Value::String("a".into())),
            ("amount".into(), Value::Int(1)),
        ]);
        let mut form = PayloadForm::from_map(partial.clone());
        let created = payload_form_account(&form);
        form.set("ts", Value::Int(5));
        let grown = payload_form_account(&form);
        assert!(matches!(form, PayloadForm::Props(_)));
        assert_eq!(
            grown,
            created + (prop_vec_heap(3) - prop_vec_heap(2)) + Value::Int(5).logical_bytes()
        );

        let mut triple = BTreeMap::from([
            ("id".into(), Value::String("a".into())),
            ("amount".into(), Value::Int(1)),
        ]);
        triple.insert("ts".into(), Value::Int(5));
        let as_created = PayloadForm::from_map(triple);
        assert!(matches!(as_created, PayloadForm::Triple { .. }));
        assert_eq!(payload_form_account(&as_created) + prop_vec_heap(3), grown);

        let interner = Interner::default();
        let mut stored = RelPayload::from_map(&interner, partial);
        stored.set(&interner, "ts", Value::Int(5));
        assert_eq!(stored.form(&interner), form);
        assert!(matches!(stored, RelPayload::Other(_)));

        let triple_map = BTreeMap::from([
            ("id".into(), Value::String("b".into())),
            ("amount".into(), Value::Int(2)),
            ("ts".into(), Value::Int(9)),
        ]);
        let mut stored_triple = RelPayload::from_map(&interner, triple_map.clone());
        let mut form_triple = PayloadForm::from_map(triple_map);
        stored_triple.set(&interner, "note", Value::String("x".into()));
        form_triple.set("note", Value::String("x".into()));
        assert_eq!(stored_triple.form(&interner), form_triple);
        assert!(matches!(stored_triple, RelPayload::Other(_)));
        assert!(matches!(form_triple, PayloadForm::Props(_)));
    }

    #[test]
    fn long_string_index_exceeds_payload() {
        let mut n = Node {
            id: 1,
            labels: Arc::new(vec!["Person".into()]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        };
        Arc::make_mut(&mut n.properties).insert("name".into(), Value::String("abcdefghij".into()));
        let plain = live_node_account(&n, &IndexCharge::default());
        let mut indexed = IndexCharge::default();
        indexed
            .text
            .entry("Person".into())
            .or_default()
            .insert("name".into());
        let with_grams = live_node_account(&n, &indexed);
        assert!(
            with_grams > plain,
            "a text index charges gram bytes; an unindexed string does not"
        );
        let char_len = 10u64;
        assert_eq!(
            with_grams - plain,
            crate::graph::trigram_count(char_len) * OVERHEAD_POSTING
        );
    }

    #[test]
    fn parts_account_matches_node_account() {
        // ADR-0063: the bulk-path charge from `LiveNode` parts must equal
        // the materialized-`Node` charge exactly (one shared core).
        let interner = Interner::default();
        // (labels, props) fixtures for one `Node` / `LiveNode` pair each.
        type Fixture = (Vec<&'static str>, Vec<(&'static str, Value)>);
        let cases: Vec<Fixture> = vec![
            (vec![], vec![]),
            (
                vec!["Person"],
                vec![("name", Value::String("Tomas 1".into()))],
            ),
            (
                vec!["Person", "Employee"],
                vec![
                    ("name", Value::String("Åsa 🦀".into())),
                    ("id", Value::Int(7)),
                    ("score", Value::Float(1.5)),
                    ("nick", Value::String("x".into())),
                ],
            ),
            // A literal "*" label collides with the any-label bucket.
            (vec!["*"], vec![("s", Value::String("abc".into()))]),
        ];
        for (labels, props) in cases {
            let n = Node {
                id: 1,
                labels: Arc::new(labels.iter().map(|l| l.to_string()).collect()),
                properties: cypherlite_core::PropertyMap::share(
                    props
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.clone()))
                        .collect::<std::collections::BTreeMap<_, _>>(),
                ),
            };
            let live = LiveNode::from_parts(
                &interner,
                labels.iter().map(|l| l.to_string()).collect(),
                props
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.clone()))
                    .collect(),
            );
            let none = IndexCharge::default();
            assert_eq!(
                live_node_account(&n, &none),
                live_node_account_live(&live, &interner, &none),
                "labels={labels:?} props={props:?}"
            );
        }
    }
}
