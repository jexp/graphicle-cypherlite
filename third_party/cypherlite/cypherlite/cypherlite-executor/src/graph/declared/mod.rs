//! `CREATE TEXT INDEX`, `CREATE RANGE INDEX`, and `CREATE POINT INDEX`
//! access methods. Each structure exists only for a catalog pair and is
//! built on create, on startup (`rebuild_indexes`), and incrementally on
//! later writes. A seek with no matching index returns `None` so the
//! caller scans the label and evaluates the predicate.

use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{NodeId, Value};

use crate::redo::{IndexDetailSpec, IndexKindSpec};

use super::*;

mod order;
mod point;
mod text;

pub(super) use order::RangeIndex;
pub(crate) use order::{RangeSeek, SpatialSeek};
pub(crate) use point::spatial_holds;
pub(super) use point::PointIndex;
pub(crate) use text::trigram_count;
pub(super) use text::GramIndex;

/// One declared index's postings for a `(label, property)` pair. The
/// graph wiring below is written once against this trait.
pub(super) trait DrainState: Default {
    fn dirty(&self) -> &HashSet<NodeId>;
    fn dirty_mut(&mut self) -> &mut HashSet<NodeId>;
    /// Post one version's value for `id`. A value of another type posts
    /// nothing.
    fn insert(&mut self, id: NodeId, value: &Value);
    /// Drop every posting of `id`.
    fn remove_node(&mut self, id: NodeId);
}

impl Graph {
    pub(super) fn declared_dirty(&self, lid: u32, kid: u32, id: NodeId) {
        Self::mark_dirty(&self.indexes.gram_pairs, lid, kid, id);
        Self::mark_dirty(&self.indexes.range_pairs, lid, kid, id);
        Self::mark_dirty(&self.indexes.point_pairs, lid, kid, id);
    }

    fn mark_dirty<St: DrainState>(pairs: &CoreMap<(u32, u32), St>, lid: u32, kid: u32, id: NodeId) {
        if let Some(mut st) = pairs.get_mut(&(lid, kid)) {
            st.dirty_mut().insert(id);
        }
    }

    /// `(gram entries, dirty ids)` when a text index has materialized this pair.
    #[cfg(test)]
    pub(super) fn gram_pair_state(&self, label: &str, key: &str) -> Option<(usize, usize)> {
        let pk = self.string_pair_id(label, key)?;
        self.indexes
            .gram_pairs
            .get(&pk)
            .map(|st| (st.grams.len(), st.dirty.len()))
    }

    pub(super) fn materialize_detail(&self, detail: &IndexDetailSpec) {
        for (label, property, kind) in detail.covered_pairs() {
            match kind {
                IndexKindSpec::Text => self.materialize_text_index(label, property),
                IndexKindSpec::Range => self.materialize_range_index(label, property),
                IndexKindSpec::Point => self.materialize_point_index(label, property),
                _ => {}
            }
        }
    }

    pub(super) fn drop_detail(&self, detail: &IndexDetailSpec) {
        let remaining: Vec<IndexDetailSpec> = self
            .theater
            .lock()
            .unwrap()
            .index_details
            .values()
            .cloned()
            .collect();
        for (label, property, kind) in detail.covered_pairs() {
            let still = remaining.iter().any(|other| {
                other
                    .covered_pairs()
                    .iter()
                    .any(|(l, p, k)| *l == label && *p == property && *k == kind)
            });
            if still {
                continue;
            }
            let Some(pk) = self.string_pair_id(label, property) else {
                continue;
            };
            match kind {
                IndexKindSpec::Text => {
                    self.indexes.gram_pairs.remove(&pk);
                }
                IndexKindSpec::Range => {
                    self.indexes.range_pairs.remove(&pk);
                }
                IndexKindSpec::Point => {
                    self.indexes.point_pairs.remove(&pk);
                }
                _ => {}
            }
        }
    }

