//! Cardinality estimates and MATCH pattern order.
//!
//! A missing count falls back to a rank sentinel. Measured counts may be
//! scaled by relationship degree; rank sentinels stay unscaled.

use std::collections::HashSet;

use cypherlite_ir::{key_of, BinaryOp, Expr, NodePattern, PathPattern, PatternElement, RelPattern};

use crate::rewrite::add_pattern_vars;
use crate::stats::PatternStats;

/// Synthetic row counts for the rank fallback. A higher rank is cheaper,
/// and a known row count below the label sentinel beats an unknown label.
const RANK_EQ: u128 = 1 << 20;
const RANK_LABEL: u128 = 1 << 48;
const RANK_BARE: u128 = 1 << 62;

/// One canonical endpoint ladder (LABS-351): a bound variable is one
/// row, then the best measured index signal (equality, CONTAINS, then
/// label rows), then the unmeasured rank sentinels. Measured counts and
/// rank sentinels share one ordered scale (the constants are ordered,
/// `direction_cost` compares them directly), so both the planner's
/// pattern ordering and the executor's start-element choice consume the
/// same estimate. `pub` for the executor's `cheapest_start`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Estimate {
    Measured(u128),
    Rank(u128),
}

/// The where clause as its conjunct list — the ladder's CONTAINS level
/// reads individual conjuncts, and the executor passes its residual
/// conjuncts in this shape natively.
fn where_conjuncts_of(where_clause: Option<&Expr>) -> Vec<&Expr> {
    let mut conjuncts = Vec::new();
    if let Some(expr) = where_clause {
        each_conjunct(expr, &mut |expr| conjuncts.push(expr));
    }
    conjuncts
}

fn unmeasured_rows(node: &NodePattern) -> u128 {
    let equality = matches!(&node.properties, Some(Expr::Map(pairs)) if !pairs.is_empty());
    if equality {
        RANK_EQ
    } else if !node.labels.is_empty() {
        RANK_LABEL
    } else {
        RANK_BARE
    }
}

/// The bound check as a membership predicate, not a concrete collection:
/// the executor's `cheapest_start` runs once per (pattern, row) and must
/// not clone the row's keys into a `HashSet` per call (LABS-351, stellar
/// F-3-1).
pub type Bound<'a> = &'a dyn Fn(&str) -> bool;

pub fn endpoint_estimate<'a>(
    node: &NodePattern,
    bound: Bound<'_>,
    where_conjuncts: impl IntoIterator<Item = &'a Expr>,
    stats: &dyn PatternStats,
) -> Estimate {
    if endpoint_bound(node, bound) {
        return Estimate::Measured(1);
    }
    let measured = |hits: Option<u64>| hits.map(u128::from).map(Estimate::Measured);
    measured(equality_hits(node, stats))
        .or_else(|| measured(contains_hits(node, where_conjuncts, stats)))
        .or_else(|| measured(label_hits(node, stats)))
        .unwrap_or_else(|| Estimate::Rank(unmeasured_rows(node)))
}

fn equality_hits(node: &NodePattern, stats: &dyn PatternStats) -> Option<u64> {
    let Expr::Map(pairs) = node.properties.as_ref()? else {
        return None;
    };
    pairs
        .iter()
        .flat_map(|(key, value)| node.labels.iter().map(move |label| (label, key, value)))
        .filter_map(|(label, key, value)| stats.equality_rows(label, key, value))
        .min()
}

fn contains_hits<'a>(
    node: &NodePattern,
    where_conjuncts: impl IntoIterator<Item = &'a Expr>,
    stats: &dyn PatternStats,
) -> Option<u64> {
    let var = node.variable.as_deref()?;
    where_conjuncts
        .into_iter()
        .filter_map(|expr| contains_prop(expr, var))
        .flat_map(|(prop, needle)| node.labels.iter().map(move |label| (label, prop, needle)))
        .filter_map(|(label, prop, needle)| stats.contains_rows(label, prop, needle))
        .min()
}

fn label_hits(node: &NodePattern, stats: &dyn PatternStats) -> Option<u64> {
    node.labels
        .iter()
        .filter_map(|label| stats.label_rows(label))
        .min()
}

fn each_conjunct<'a>(expr: &'a Expr, visit: &mut impl FnMut(&'a Expr)) {
    if let Expr::Binary {
        op: BinaryOp::And,
        left,
        right,
    } = expr
    {
        each_conjunct(left, visit);
        each_conjunct(right, visit);
    } else {
        visit(expr);
    }
}

