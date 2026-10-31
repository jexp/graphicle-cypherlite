//! Index seeks over the overlay: the base graph's answer at the snapshot,
//! minus scratch deletions, plus scratch nodes that satisfy the predicate.

use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{NodeId, Value};

use crate::graph::{spatial_holds, RangeSeek, SpatialSeek};

use super::{ExecGraph, WriteScratch};

impl<'a> ExecGraph<'a> {
    pub fn lookup_nodes(
        &self,
        labels: &[String],
        props: &BTreeMap<String, Value>,
    ) -> Option<Vec<NodeId>> {
        if labels.is_empty() && props.is_empty() {
            return None;
        }
        let mut ids = self
            .base
            .lookup_nodes_at(labels, props, self.snapshot_ts())
            .unwrap_or_default();
        // Existence check is clone-free (node_exists, not node()).
        ids.retain(|id| self.node_exists(*id));
        // Add scratch matches. A property seek uses the (label,key) index so it
        // does not scan all scratch nodes; a label-only seek still scans (the
        // result is the whole label set either way).
        if let Some(cell) = self.scratch {
            let seen: HashSet<NodeId> = ids.iter().copied().collect();
            if props.is_empty() {
                let s = cell.borrow();
                for (id, n) in s.batch.nodes.iter() {
                    if !seen.contains(id) && labels.iter().all(|l| n.labels.iter().any(|x| x == l))
                    {
                        ids.push(*id);
                    }
                }
            } else {
                let s = &mut *cell.borrow_mut();
                // Props intersect (AND), matching the base lookup.
                let mut scratch: Option<HashSet<NodeId>> = None;
                for (k, v) in props {
                    let hits = s.batch.prop_ix.seek(&s.batch.nodes, labels, k, v);
                    scratch = Some(match scratch {
                        None => hits,
                        Some(prev) => prev.intersection(&hits).copied().collect(),
                    });
                }
                for id in scratch.unwrap_or_default() {
                    if !seen.contains(&id) {
                        ids.push(id);
                    }
                }
            }
        }
        ids.sort();
        Some(ids)
    }