    pub(super) fn rebuild_declared_indexes(&self) {
        self.indexes.gram_pairs.clear();
        self.indexes.range_pairs.clear();
        self.indexes.point_pairs.clear();
        let details: Vec<IndexDetailSpec> = self
            .theater
            .lock()
            .unwrap()
            .index_details
            .values()
            .cloned()
            .collect();
        for detail in details {
            self.materialize_detail(&detail);
        }
    }

    /// The node `(label, property)` pairs the store's RANGE indexes serve an
    /// equality seek on, read from the typed catalog details
    /// ([`IndexDetailSpec::range_seek_pairs`]) rather than re-parsed from the
    /// rendered `:Label(property)` text: every component of a composite range
    /// index is covered, a relationship index contributes no node pair, and a
    /// comma-bearing property name stays one name.
    pub fn range_indexed_node_properties(&self) -> Vec<(String, String)> {
        self.theater
            .lock()
            .unwrap()
            .index_details
            .values()
            .flat_map(IndexDetailSpec::range_seek_pairs)
            .map(|(label, property)| (label.to_string(), property.to_string()))
            .collect()
    }

    fn intern_pair(&self, label: &str, key: &str) -> (u32, u32) {
        (self.interner.intern(label), self.interner.intern(key))
    }

    /// Labels among `labels` whose pair for `key` exists in `pairs`.
    /// `None` when none do (the seek must scan). An empty pattern label
    /// list cannot be served: an index on one label would miss the others.
    fn indexed_labels<St>(
        &self,
        labels: &[String],
        key: &str,
        pairs: &CoreMap<(u32, u32), St>,
    ) -> Option<Vec<u32>> {
        if labels.is_empty() {
            return None;
        }
        let kid = self.interner.get_id(key)?;
        let ids: Vec<u32> = labels
            .iter()
            .filter_map(|l| self.interner.get_id(l))
            .filter(|lid| pairs.contains_key(&(*lid, kid)))
            .collect();
        (!ids.is_empty()).then_some(ids)
    }

    /// Build one pair from every version of every node in the label index
    /// (a superset; the seek re-checks the snapshot, ADR-0018).
    fn materialize_pair<St: DrainState>(
        &self,
        pairs: &CoreMap<(u32, u32), St>,
        label: &str,
        property: &str,
    ) -> (u32, u32) {
        let (lid, kid) = self.intern_pair(label, property);
        let mut st = St::default();
        self.for_each_pair_value(lid, kid, |id, value| st.insert(id, value));
        pairs.insert((lid, kid), st);
        (lid, kid)
    }

    pub(super) fn materialize_text_index(&self, label: &str, property: &str) {
        let pair = self.materialize_pair(&self.indexes.gram_pairs, label, property);
        if let Some(mut st) = self.indexes.gram_pairs.get_mut(&pair) {
            st.seal();
        }
    }

    pub(super) fn materialize_range_index(&self, label: &str, property: &str) {
        self.materialize_pair(&self.indexes.range_pairs, label, property);
    }

    pub(super) fn materialize_point_index(&self, label: &str, property: &str) {
        self.materialize_pair(&self.indexes.point_pairs, label, property);
    }

    /// Repost every dirty node of one pair. The write guard is held across
    /// the whole drain, so a concurrent `declared_dirty` lands either before
    /// the take (reposted here) or after it (kept for the next drain).
    fn drain_pair<St: DrainState>(&self, pairs: &CoreMap<(u32, u32), St>, lid: u32, kid: u32) {
        let Some(mut st) = pairs.get_mut(&(lid, kid)) else {
            return;
        };
        if st.dirty().is_empty() {
            return;
        }
        let ids = std::mem::take(st.dirty_mut());
        for id in ids {
            st.remove_node(id);
            self.for_each_node_pair_value(id, lid, kid, |value| st.insert(id, value));
        }
    }