fn contains_prop<'a>(expr: &'a Expr, var: &str) -> Option<(&'a str, &'a Expr)> {
    let Expr::Binary {
        op: BinaryOp::Contains,
        left,
        right,
    } = expr
    else {
        return None;
    };
    // CONTAINS is operand-asymmetric (needle/haystack), so only the left
    // side is recognized — a right-side property declines like every other
    // unservable shape, matching string_seek_expr's one-sided policy; the
    // estimate falls back to the unfiltered count.
    key_of(left, var).map(|key| (key, right.as_ref()))
}

/// Named paths, named variable-length relationships, and GPM paths stay
/// in written order (TCK Match9). A GPM path's `nodes`/`rels` may be a
/// cleared or duplicated view of the `elements` the GPM engine walks, so
/// an orientation flip could desync the two views — it declines to
/// reorder and only the cost ladder's bare rank prices it.
fn orientation_locked(path: &PathPattern) -> bool {
    path.variable.is_some()
        || path.needs_gpm()
        || path.nodes.len() < 2
        || path
            .rels
            .iter()
            .any(|rel| rel.var_length && rel.variable.is_some())
}

/// The reversal's evaluation-order gate — the planner-side twin of the
/// executor's `pattern_self_contained` (ADR-0350): the forward walk binds a
/// pattern's elements left to right, so an element's predicate may legally
/// reference another element's variable and see it bound. Reversing the
/// path evaluates those predicates in the opposite order, so the reference
/// would see an unbound variable — the strict evaluator rejects that
/// (ADR-0335) instead of fabricating null. A path whose element predicates
/// reference a peer pattern variable therefore keeps its written
/// orientation; a variable the surrounding scope already binds is safe in
/// every order.
fn reversal_safe(path: &PathPattern, bound: Bound<'_>) -> bool {
    let own = |var: &String| {
        path.nodes.iter().any(|n| n.variable.as_ref() == Some(var))
            || path.rels.iter().any(|r| r.variable.as_ref() == Some(var))
    };
    let mut refs = HashSet::new();
    for node in &path.nodes {
        if let Some(e) = &node.properties {
            cypherlite_ir::expr_vars(e, &mut refs);
        }
        if let Some(e) = &node.where_clause {
            cypherlite_ir::expr_vars(e, &mut refs);
        }
        if let Some(le) = &node.label_expr {
            cypherlite_ir::label_expr_vars(le, &mut refs);
        }
    }
    for rel in &path.rels {
        if let Some(e) = &rel.properties {
            cypherlite_ir::expr_vars(e, &mut refs);
        }
        if let Some(e) = &rel.where_clause {
            cypherlite_ir::expr_vars(e, &mut refs);
        }
    }
    !refs.iter().any(|var| own(var) && !bound(var.as_str()))
}

/// `(cost, seed)`: the degree-scaled walk cost and the start endpoint's
/// own estimate it was seeded from — the tie-break needs the seed, so it
/// is returned rather than recomputed (stellar F-2-6).
fn direction_cost(
    path: &PathPattern,
    reversed: bool,
    bound: Bound<'_>,
    where_conjuncts: &[&Expr],
    stats: &dyn PatternStats,
) -> (u128, u128) {
    if path.nodes.is_empty() {
        return (RANK_BARE, RANK_BARE);
    }
    let (nodes, rels) = oriented(path, reversed);
    match endpoint_estimate(nodes[0], bound, where_conjuncts.iter().copied(), stats) {
        Estimate::Rank(rows) => (rows, rows),
        Estimate::Measured(mut rows) => {
            let seed = rows;
            for (node, rel) in nodes.iter().zip(rels.iter()) {
                rows = apply_degree(rows, node, rel, seed_on_rel_start(rel, reversed), stats);
            }
            (rows, seed)
        }
    }
}

fn oriented(path: &PathPattern, reversed: bool) -> (Vec<&NodePattern>, Vec<&RelPattern>) {
    if reversed {
        (
            path.nodes.iter().rev().collect(),
            path.rels.iter().rev().collect(),
        )
    } else {
        (path.nodes.iter().collect(), path.rels.iter().collect())
    }
}

