//! The index-backed lookup and scan planning (ADR-0218, ADR-0063): the
//! label/equality candidate sets, the residual scan, and the plan-time
//! cardinality reads. A sibling of `index.rs`.

use super::declared::intersect_acc;
use super::*;

enum NodeCandidates {
    /// No labels and no properties: the caller scans.
    Unindexed,
    /// Index postings that still need `node_matches`.
    Indexed(HashSet<NodeId>),
    /// A finished id list. Empty means the predicate cannot match.
    Final(Vec<NodeId>),
}

impl Graph {
    /// Approximate number of nodes carrying `label`: the label-index set
    /// size (ADR-0218). Plan-time estimate only — it is O(1), ignores
    /// snapshot visibility, and counts every id the index holds; a label
    /// the store has never interned is 0.
    pub fn label_count(&self, label: &str) -> u64 {
        self.interner
            .get_id(label)
            .and_then(|lid| self.indexes.label_index.get(&lid))
            .map_or(0, |ids| ids.len() as u64)
    }

    pub fn lookup_nodes(
        &self,
        labels: &[String],
        props: &BTreeMap<String, Value>,
    ) -> Option<Vec<NodeId>> {
        self.lookup_nodes_at(labels, props, self.read_ts())
    }

    pub fn lookup_nodes_at(
        &self,
        labels: &[String],
        props: &BTreeMap<String, Value>,
        ts: u64,
    ) -> Option<Vec<NodeId>> {
        match self.node_candidates(labels, props, ts) {
            NodeCandidates::Unindexed => None,
            NodeCandidates::Final(ids) => Some(ids),
            NodeCandidates::Indexed(acc) => {
                let mut ids: Vec<_> = acc
                    .into_iter()
                    .filter(|id| {
                        self.node_at(*id, ts)
                            .is_some_and(|n| Self::node_matches(&n, labels, props))
                    })
                    .collect();
                ids.sort();
                Some(ids)
            }
        }
    }

    /// The ids posted under every label in `labels`: the label-index
    /// buckets intersected through `intersect_acc`, the fold every
    /// multi-set seek uses. A label the store never interned can match
    /// nothing, so its empty bucket empties the result. Callers route a
    /// label-less pattern to the full scan — this is only reached with a
    /// non-empty `labels`.
    fn intersect_label_ids(&self, labels: &[String]) -> HashSet<NodeId> {
        let mut acc: Option<HashSet<NodeId>> = None;
        for label in labels {
            // An uninterned label can match nothing.
            let ids = self
                .interner
                .get_id(label)
                .and_then(|lid| self.indexes.label_index.get(&lid).map(|r| r.clone()))
                .unwrap_or_default();
            intersect_acc(&mut acc, ids);
        }
        acc.unwrap_or_default()
    }

    /// Index postings for a node lookup, or a finished answer when the
    /// index cannot serve the predicate.
    fn node_candidates(
        &self,
        labels: &[String],
        props: &BTreeMap<String, Value>,
        ts: u64,
    ) -> NodeCandidates {
        if labels.is_empty() && props.is_empty() {
            return NodeCandidates::Unindexed;
        }
        if !labels.is_empty() && props.is_empty() {
            return NodeCandidates::Indexed(self.intersect_label_ids(labels));
        }
        if props.is_empty() || !self.range_serves_equality(labels, props) {
            return NodeCandidates::Final(self.scan_match(labels, props, ts));
        }
        let mut acc: Option<HashSet<NodeId>> = None;
        for (key, value) in props {
            if GroupKey::for_property_index(value).is_none() {
                return NodeCandidates::Final(Vec::new());
            }
            let Some(kid) = self.interner.get_id(key) else {
                return NodeCandidates::Final(Vec::new());
            };
            let mut next: Option<HashSet<NodeId>> = None;
            for label in labels {
                let Some(lid) = self.interner.get_id(label) else {
                    continue;
                };
                if !self.indexes.range_pairs.contains_key(&(lid, kid)) {
                    continue;
                }
                let Some(ids) = self.range_equality_ids(lid, kid, value) else {
                    return NodeCandidates::Final(self.scan_match(labels, props, ts));
                };
                intersect_acc(&mut next, ids);
            }
            intersect_acc(&mut acc, next.unwrap_or_default());
        }
        NodeCandidates::Indexed(acc.unwrap_or_default())
    }

    /// Label scan (or a full scan when the pattern has no label) filtered by
    /// equality. Used when no `RANGE` index serves the properties.
    fn scan_match(
        &self,
        labels: &[String],
        props: &BTreeMap<String, Value>,
        ts: u64,
    ) -> Vec<NodeId> {
        let mut ids: Vec<NodeId> = if labels.is_empty() {
            self.all_node_ids_at(ts)
        } else {
            self.intersect_label_ids(labels).into_iter().collect()
        };
        ids.retain(|id| {
            self.node_at(*id, ts)
                .is_some_and(|n| Self::node_matches(&n, labels, props))
        });
        ids.sort();
        ids
    }

    pub fn lookup_string_contains_at(
        &self,
        labels: &[String],
        key: &str,
        needle: &str,
        ts: u64,
    ) -> Option<Vec<NodeId>> {
        self.lookup_text_contains(labels, key, needle, ts)
    }

    /// `None` unless a `RANGE` index names the `(label, property)`.
    /// `STARTS WITH` reads that ordered map; without one the caller scans.
    pub fn lookup_string_prefix_at(
        &self,
        labels: &[String],
        key: &str,
        prefix: &str,
        ts: u64,
    ) -> Option<Vec<NodeId>> {
        self.lookup_text_prefix(labels, key, prefix, ts)
    }
}