    /// The shared seek: drain each indexed label's pair, scan it, intersect
    /// across labels, and re-check the snapshot. `scan` returns `None` to
    /// decline (the caller scans), as does a pair that vanished between
    /// `indexed_labels` and here — a concurrent `DROP INDEX` must not be
    /// reported as zero rows.
    fn lookup_indexed<St: DrainState>(
        &self,
        pairs: &CoreMap<(u32, u32), St>,
        labels: &[String],
        key: &str,
        ts: u64,
        scan: impl Fn(&St) -> Option<HashSet<NodeId>>,
        pred: impl Fn(&Value) -> bool,
    ) -> Option<Vec<NodeId>> {
        let lids = self.indexed_labels(labels, key, pairs)?;
        let kid = self.interner.get_id(key)?;
        let mut acc: Option<HashSet<NodeId>> = None;
        for lid in lids {
            self.drain_pair(pairs, lid, kid);
            let st = pairs.get(&(lid, kid))?;
            let next = scan(&st)?;
            drop(st);
            intersect_acc(&mut acc, next);
        }
        Some(self.finish_ids(acc, labels, key, ts, pred))
    }

    pub(super) fn lookup_text_contains(
        &self,
        labels: &[String],
        key: &str,
        needle: &str,
        ts: u64,
    ) -> Option<Vec<NodeId>> {
        let grams = text::query_grams(needle)?;
        let pairs = &self.indexes.gram_pairs;
        let lids = self.indexed_labels(labels, key, pairs)?;
        let kid = self.interner.get_id(key)?;
        let mut acc: Option<HashSet<NodeId>> = None;
        for lid in lids {
            self.drain_pair(pairs, lid, kid);
            let mut st = pairs.get_mut(&(lid, kid))?;
            let next = text::intersect_grams(&mut st, &grams);
            drop(st);
            intersect_acc(&mut acc, next);
        }
        Some(self.finish_ids(acc, labels, key, ts, |v| {
            v.as_string().is_some_and(|s| s.contains(needle))
        }))
    }

    pub(super) fn lookup_text_prefix(
        &self,
        labels: &[String],
        key: &str,
        prefix: &str,
        ts: u64,
    ) -> Option<Vec<NodeId>> {
        self.lookup_indexed(
            &self.indexes.range_pairs,
            labels,
            key,
            ts,
            |st| Some(order::prefix_ids(&st.order, prefix)),
            |v| v.as_string().is_some_and(|s| s.starts_with(prefix)),
        )
    }

    pub(crate) fn lookup_range_seek(
        &self,
        labels: &[String],
        key: &str,
        seek: &RangeSeek,
        ts: u64,
    ) -> Option<Vec<NodeId>> {
        match seek {
            RangeSeek::Eq(v) => self.lookup_range_values(labels, key, std::slice::from_ref(v), ts),
            RangeSeek::In(vs) => self.lookup_range_values(labels, key, vs, ts),
            RangeSeek::Cmp { op, bound } => {
                order::order_key(bound)?;
                let op = *op;
                self.lookup_indexed(
                    &self.indexes.range_pairs,
                    labels,
                    key,
                    ts,
                    // A stored temporal outside the order-key window is not
                    // posted; the index cannot answer a comparison then.
                    |st| {
                        st.unbounded
                            .is_empty()
                            .then(|| order::numeric_ids(&st.order, op, bound))
                    },
                    |v| order::cmp_holds(v, op, bound),
                )
            }
        }
    }

    /// Upper bound on the ids [`Self::lookup_range_seek`] would collect from
    /// the index, counted from posting lengths. `None` when the index cannot
    /// serve the predicate. A comparison with unbounded temporal postings
    /// declines, matching the id seek. The bound is not snapshot-filtered.
    pub(crate) fn range_seek_cardinality(
        &self,
        labels: &[String],
        key: &str,
        seek: &RangeSeek,
    ) -> Option<usize> {
        match seek {
            RangeSeek::Eq(v) => self.range_values_cardinality(labels, key, std::slice::from_ref(v)),
            RangeSeek::In(vs) => self.range_values_cardinality(labels, key, vs),
            RangeSeek::Cmp { op, bound } => {
                order::order_key(bound)?;
                let op = *op;
                self.range_pair_cost(labels, key, |st| {
                    st.unbounded
                        .is_empty()
                        .then(|| order::numeric_count(&st.order, op, bound))
                })
            }
        }
    }