/// Which side of `rel` a walk's seed node rides (LABS-426): the typed
/// derivation every walk-orientation consumer shares — a written-outgoing
/// rel seeded from its start side walked forward, a written-incoming one
/// seeded from its start side walked reversed, and an UNDIRECTED rel has
/// no start side in either orientation (the mean stays; the histogram is
/// keyed by the start side only). [`reverse_path`] flips directions, so a
/// post-reversal spelling is its own written direction.
pub fn seed_on_rel_start(rel: &RelPattern, reversed: bool) -> bool {
    match rel.direction {
        cypherlite_ir::Direction::Outgoing => !reversed,
        cypherlite_ir::Direction::Incoming => reversed,
        cypherlite_ir::Direction::Both => false,
    }
}

pub fn apply_degree(
    rows: u128,
    start: &NodePattern,
    rel: &RelPattern,
    seed_is_start: bool,
    stats: &dyn PatternStats,
) -> u128 {
    let [rel_type] = rel.types.as_slice() else {
        return rows;
    };
    // LABS-426 (the mean-to-tail flip): when the hop's seed rides the
    // rel's START side ([`seed_is_start`], the caller's one derived fact),
    // the fanout rides the (seed-label, rel-type) out-degree TAIL — the
    // log2-bucket top, the conservative floor of the highest observed
    // degree — not the mean: a label with a 53-degree mean and one
    // 50k-degree supernode expands its seed rows by the supernode's
    // bucket, the one plan-cost fact the mean cannot express. The min
    // across the seed's labels — every label's tail bounds the
    // intersection's max degree (the A:B node's fanout is inside both the
    // (A,type) and the (B,type) histograms), so the tighter conservative
    // bound is the min, the same discipline [`label_hits`] uses. Any
    // other hop shape — undirected ([`seed_on_rel_start`] answers false
    // in both orientations: no start side), or the seed on the rel's end
    // side — keeps the mean, as does a seed with no histogram (the null
    // stats, an unlabeled start).
    if seed_is_start {
        if let Some(tail) = start
            .labels
            .iter()
            .filter_map(|label| stats.degree_tail(label, rel_type))
            .min()
        {
            // The clamp: the seed subset's degree SUM cannot exceed the
            // rel type's total — a whole-label scan (rows near the label
            // population) is priced by that total, the mean's own scale,
            // not rows x the one supernode's bucket (the sandbox's
            // 1000 x 32768 = 32.7M priced a 53k-fanout scan and flipped
            // the seed to the 100k-DbHits side). A small-seed seek rides
            // the tail alone: one row matched the label's supernode is
            // the case the mean cannot price.
            let scaled = rows.saturating_mul(u128::from(tail));
            return match stats.rel_type_rows(rel_type) {
                Some(total) => scaled.min(u128::from(total)),
                None => scaled,
            };
        }
    }
    let Some(rel_rows) = stats.rel_type_rows(rel_type) else {
        return rows;
    };
    let Some(label_rows) = label_hits(start, stats) else {
        return rows;
    };
    if label_rows == 0 {
        return rows;
    }
    let scaled = rows.saturating_mul(u128::from(rel_rows)) / u128::from(label_rows);
    if rows > 0 && rel_rows > 0 && scaled == 0 {
        1
    } else {
        scaled
    }
}

fn endpoint_bound(node: &NodePattern, bound: Bound<'_>) -> bool {
    node.variable
        .as_ref()
        .is_some_and(|var| bound(var.as_str()))
}

pub fn seed_rows(estimate: Estimate) -> u128 {
    match estimate {
        Estimate::Measured(rows) | Estimate::Rank(rows) => rows,
    }
}

