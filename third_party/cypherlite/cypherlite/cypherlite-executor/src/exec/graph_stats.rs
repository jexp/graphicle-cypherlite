//! The one live-graph [`cypherlite_planner::PatternStats`] implementation (LABS-351): the
//! executor's compound start-element estimate consumes it directly, and
//! the top-level crate's plan-time stats adapter delegates to it, so
//! one implementation answers both consumers.

use std::collections::BTreeMap;

use cypherlite_core::Value;
use cypherlite_ir::{Expr, NodePattern, RelPattern};
use cypherlite_planner::{apply_degree, endpoint_estimate, seed_on_rel_start, seed_rows, Bound};

use crate::graph::Graph;

/// Construction goes through [`GraphStats::new`] (the ADR-0218 one-
/// construction idiom the sibling plan-time stats impls use), so an
/// inline build cannot drift from it.
pub struct GraphStats<'a> {
    graph: &'a Graph,
    params: &'a BTreeMap<String, Value>,
}

impl<'a> GraphStats<'a> {
    /// The one construction: the connection's graph and the statement's
    /// parameters.
    pub fn new(graph: &'a Graph, params: &'a BTreeMap<String, Value>) -> GraphStats<'a> {
        GraphStats { graph, params }
    }

    /// One element's compound start estimate (LABS-351): the planner's
    /// canonical endpoint ladder over this graph's stats, mapped onto
    /// the one u64 scale the executor compares. The rank sentinels
    /// share the measured scale (`direction_cost` compares them
    /// directly); the bare rank — an unlabeled element, an all-node
    /// scan — stays the whole-graph sentinel.
    pub(crate) fn estimate(
        &self,
        np: &NodePattern,
        bound: Bound<'_>,
        where_conjuncts: impl IntoIterator<Item = &'a Expr>,
        first_hop: Option<&RelPattern>,
        reversed: bool,
    ) -> u64 {
        let rows = seed_rows(endpoint_estimate(np, bound, where_conjuncts, self));
        // The start's own first hop scaled by the planner's own degree
        // scaling (q30's regression: a 1.6k-person endpoint with ~95
        // relationships each is NOT cheaper than a 151k-comment endpoint,
        // and a raw endpoint estimate chose it). The walk orientation is
        // the caller's — the planner's reversal already flipped the
        // directions (`reverse_path`), so the PLANNED spelling is its own
        // written direction and the forward walker passes false; the
        // backward walker (element_estimate's k > 0 pivots) passes true
        // — and the seed-side derivation is the planner's one typed
        // helper ([`seed_on_rel_start`]: an undirected hop has no start
        // side in either orientation), so the tail prices exactly the
        // walks whose seed rides the rel's start side (LABS-426, round 1
        // F-1-2/F-1-3: the hardcoded forward flag inverted the guard on
        // the backward walker's candidates).
        let rows = match first_hop {
            Some(rel) => apply_degree(rows, np, rel, seed_on_rel_start(rel, reversed), self),
            None => rows,
        };
        rows.min(u64::MAX as u128) as u64
    }
}

impl cypherlite_planner::PatternStats for GraphStats<'_> {
    fn label_rows(&self, label: &str) -> Option<u64> {
        Some(self.graph.label_count(label))
    }

    fn equality_rows(&self, label: &str, property: &str, value: &Expr) -> Option<u64> {
        let value = value.seek_value(self.params)?;
        self.graph.pattern_equality_rows(label, property, &value)
    }

    fn contains_rows(&self, label: &str, property: &str, needle: &Expr) -> Option<u64> {
        let needle = needle.seek_value(self.params)?;
        let Value::String(needle) = needle else {
            return None;
        };
        self.graph.pattern_contains_rows(label, property, &needle)
    }

    fn rel_type_rows(&self, rel_type: &str) -> Option<u64> {
        Some(self.graph.pattern_rel_type_rows(rel_type))
    }

    /// LABS-351: the live graph's histogram tail (the executor keeps the
    /// real one; the null stats default None).
    fn degree_tail(&self, label: &str, rel_type: &str) -> Option<u64> {
        self.graph.pattern_degree_tail(label, rel_type)
    }
}