    /// Committed postings plus the scratch's, for one equality probe
    /// (ADR-0172 §3, ADR-0185): ascending, deduplicated. A superset — the
    /// caller re-verifies every id on this view.
    pub(super) fn merged_candidates<Id: Ord + Copy>(
        &self,
        committed: Vec<Id>,
        scratch: impl FnOnce(&mut WriteScratch) -> HashSet<Id>,
    ) -> Vec<Id> {
        let mut ids = committed;
        if let Some(cell) = self.scratch {
            let s = &mut *cell.borrow_mut();
            ids.extend(scratch(s));
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Committed bucket length plus the scratch's, both counted in place
    /// (ADR-0184): a composite uniqueness probe compares these sizes
    /// without materializing any candidate list.
    pub(super) fn merged_len(
        &self,
        committed: usize,
        scratch: impl FnOnce(&mut WriteScratch) -> usize,
    ) -> usize {
        committed
            + self.scratch.map_or(0, |cell| {
                let s = &mut *cell.borrow_mut();
                scratch(s)
            })
    }

    /// Candidate ids for a node uniqueness probe on `(label, key) = value`
    /// (ADR-0172 §3), ascending and deduplicated: the committed graph's lazy
    /// equality postings for the pair plus the scratch's `prop_ix` seek.
    /// A superset — the caller re-verifies every id on this view. Empty when
    /// `value` has no property-index key (it contains a NaN).
    pub(crate) fn equality_candidates(&self, label: &str, key: &str, value: &Value) -> Vec<NodeId> {
        self.merged_candidates(self.base.equality_postings(label, key, value), |s| {
            s.batch.prop_ix.seek(&s.batch.nodes, &[label], key, value)
        })
    }

    /// The size of the committed plus scratch equality bucket for
    /// `(label, key) = value` (ADR-0184), counted in place: the pair
    /// selection of a composite uniqueness probe compares these sizes
    /// without materializing any candidate list. Zero when `value` has no
    /// property-index key (it contains a NaN).
    pub(crate) fn equality_bucket_len(&self, label: &str, key: &str, value: &Value) -> usize {
        self.merged_len(self.base.equality_postings_len(label, key, value), |s| {
            s.batch.prop_ix.seek_len(&s.batch.nodes, label, key, value)
        })
    }

    /// `None` when the base index cannot serve the needle (the gram index
    /// rejects empty needles): the caller falls back to a full scan, exactly
    /// like the `lookup_nodes` path, rather than matching nothing.
    pub fn lookup_string_contains(
        &self,
        labels: &[String],
        key: &str,
        needle: &str,
    ) -> Option<Vec<NodeId>> {
        self.merge_string_lookup(
            labels,
            key,
            |n| n.as_string().is_some_and(|s| s.contains(needle)),
            || {
                self.base
                    .lookup_string_contains_at(labels, key, needle, self.snapshot_ts())
            },
        )
    }

    /// `None` when no `RANGE` index serves the prefix: the caller scans.
    pub fn lookup_string_prefix(
        &self,
        labels: &[String],
        key: &str,
        prefix: &str,
    ) -> Option<Vec<NodeId>> {
        self.merge_string_lookup(
            labels,
            key,
            |n| n.as_string().is_some_and(|s| s.starts_with(prefix)),
            || {
                self.base
                    .lookup_string_prefix_at(labels, key, prefix, self.snapshot_ts())
            },
        )
    }

    pub(crate) fn lookup_range(
        &self,
        labels: &[String],
        key: &str,
        seek: &RangeSeek,
    ) -> Option<Vec<NodeId>> {
        self.merge_string_lookup(
            labels,
            key,
            |v| Self::range_overlay_pred(v, seek),
            || {
                self.base
                    .lookup_range_seek(labels, key, seek, self.snapshot_ts())
            },
        )
    }

    /// Upper bound on [`Self::lookup_range`], counted from posting lengths.
    /// `None` when the index cannot serve the predicate or the write overlay
    /// has created, updated, or deleted a node: the committed postings are
    /// then an incomplete description of this view, and the caller keeps the
    /// eager probe.
    pub(crate) fn lookup_range_cardinality(
        &self,
        labels: &[String],
        key: &str,
        seek: &RangeSeek,
    ) -> Option<usize> {
        if self.overlay_changes_node_membership() {
            return None;
        }
        self.base.range_seek_cardinality(labels, key, seek)
    }

    pub(crate) fn lookup_spatial(
        &self,
        labels: &[String],
        key: &str,
        seek: &SpatialSeek,
    ) -> Option<Vec<NodeId>> {
        self.merge_string_lookup(
            labels,
            key,
            |v| spatial_holds(v, seek),
            || {
                self.base
                    .lookup_spatial_seek(labels, key, seek, self.snapshot_ts())
            },
        )
    }

    fn range_overlay_pred(v: &Value, seek: &RangeSeek) -> bool {
        match seek {
            RangeSeek::Eq(b) => v.eq_non_null(b),
            RangeSeek::In(xs) => xs.iter().any(|b| v.eq_non_null(b)),
            RangeSeek::Cmp { op, bound } => {
                // Superset of the committed comparison. An incomparable value
                // or an unexpected operator stays in the candidate set; the
                // residual predicate in expand rechecks it.
                let Some(ord) = v.compare(bound) else {
                    return true;
                };
                match op {
                    cypherlite_ir::BinaryOp::Lt => ord.is_lt(),
                    cypherlite_ir::BinaryOp::Gt => ord.is_gt(),
                    cypherlite_ir::BinaryOp::Lte => ord.is_le(),
                    cypherlite_ir::BinaryOp::Gte => ord.is_ge(),
                    _ => true,
                }
            }
        }
    }

    fn merge_string_lookup(
        &self,
        labels: &[String],
        key: &str,
        pred: impl Fn(&Value) -> bool,
        base: impl FnOnce() -> Option<Vec<NodeId>>,
    ) -> Option<Vec<NodeId>> {
        let mut ids = base()?;
        ids.retain(|id| self.node_exists(*id));
        if let Some(cell) = self.scratch {
            for id in cell.borrow().batch.nodes.keys() {
                if ids.contains(id) {
                    continue;
                }
                if let Some(n) = self.node(*id) {
                    if labels.iter().all(|l| n.labels.iter().any(|x| x == l))
                        && n.properties.get(key).is_some_and(&pred)
                    {
                        ids.push(*id);
                    }
                }
            }
        }
        ids.sort();
        Some(ids)
    }
}
