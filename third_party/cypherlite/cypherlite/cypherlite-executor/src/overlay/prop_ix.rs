//! The write scratch's lazy equality index, the scratch-side twin of the
//! committed graph's equality postings (ADR-0172 §3, ADR-0173, ADR-0185).
//!
//! One index serves nodes and relationships: a `(class, key)` pair is built
//! from the scratch items on its first seek and kept current by posting,
//! which only `WriteScratch::push_redo` drives. The class of a [`Node`] is
//! each of its labels plus the any-label bucket; the class of a
//! [`Relationship`] is its single type. Postings are additions only: an id
//! stays after its item loses the label or value, or is deleted, so every
//! seek result is a superset the caller re-verifies on its view.

use std::collections::{HashMap, HashSet};

use cypherlite_core::{Node, NodeId, RelId, Relationship, Value};

use crate::group_key::GroupKey;

/// The bucket of a node seek that names no label: every scratch node
/// carrying the key, whatever its labels. Nodes only — a relationship's
/// single class is its type.
const ANY_LABEL: &str = "";

/// A scratch item the equality index spans. The index keys its pairs on
/// `(class, key)` and reads the item's properties; the id comes from the
/// scratch map key.
pub(super) trait ScratchItem {
    /// The scratch map key of this item kind ([`NodeId`] / [`RelId`]).
    type Id: Copy + Eq + std::hash::Hash;

    /// The item's properties: scanned on a pair's first build, and the
    /// source a label-gain post reads.
    fn properties(&self) -> &cypherlite_core::PropertyMap;

    /// Whether the item belongs to `class`. A pair build for `class`
    /// selects the items that belong to it; a post names its target
    /// classes explicitly. For a [`Node`] a class is a label or the
    /// any-label bucket; for a [`Relationship`] its single type.
    fn has_class(&self, class: &str) -> bool;
}

impl ScratchItem for Node {
    type Id = NodeId;

    fn properties(&self) -> &cypherlite_core::PropertyMap {
        &self.properties
    }

    fn has_class(&self, class: &str) -> bool {
        class == ANY_LABEL || self.labels.iter().any(|l| l == class)
    }
}

impl ScratchItem for Relationship {
    type Id = RelId;

    fn properties(&self) -> &cypherlite_core::PropertyMap {
        &self.properties
    }

    fn has_class(&self, class: &str) -> bool {
        self.rel_type == class
    }
}

type Postings<Id> = HashMap<GroupKey, HashSet<Id>>;

#[derive(Clone, Debug)]
pub(super) struct ScratchPropIndex<T: ScratchItem> {
    /// `key -> class -> value -> ids` for the built pairs, keyed key first
    /// so a post looks up borrowed `&str`s.
    pairs: HashMap<String, HashMap<String, Postings<T::Id>>>,
    _item: std::marker::PhantomData<T>,
}

/// Manual so `T` needs no `Default` (the index holds no `T`s).
impl<T: ScratchItem> Default for ScratchPropIndex<T> {
    fn default() -> Self {
        Self {
            pairs: HashMap::new(),
            _item: std::marker::PhantomData,
        }
    }
}

/// The item-generic core: pair building, single-class seeks, and posting.
/// The node- and relationship-specific `impl` blocks below wrap it.
impl<T: ScratchItem> ScratchPropIndex<T> {
    /// Ids of `items` in `class` with `key == value`, building the pair on
    /// first use. A superset: the caller re-verifies every id. Empty when
    /// `value` has no property-index key (it contains a NaN).
    fn seek_class(
        &mut self,
        items: &HashMap<T::Id, T>,
        class: &str,
        key: &str,
        value: &Value,
    ) -> HashSet<T::Id> {
        self.bucket(items, class, key, value)
            .cloned()
            .unwrap_or_default()
    }

    /// The size of one class's seek bucket, counted in place (ADR-0184):
    /// [`Self::seek_class`] without materializing the ids.
    fn seek_class_len(
        &mut self,
        items: &HashMap<T::Id, T>,
        class: &str,
        key: &str,
        value: &Value,
    ) -> usize {
        self.bucket(items, class, key, value)
            .map_or(0, HashSet::len)
    }

    /// The built bucket of one `(class, key)` pair at `value`, building
    /// the pair from `items` on first use; `None` when `value` has no
    /// property-index key (it contains a NaN). The single pair lookup both
    /// [`Self::seek_class`] and [`Self::seek_class_len`] read, so the two
    /// cannot drift.
    fn bucket(
        &mut self,
        items: &HashMap<T::Id, T>,
        class: &str,
        key: &str,
        value: &Value,
    ) -> Option<&HashSet<T::Id>> {
        let group_key = GroupKey::for_property_index(value)?;
        self.pair(items, class, key).get(&group_key)
    }