/// `(reversed, cost)`. A strictly cheaper expanded cost wins. When both
/// directions estimate the same output (a typed hop scaled by its degree),
/// the smaller seed endpoint wins (ADR-0218): that is the scan, and the
/// expanded row count hides it. Equal seeds keep the written direction.
fn choose_orientation(
    path: &PathPattern,
    bound: Bound<'_>,
    where_clause: Option<&Expr>,
    stats: &dyn PatternStats,
) -> (bool, u128) {
    // Derived once per call: every direction cost and the seed tie-break
    // read the same conjunct slice.
    let where_conjuncts = where_conjuncts_of(where_clause);
    if orientation_locked(path) || path.nodes.is_empty() {
        return (
            false,
            direction_cost(path, false, bound, &where_conjuncts, stats).0,
        );
    }
    // The written orientation is the one the elements' left-to-right
    // binding order is defined on; a peer-referenced predicate keeps it.
    if !reversal_safe(path, bound) {
        return (
            false,
            direction_cost(path, false, bound, &where_conjuncts, stats).0,
        );
    }
    let first_bound = endpoint_bound(&path.nodes[0], bound);
    let last_bound = endpoint_bound(path.nodes.last().unwrap(), bound);
    if first_bound && !last_bound {
        return (
            false,
            direction_cost(path, false, bound, &where_conjuncts, stats).0,
        );
    }
    if last_bound && !first_bound {
        return (
            true,
            direction_cost(path, true, bound, &where_conjuncts, stats).0,
        );
    }
    let (written, written_seed) = direction_cost(path, false, bound, &where_conjuncts, stats);
    let (flipped, flipped_seed) = direction_cost(path, true, bound, &where_conjuncts, stats);
    if flipped < written {
        return (true, flipped);
    }
    if written < flipped {
        return (false, written);
    }
    if flipped_seed < written_seed {
        (true, flipped)
    } else {
        (false, written)
    }
}

fn pattern_touches(path: &PathPattern, bound: Bound<'_>) -> bool {
    cypherlite_ir::path_pattern_vars(path).any(|var| bound(var.as_str()))
}

fn reverse_path(path: &mut PathPattern) {
    path.nodes.reverse();
    path.rels.reverse();
    for rel in &mut path.rels {
        rel.direction = rel.direction.flip();
    }
    if path.elements.len() == path.nodes.len() + path.rels.len() {
        path.elements.reverse();
        for element in &mut path.elements {
            if let PatternElement::Relationship(rel) = element {
                rel.direction = rel.direction.flip();
            }
        }
    }
}