    /// Work upper bound for one range seek across the indexed labels. `Some(0)`
    /// when any label posts nothing, because the label intersection is empty.
    fn range_pair_cost(
        &self,
        labels: &[String],
        key: &str,
        count: impl Fn(&order::RangeIndex) -> Option<usize>,
    ) -> Option<usize> {
        let lids = self.indexed_labels(labels, key, &self.indexes.range_pairs)?;
        let kid = self.interner.get_id(key)?;
        let mut cost = 0usize;
        for lid in lids {
            self.drain_pair(&self.indexes.range_pairs, lid, kid);
            let st = self.indexes.range_pairs.get(&(lid, kid))?;
            let n = count(&st)?;
            if n == 0 {
                return Some(0);
            }
            cost = cost.saturating_add(n);
        }
        Some(cost)
    }

    fn range_values_cardinality(
        &self,
        labels: &[String],
        key: &str,
        values: &[Value],
    ) -> Option<usize> {
        let mut total = 0usize;
        let mut served = false;
        for value in values {
            if GroupKey::for_property_index(value).is_none() {
                continue;
            }
            let n =
                self.range_pair_cost(labels, key, |st| Some(order::equality_count(st, value)))?;
            served = true;
            total = total.saturating_add(n);
        }
        served.then_some(total)
    }

    fn lookup_range_values(
        &self,
        labels: &[String],
        key: &str,
        values: &[Value],
        ts: u64,
    ) -> Option<Vec<NodeId>> {
        let lids = self.indexed_labels(labels, key, &self.indexes.range_pairs)?;
        let kid = self.interner.get_id(key)?;
        let mut matched = HashSet::new();
        for v in values {
            if GroupKey::for_property_index(v).is_none() {
                continue;
            }
            let mut acc: Option<HashSet<NodeId>> = None;
            for lid in &lids {
                let ids = self.range_equality_ids(*lid, kid, v)?;
                intersect_acc(&mut acc, ids);
            }
            if let Some(ids) = acc {
                matched.extend(ids);
            }
        }
        Some(self.finish_ids(Some(matched), labels, key, ts, |v| {
            values.iter().any(|b| v.eq_non_null(b))
        }))
    }

    /// Drain the range pair and read its equality postings. `None` when the
    /// pair vanished (a concurrent `DROP INDEX`), so the caller scans
    /// instead of reporting zero rows.
    pub(super) fn range_equality_ids(
        &self,
        lid: u32,
        kid: u32,
        value: &Value,
    ) -> Option<HashSet<NodeId>> {
        self.drain_pair(&self.indexes.range_pairs, lid, kid);
        let st = self.indexes.range_pairs.get(&(lid, kid))?;
        Some(order::equality_ids(&st, value))
    }

    pub(crate) fn lookup_spatial_seek(
        &self,
        labels: &[String],
        key: &str,
        seek: &SpatialSeek,
        ts: u64,
    ) -> Option<Vec<NodeId>> {
        self.lookup_indexed(
            &self.indexes.point_pairs,
            labels,
            key,
            ts,
            |st| Some(point::spatial_ids(st, seek)),
            |v| point::spatial_holds(v, seek),
        )
    }