    /// Post `id` under `value` into every built `(class, key)` pair of
    /// `classes`. Unbuilt pairs stay unbuilt (their build scans the
    /// items), and a NaN-bearing value is unindexable. A null value posts
    /// nothing — the one home of that policy: the set verb's
    /// null-means-remove contract never lands a null in a properties map
    /// (every producer of a property op pushes a Remove for a null), and
    /// the index treats a null like any other value it is never handed.
    /// The value's key is computed even when no class has a built pair —
    /// one wasted normalization for a flat loop.
    fn post<'c>(
        &mut self,
        id: T::Id,
        classes: impl IntoIterator<Item = &'c str>,
        key: &str,
        value: &Value,
    ) {
        if value.is_null() {
            return;
        }
        let Some(by_class) = self.pairs.get_mut(key) else {
            return;
        };
        let Some(g) = GroupKey::for_property_index(value) else {
            return;
        };
        for class in classes {
            if let Some(postings) = by_class.get_mut(class) {
                postings.entry(g.clone()).or_default().insert(id);
            }
        }
    }

    /// The `(class, key)` pair, built from `items` on first use.
    fn pair(&mut self, items: &HashMap<T::Id, T>, class: &str, key: &str) -> &Postings<T::Id> {
        let built = self
            .pairs
            .get(key)
            .is_some_and(|by_class| by_class.contains_key(class));
        if !built {
            let mut postings = Postings::new();
            for (id, item) in items {
                if !item.has_class(class) {
                    continue;
                }
                if let Some(g) = item
                    .properties()
                    .get(key)
                    .and_then(GroupKey::for_property_index)
                {
                    postings.entry(g).or_default().insert(*id);
                }
            }
            self.pairs
                .entry(key.to_string())
                .or_default()
                .insert(class.to_string(), postings);
        }
        &self.pairs[key][class]
    }
}

/// The node surface: classes are labels plus the any-label bucket.
impl ScratchPropIndex<Node> {
    /// Ids of `nodes` with `key == value` under any of `labels` (any node
    /// with the key when `labels` is empty), building each pair on first
    /// use. A superset: the caller re-verifies every id. Empty when `value`
    /// has no property-index key (it contains a NaN).
    pub(super) fn seek<S: AsRef<str>>(
        &mut self,
        nodes: &HashMap<NodeId, Node>,
        labels: &[S],
        key: &str,
        value: &Value,
    ) -> HashSet<NodeId> {
        let any = labels.is_empty().then_some(ANY_LABEL);
        let mut out = HashSet::new();
        for label in any.into_iter().chain(labels.iter().map(AsRef::as_ref)) {
            if let Some(ids) = self.bucket(nodes, label, key, value) {
                out.extend(ids.iter().copied());
            }
        }
        out
    }

    /// The size of one label's seek bucket, counted in place (ADR-0184):
    /// [`Self::seek`] for a single label without materializing the ids.
    /// Empty when `value` has no property-index key (it contains a NaN).
    pub(super) fn seek_len(
        &mut self,
        nodes: &HashMap<NodeId, Node>,
        label: &str,
        key: &str,
        value: &Value,
    ) -> usize {
        self.seek_class_len(nodes, label, key, value)
    }

    /// `id` newly carries `key = value` (a create or a `SET`): post it
    /// under the any-label bucket and each of the node's `labels` — the
    /// node's classes, per [`ScratchItem::has_class`].
    pub(super) fn post_value(&mut self, id: NodeId, labels: &[String], key: &str, value: &Value) {
        let classes = std::iter::once(ANY_LABEL).chain(labels.iter().map(String::as_str));
        self.post(id, classes, key, value);
    }

    /// `id` newly carries the `added` labels: post each of its `properties`
    /// under each added label. Its any-label pairings are unchanged, since
    /// a label changes no value.
    pub(super) fn post_labels(
        &mut self,
        id: NodeId,
        added: &[String],
        properties: &cypherlite_core::PropertyMap,
    ) {
        for (key, value) in properties {
            self.post(id, added.iter().map(String::as_str), key, value);
        }
    }
}

/// The relationship surface: the one class is the relationship's type.
impl ScratchPropIndex<Relationship> {
    /// Ids of `rels` of `rel_type` with `key == value`, building the pair
    /// on first use. A superset: the caller re-verifies every id. Empty
    /// when `value` has no property-index key (it contains a NaN).
    pub(super) fn seek(
        &mut self,
        rels: &HashMap<RelId, Relationship>,
        rel_type: &str,
        key: &str,
        value: &Value,
    ) -> HashSet<RelId> {
        self.seek_class(rels, rel_type, key, value)
    }

    /// The size of one type's seek bucket, counted in place (ADR-0184):
    /// [`Self::seek`] without materializing the ids. Empty when `value`
    /// has no property-index key (it contains a NaN).
    pub(super) fn seek_len(
        &mut self,
        rels: &HashMap<RelId, Relationship>,
        rel_type: &str,
        key: &str,
        value: &Value,
    ) -> usize {
        self.seek_class_len(rels, rel_type, key, value)
    }

    /// `id` newly carries `key = value` under `rel_type` (a create or a
    /// `SET`): post it into the built pair — the relationship's one
    /// class, per [`ScratchItem::has_class`].
    pub(super) fn post_value(&mut self, id: RelId, rel_type: &str, key: &str, value: &Value) {
        self.post(id, [rel_type], key, value);
    }
}

#[cfg(test)]
mod tests;