/// Greedy seed plus connected expansion: the cheapest pattern seeds, then
/// each next pattern that shares a variable already bound (LABS-157), or
/// the cheapest remaining one when none is connected. Orientation and the
/// bound set advance with every chosen pattern; the grown bound set is
/// returned. Equal costs keep written order.
pub(crate) fn order_patterns(
    patterns: &mut Vec<PathPattern>,
    prebound: &HashSet<String>,
    where_clause: Option<&Expr>,
    stats: &dyn PatternStats,
) -> HashSet<String> {
    let mut bound = prebound.clone();
    if patterns.is_empty() {
        return bound;
    }
    let mut pending: Vec<(usize, PathPattern)> =
        std::mem::take(patterns).into_iter().enumerate().collect();
    let mut chosen = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        // The bound set stays the loop's mutable state; the ladder reads
        // it through the membership predicate (one view per iteration,
        // re-created after each chosen pattern extends the set).
        let bound_view = |name: &str| bound.contains(name);
        let candidates: Vec<usize> = if chosen.is_empty() {
            (0..pending.len()).collect()
        } else {
            let connected: Vec<usize> = (0..pending.len())
                .filter(|index| pattern_touches(&pending[*index].1, &bound_view))
                .collect();
            if connected.is_empty() {
                (0..pending.len()).collect()
            } else {
                connected
            }
        };
        let (index, reversed, _, _) = candidates
            .into_iter()
            .map(|index| {
                let (reversed, cost) =
                    choose_orientation(&pending[index].1, &bound_view, where_clause, stats);
                (index, reversed, cost, pending[index].0)
            })
            .min_by_key(|&(_, _, cost, written)| (cost, written))
            .expect("candidate set is non-empty");
        let (_, mut path) = pending.remove(index);
        if reversed {
            reverse_path(&mut path);
        }
        add_pattern_vars(&mut bound, &path);
        chosen.push(path);
    }
    *patterns = chosen;
    bound
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use cypherlite_ir::Clause;

    use super::*;
    use crate::plan_with_stats;
    use crate::stats::NoPatternStats;

    struct Stub {
        labels: &'static [(&'static str, u64)],
        equals: &'static [(&'static str, &'static str, &'static str, u64)],
        contains: &'static [(&'static str, &'static str, &'static str, u64)],
        rels: &'static [(&'static str, u64)],
    }

    impl PatternStats for Stub {
        fn label_rows(&self, label: &str) -> Option<u64> {
            self.labels
                .iter()
                .find(|(name, _)| *name == label)
                .map(|(_, rows)| *rows)
        }

        fn equality_rows(&self, label: &str, property: &str, value: &Expr) -> Option<u64> {
            let text = match value {
                Expr::String(text) => text.as_str(),
                _ => return None,
            };
            self.equals
                .iter()
                .find(|(l, p, v, _)| *l == label && *p == property && *v == text)
                .map(|(_, _, _, rows)| *rows)
        }

        fn contains_rows(&self, label: &str, property: &str, needle: &Expr) -> Option<u64> {
            let text = match needle {
                Expr::String(text) => text.as_str(),
                _ => return None,
            };
            self.contains
                .iter()
                .find(|(l, p, n, _)| *l == label && *p == property && *n == text)
                .map(|(_, _, _, rows)| *rows)
        }

        fn rel_type_rows(&self, rel_type: &str) -> Option<u64> {
            self.rels
                .iter()
                .find(|(name, _)| *name == rel_type)
                .map(|(_, rows)| *rows)
        }
    }

    fn patterns_of(cypher: &str, stats: &dyn PatternStats) -> Vec<PathPattern> {
        let query = cypherlite_front::parse_and_lower(cypher).unwrap();
        let plan = plan_with_stats(query, &mut |_| false, stats).unwrap();
        let Clause::Match { patterns, .. } = &plan.parts[0][0] else {
            panic!("expected a leading MATCH");
        };
        patterns.clone()
    }

    /// LABS-426 (the mean-to-tail flip): the tail-carrying stats — one
    /// (label, rel-type) tail beside the mean's inputs, so the flip's
    /// own arithmetic is pinned without the executor's graphs.
    struct TailStub {
        labels: &'static [(&'static str, u64)],
        rels: &'static [(&'static str, u64)],
        tails: &'static [(&'static str, &'static str, u64)],
    }

    impl PatternStats for TailStub {
        fn label_rows(&self, label: &str) -> Option<u64> {
            self.labels
                .iter()
                .find(|(name, _)| *name == label)
                .map(|(_, rows)| *rows)
        }

        fn equality_rows(&self, _: &str, _: &str, _: &Expr) -> Option<u64> {
            None
        }

        fn contains_rows(&self, _: &str, _: &str, _: &Expr) -> Option<u64> {
            None
        }

        fn rel_type_rows(&self, rel_type: &str) -> Option<u64> {
            self.rels
                .iter()
                .find(|(name, _)| *name == rel_type)
                .map(|(_, rows)| *rows)
        }

        fn degree_tail(&self, label: &str, rel_type: &str) -> Option<u64> {
            self.tails
                .iter()
                .find(|(l, t, _)| *l == label && *t == rel_type)
                .map(|(_, _, tail)| *tail)
        }
    }

    /// The pattern's written first hop — parse only, no planning: the
    /// written spelling is what both walk orientations evaluate against
    /// (the reversed evaluation walks it from the end side), so the
    /// parity pins need the un-reversed, un-flipped form.
    fn first_hop(cypher: &str) -> (NodePattern, RelPattern) {
        let query = cypherlite_front::parse_and_lower(cypher).unwrap();
        let Clause::Match { patterns, .. } = &query.parts[0].clauses[0] else {
            panic!("expected a leading MATCH");
        };
        let path = &patterns[0];
        (path.nodes[0].clone(), path.rels[0].clone())
    }

    /// LABS-426: an outgoing hop's fanout rides the (start-label,
    /// rel-type) out-degree tail, not the mean — the label's mean (53)
    /// hides the one 50k-degree supernode; the tail (32768, the log2
    /// bucket top) does not. An incoming or undirected hop keeps the mean
    /// (the histogram's key side is the far side), and a multi-label
    /// start takes the min tail across its labels (every label's tail
    /// bounds the intersection's max degree).
    #[test]
    fn apply_degree_rides_the_tail_on_an_outgoing_hop() {
        let stats = TailStub {
            labels: &[("P", 1000), ("A", 1000), ("B", 1000)],
            rels: &[("KNOWS", 53000)],
            tails: &[("P", "KNOWS", 32768), ("A", "KNOWS", 16384)],
        };
        // Outgoing, a small-seed seek: rows x tail, clamped by the type
        // total — 1 x 32768 (the mean would price the supernode at 53).
        let (start, rel) = first_hop("MATCH (p:P)-[:KNOWS]->(q:Q) RETURN q");
        assert!(seed_on_rel_start(&rel, false));
        assert_eq!(apply_degree(1, &start, &rel, true, &stats), 32_768);
        // Outgoing, a whole-label scan: the clamp holds the sum at the
        // type total — 53000, the mean's own scale, not 1000 x 32768.
        assert_eq!(apply_degree(1000, &start, &rel, true, &stats), 53_000);
        // Incoming: the mean — the histogram's key side is the far side.
        let (start, rel) = first_hop("MATCH (p:P)<-[:KNOWS]-(q:Q) RETURN q");
        assert!(!seed_on_rel_start(&rel, false));
        assert_eq!(apply_degree(1000, &start, &rel, false, &stats), 53_000);
        // Undirected: no start side in EITHER orientation (round 1,
        // F-1-1: the XOR coincidence answered true for reversed) — the
        // mean in both walks.
        let (start, rel) = first_hop("MATCH (p:P)-[:KNOWS]-(q:Q) RETURN q");
        assert!(!seed_on_rel_start(&rel, false));
        assert!(!seed_on_rel_start(&rel, true));
        assert_eq!(apply_degree(1000, &start, &rel, false, &stats), 53_000);
        assert_eq!(apply_degree(1000, &start, &rel, true, &stats), 53_000);
        // A multi-label start takes the min tail across its labels
        // (a small seed: 1 x 16384, clamped by the 53000 total).
        let (start, rel) = first_hop("MATCH (p:A:B)-[:KNOWS]->(q:Q) RETURN q");
        assert_eq!(apply_degree(1, &start, &rel, true, &stats), 16_384);
        // A label with no histogram entry keeps the mean (B alone: no
        // (B,KNOWS) tail).
        let (start, rel) = first_hop("MATCH (p:B)-[:KNOWS]->(q:Q) RETURN q");
        assert_eq!(apply_degree(1000, &start, &rel, true, &stats), 53_000);
        // The seed on the rel's END side (the reversed walk of a
        // written-outgoing hop): the histogram's key side is the far
        // side, so the mean stays even when the seed's label carries a
        // tail.
        let (start, rel) = first_hop("MATCH (p:P)-[:KNOWS]->(q:Q) RETURN q");
        assert!(!seed_on_rel_start(&rel, true));
        assert_eq!(apply_degree(1000, &start, &rel, false, &stats), 53_000);
        // The reversed walk of a written-INCOMING hop: the seed IS the
        // rel's start side — the tail rides again (a small seed).
        let (start, rel) = first_hop("MATCH (p:P)<-[:KNOWS]-(q:Q) RETURN q");
        assert!(seed_on_rel_start(&rel, true));
        assert_eq!(apply_degree(1, &start, &rel, true, &stats), 32_768);
    }

    fn starts_with_label(path: &PathPattern, label: &str) -> bool {
        path.nodes
            .first()
            .is_some_and(|node| node.labels.iter().any(|got| got == label))
    }

    #[test]
    fn no_stats_reorders_q22_and_q27_and_keeps_equal_scores() {
        let stats = Stub {
            labels: &[],
            equals: &[],
            contains: &[],
            rels: &[],
        };
        let q22 = patterns_of(
            "MATCH (p:Person)-[:workAt]->(o:Organisation), \
             (c:Comment)-[:replyOfPost]->(post:Post), \
             (c)-[:commentHasCreator]->(p) \
             WHERE o.name = 'Linxair' RETURN p",
            &stats,
        );
        assert!(starts_with_label(&q22[0], "Organisation"));
        assert_eq!(q22[1].nodes[0].variable.as_deref(), Some("p"));
        assert_eq!(q22[2].nodes[0].variable.as_deref(), Some("c"));

        let q27 = patterns_of(
            "MATCH (c:Comment)-[:commentHasCreator]->(p:Person), \
             (p)-[:personIsLocatedIn]->(l:Place), \
             (c)-[:commentHasTag]->(t:Tag) \
             WHERE l.name = 'Toronto' AND t.name = 'Winston_Churchill' \
             RETURN c",
            &stats,
        );
        assert!(starts_with_label(&q27[0], "Place"));
        assert_eq!(q27[1].nodes[0].variable.as_deref(), Some("p"));
        assert_eq!(q27[2].nodes[0].variable.as_deref(), Some("c"));

        let tied = patterns_of(
            "MATCH (a:A)-[:R]->(b:B), (c:C)-[:S]->(d:D) RETURN a",
            &stats,
        );
        assert_eq!(tied[0].nodes[0].variable.as_deref(), Some("a"));
        assert_eq!(tied[1].nodes[0].variable.as_deref(), Some("c"));
        assert_eq!(
            tied[0].rels[0].direction,
            cypherlite_ir::Direction::Outgoing
        );
    }

    /// The reversal's evaluation-order gate: a pattern whose element
    /// predicate references a peer pattern variable keeps its written
    /// orientation — the reference is legal in that order, and the strict
    /// evaluator rejects it once the walk order is reversed (ADR-0335).
    /// Without such a reference the bound-endpoint reversal still applies.
    #[test]
    fn peer_referenced_predicate_keeps_the_written_orientation() {
        let stats = Stub {
            labels: &[],
            equals: &[],
            contains: &[],
            rels: &[],
        };
        let referencing = patterns_of(
            "MATCH (q:Question)<-[:COMMENTED_ON]-(first:Comment), \
             (u2:User)-[:COMMENTED]->(c2:Comment {seen: u2.name})-[:COMMENTED_ON]->(q) \
             RETURN u2.name AS name",
            &stats,
        );
        assert_eq!(referencing[1].nodes[0].variable.as_deref(), Some("u2"));
        assert_eq!(
            referencing[1].nodes[1].variable.as_deref(),
            Some("c2"),
            "the written order binds u2 before the referencing predicate"
        );

        let plain = patterns_of(
            "MATCH (q:Question)<-[:COMMENTED_ON]-(first:Comment), \
             (u2:User)-[:COMMENTED]->(c2:Comment {seen: 'x'})-[:COMMENTED_ON]->(q) \
             RETURN u2.name AS name",
            &stats,
        );
        assert_eq!(
            plain[1].nodes[0].variable.as_deref(),
            Some("q"),
            "the bound-endpoint reversal still applies without a peer reference"
        );
    }

    #[test]
    fn label_and_index_counts_seed_q19_and_q28() {
        let stats = Stub {
            labels: &[
                ("Comment", 2_000_000),
                ("Tag", 16_000),
                ("Place", 1_460),
                ("Tagclass", 71),
                ("Person", 9_892),
            ],
            equals: &[
                ("Place", "name", "Manila", 1),
                ("Tagclass", "name", "BritishRoyalty", 1),
            ],
            contains: &[("Tag", "name", "Copernicus", 4)],
            rels: &[
                ("commentHasTag", 2_000_000),
                ("commentIsLocatedIn", 2_000_000),
                ("personIsLocatedIn", 9_892),
                ("hasType", 16_080),
                ("hasInterest", 229_166),
            ],
        };
        let q19 = patterns_of(
            "MATCH (c:Comment)-[:commentHasTag]->(t:Tag), \
             (c)-[:commentIsLocatedIn]->(l:Place) \
             WHERE t.name CONTAINS 'Copernicus' RETURN l",
            &stats,
        );
        assert!(starts_with_label(&q19[0], "Tag"));
        assert_eq!(q19[1].nodes[0].variable.as_deref(), Some("c"));

        let q28 = patterns_of(
            "MATCH (t:Tag)-[:hasType]->(tc:Tagclass), \
             (p:Person)-[:hasInterest]->(t), \
             (p)-[:personIsLocatedIn]->(l:Place) \
             WHERE tc.name = 'BritishRoyalty' AND l.name = 'Manila' RETURN p",
            &stats,
        );
        assert!(starts_with_label(&q28[0], "Place"));
        assert!(q28[1].rels[0].types.iter().any(|ty| ty == "hasInterest"));
        assert!(q28[2].rels[0].types.iter().any(|ty| ty == "hasType"));
    }

    #[test]
    fn named_variable_length_stays_written() {
        let stats = Stub {
            labels: &[("B", 1)],
            equals: &[],
            contains: &[],
            rels: &[],
        };
        let paths = patterns_of("MATCH (a)-[r*]->(b:B) RETURN a", &stats);
        assert_eq!(paths[0].nodes[0].variable.as_deref(), Some("a"));
        assert_eq!(
            paths[0].rels[0].direction,
            cypherlite_ir::Direction::Outgoing
        );
    }

    struct SawParameter(Cell<bool>);

    impl PatternStats for SawParameter {
        fn label_rows(&self, _: &str) -> Option<u64> {
            None
        }
        fn equality_rows(&self, label: &str, property: &str, value: &Expr) -> Option<u64> {
            if label == "Place"
                && property == "name"
                && matches!(value, Expr::Parameter(name) if name == "city")
            {
                self.0.set(true);
                return Some(1);
            }
            None
        }
        fn contains_rows(&self, _: &str, _: &str, _: &Expr) -> Option<u64> {
            None
        }
        fn rel_type_rows(&self, _: &str) -> Option<u64> {
            None
        }
    }

    #[test]
    fn equality_parameters_reach_stats() {
        let stats = SawParameter(Cell::new(false));
        let _ = patterns_of("MATCH (l:Place) WHERE l.name = $city RETURN l", &stats);
        assert!(stats.0.get());
    }

    fn label_stats(labels: &'static [(&'static str, u64)]) -> Stub {
        Stub {
            labels,
            equals: &[],
            contains: &[],
            rels: &[],
        }
    }

    fn first_label(path: &PathPattern) -> &str {
        path.nodes[0].labels[0].as_str()
    }

    // ADR-0218: with no bound node and equal coarse ranks, the part orients
    // toward the end whose label is rarer.
    #[test]
    fn unbound_part_reverses_toward_the_rare_label() {
        let stats = label_stats(&[("Hub", 200_000), ("Target", 10)]);
        let patterns = patterns_of("MATCH (h:Hub)-[:R]->(t:Target) RETURN h", &stats);
        assert_eq!(first_label(&patterns[0]), "Target");
        assert_eq!(patterns[0].nodes.last().unwrap().labels[0].as_str(), "Hub");
        assert_eq!(
            patterns[0].rels[0].direction,
            cypherlite_ir::Direction::Incoming
        );
    }

    // A typed hop scales both directions to the same edge count. The seed
    // is still the rarer label: that is the scan ADR-0218 exists to avoid.
    #[test]
    fn degree_tie_still_seeds_the_rare_label() {
        let stats = Stub {
            labels: &[("Hub", 200_000), ("Target", 10)],
            equals: &[],
            contains: &[],
            rels: &[("R", 100)],
        };
        let patterns = patterns_of("MATCH (h:Hub)-[:R]->(t:Target) RETURN h", &stats);
        assert_eq!(first_label(&patterns[0]), "Target");
    }

    #[test]
    fn selective_written_first_end_stays_first() {
        let stats = label_stats(&[("Hub", 200_000), ("Target", 10)]);
        let patterns = patterns_of("MATCH (t:Target)<-[:R]-(h:Hub) RETURN t", &stats);
        assert_eq!(first_label(&patterns[0]), "Target");
    }

    #[test]
    fn equal_cardinalities_keep_written_order() {
        let stats = label_stats(&[("A", 5), ("B", 5)]);
        let patterns = patterns_of("MATCH (a:A)-[:R]->(b:B) RETURN a", &stats);
        assert_eq!(first_label(&patterns[0]), "A");
    }

    #[test]
    fn no_stats_keeps_written_order() {
        let patterns = patterns_of("MATCH (h:Hub)-[:R]->(t:Target) RETURN h", &NoPatternStats);
        assert_eq!(first_label(&patterns[0]), "Hub");
    }

    #[test]
    fn no_stats_keeps_written_part_order() {
        let patterns = patterns_of(
            "MATCH (h:Hub)-[:R]->(t:Target), (x:Tiny)-[:S]->(y:Tiny) RETURN x",
            &NoPatternStats,
        );
        assert_eq!(first_label(&patterns[0]), "Hub");
        assert_eq!(first_label(&patterns[1]), "Tiny");
    }

    #[test]
    fn bound_node_still_wins_over_cardinality() {
        let stats = label_stats(&[("Hub", 200_000), ("Target", 10)]);
        let query = cypherlite_front::parse_and_lower(
            "MATCH (h:Hub) WITH h MATCH (h)-[:R]->(t:Target) RETURN t",
        )
        .unwrap();
        let plan = plan_with_stats(query, &mut |_| false, &stats).unwrap();
        let Clause::Match { patterns, .. } = &plan.parts[0][2] else {
            panic!("expected the second MATCH");
        };
        assert_eq!(patterns[0].nodes[0].variable.as_deref(), Some("h"));
    }

    #[test]
    fn cheaper_independent_part_runs_first() {
        let stats = label_stats(&[("Hub", 200_000), ("Target", 10), ("Tiny", 3)]);
        let patterns = patterns_of(
            "MATCH (h:Hub)-[:R]->(t:Target), (x:Tiny)-[:S]->(y:Tiny) RETURN x",
            &stats,
        );
        assert_eq!(first_label(&patterns[0]), "Tiny");
    }
}