    fn finish_ids(
        &self,
        acc: Option<HashSet<NodeId>>,
        labels: &[String],
        key: &str,
        ts: u64,
        pred: impl Fn(&Value) -> bool,
    ) -> Vec<NodeId> {
        let mut ids: Vec<_> = acc
            .unwrap_or_default()
            .into_iter()
            .filter(|id| {
                self.node_at(*id, ts).is_some_and(|n| {
                    Self::node_matches(&n, labels, &BTreeMap::new())
                        && n.properties.get(key).is_some_and(&pred)
                })
            })
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// True when every property has a range index on at least one pattern label.
    pub(super) fn range_serves_equality(
        &self,
        labels: &[String],
        props: &BTreeMap<String, Value>,
    ) -> bool {
        if labels.is_empty() || props.is_empty() {
            return false;
        }
        props.keys().all(|k| {
            labels.iter().any(|l| {
                self.string_pair_id(l, k)
                    .is_some_and(|pk| self.indexes.range_pairs.contains_key(&pk))
            })
        })
    }
}

/// Intersect `next` into the accumulator: the first set becomes the
/// accumulator, every later one narrows it. The canonical accumulator fold
/// for every multi-set seek (declared indexes and equality lookups).
pub(super) fn intersect_acc(acc: &mut Option<HashSet<NodeId>>, next: HashSet<NodeId>) {
    *acc = Some(match acc.take() {
        None => next,
        Some(prev) => prev.intersection(&next).copied().collect(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_remove_drops_only_that_node() {
        let mut st = RangeIndex::default();
        order::insert_order(&mut st, &Value::Int(1), 10);
        order::insert_order(&mut st, &Value::Int(2), 11);
        order::order_remove_node(&mut st, 10);
        assert!(!st
            .order
            .contains_key(&order::order_key(&Value::Int(1)).unwrap()));
        assert!(st
            .order
            .get(&order::order_key(&Value::Int(2)).unwrap())
            .unwrap()
            .contains(11));
    }

    #[test]
    fn string_order_key_shares_the_value_payload() {
        let payload: Arc<str> = "Ada".into();
        let value = Value::String(Arc::clone(&payload));
        let mut st = RangeIndex::default();
        order::insert_order(&mut st, &value, 10);
        order::insert_order(&mut st, &Value::String("Bob".into()), 11);
        let posted = st.by_node[&10].first().unwrap();
        assert!(matches!(posted, order::OrderKey::String(s) if Arc::ptr_eq(s, &payload)));
        assert!(st
            .order
            .keys()
            .any(|k| matches!(k, order::OrderKey::String(s) if Arc::ptr_eq(s, &payload))));
        assert_eq!(order::prefix_ids(&st.order, "A"), HashSet::from([10]));
    }

    #[test]
    fn gram_remove_drops_only_that_node() {
        let mut st = GramIndex::default();
        text::gram_insert(&mut st, &Arc::from("abc"), 10);
        text::gram_insert(&mut st, &Arc::from("zzz"), 11);
        text::gram_remove_node(&mut st, 10);
        assert!(st.grams.values().all(|ids| !ids.holds(10)));
        assert!(st.grams.values().any(|ids| ids.holds(11)));
    }

    #[test]
    fn out_of_window_datetime_is_not_an_order_key() {
        use chrono::NaiveDate;
        use cypherlite_core::Temporal;
        let far = NaiveDate::from_ymd_opt(9999, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        let value = Value::Temporal(Temporal::LocalDateTime(far));
        let mut st = RangeIndex::default();
        order::insert_order(&mut st, &value, 7);
        assert!(st.order.is_empty());
        assert!(st.unbounded.contains(&7));
        order::order_remove_node(&mut st, 7);
        assert!(st.unbounded.is_empty());
    }

    #[test]
    fn cells_in_scans_occupied_cells_when_the_rectangle_is_huge() {
        let mut st = PointIndex::default();
        point::point_insert(&mut st, 1, 7203, 1.0, 1.0);
        let ids = point::cells_in(&st, -1.0e12, -1.0e12, 1.0e12, 1.0e12);
        assert!(ids.contains(&1));
        let ids = point::cells_in(&st, 1.0e308, 1.0e308, -1.0e308, -1.0e308);
        assert!(ids.is_empty());
    }
}
