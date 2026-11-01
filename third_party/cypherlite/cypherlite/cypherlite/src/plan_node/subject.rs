//! The MATCH subject (ADR-0344 phase 3): the operator's own object —
//! the pattern text shown in the `Details` column, and the cardinality
//! estimate ADR-0218 `PatternStats` can give it honestly.
//!
//! The pattern text is the static subject (variables, labels, rel types,
//! quantifier bounds); property and `WHERE` predicates are filters, not
//! the subject, so they stay out of the text (Neo4j renders them as
//! separate operators too). Label and type expressions beyond the
//! static conjunction/disjunction are not statically printable; the
//! retained static `labels`/`types` lists are what render.

use std::borrow::Cow;

use cypherlite_ir::{
    Direction, Expr, LabelExpr, NodePattern, PathPattern, PatternElement, RelPattern,
};
use cypherlite_planner::PatternStats;

use crate::stats::PlanStats;

/// The MATCH subject: the operator a pattern set lowers to and its
/// details text. A lone node with no relationships is a scan (it names
/// its labels, or scans all nodes when it has none); anything with a
/// relationship is an `Expand` over the rendered pattern texts. An
/// `OPTIONAL` MATCH wraps the operator in `Optional(…)` — derived from
/// the one operator fact here, never re-spelled by the caller.
pub(super) fn match_subject(
    patterns: &[PathPattern],
    optional: bool,
) -> (Cow<'static, str>, String) {
    let operator = match lone_node_of(patterns) {
        Some(node) if is_all_nodes_scan(node) => "AllNodesScan",
        Some(_) => "NodeByLabelScan",
        None => "Expand",
    };
    let operator = if optional {
        Cow::Owned(format!("Optional({operator})"))
    } else {
        Cow::Borrowed(operator)
    };
    (operator, pattern_texts(patterns))
}

/// Whether a lone node pattern scans every node — no labels, however they
/// were spelled (`labels` retains the static conjunction; `label_expr` the
/// non-static spelling). The one derivation the operator naming and the
/// `AllNodesScan` warning detection share.
pub(super) fn is_all_nodes_scan(node: &NodePattern) -> bool {
    node.labels.is_empty() && node.label_expr.is_none()
}

/// A pattern set's lone node, when the set is a single pattern that is a
/// single node with no relationships — the scan shape. The one MATCH-shape
/// decomposition every read of the shape shares (the operator naming, the
/// estimate, the scan warning), never a second `[single]` + `lone_node`
/// spelling at a caller.
pub(super) fn lone_node_of(patterns: &[PathPattern]) -> Option<&NodePattern> {
    let [single] = patterns else {
        return None;
    };
    lone_node(single)
}

/// The node's one static label, when it has exactly one and no dynamic label
/// expression beyond a static conjunction — the only shape a single
/// `(label, property)` index can serve. The one derivation the estimate and
/// the `MissingIndex` notification share.
pub(super) fn single_static_label(node: &NodePattern) -> Option<&str> {
    let [label] = node.labels.as_slice() else {
        return None;
    };
    if node
        .label_expr
        .as_ref()
        .is_some_and(|expr| !expr.is_static_conjunction())
    {
        return None;
    }
    Some(label)
}

/// Every pattern's text, statement order, comma-joined — the subject of a
/// multi-pattern MATCH or a CREATE.
pub(super) fn pattern_texts(patterns: &[PathPattern]) -> String {
    patterns
        .iter()
        .map(pattern_text)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The honest estimate for an equality-predicated node pattern: the
/// minimum `equality_rows` over the property map's pairs (the tightest
/// bound, mirroring the planner's `equality_hits` policy). `None` when
/// the stats cannot speak for any pair.
fn equality_estimate(label: &str, props: &Expr, stats: &dyn PatternStats) -> Option<u64> {
    let Expr::Map(pairs) = props else {
        return None;
    };
    pairs
        .iter()
        .filter_map(|(key, value)| stats.equality_rows(label, key, value))
        .min()
}

/// The cardinality estimate for a MATCH's subject from ADR-0218
/// `PatternStats` — only what the stats offer honestly: a single-label
/// scan's label row count — an equality-predicated scan folds to the
/// indexed hit count, else the whole label (the scan-then-filter reality),
/// a single typed relationship's type row count when both endpoints are
/// unfiltered. `None` everywhere else (a multi-pattern cartesian, a
/// multi-label node, a quantified path — no signal covers those).
pub(super) fn match_estimate(patterns: &[PathPattern], stats: &dyn PlanStats) -> Option<u64> {
    // One decomposition per function: the single pattern destructured
    // once at the top, the scan shape read off it through `lone_node`
    // (the shape half of `lone_node_of`, whose other callers hold no
    // other need for the single pattern).
    let [single] = patterns else {
        return None;
    };
    if let Some(node) = lone_node(single) {
        // One static label (the `labels` list is its retained conjunction,
        // however the label expression spelled it): the stats count it.
        // A conjunction of several labels has no count of its own.
        if let Some(label) = single_static_label(node) {
            // An equality-predicated property with an index serving it
            // folds to the hit count (~1 for a point lookup). Without
            // one, the fused scan-then-filter operator really scans the
            // whole label before filtering — the label count IS the
            // honest estimate, and the est-vs-actual ⚠ it fires is the
            // missing-index diagnosis (the LABS-328 class), pairing with
            // the scan warnings.
            if let Some(props) = node.properties.as_ref() {
                return equality_estimate(label, props, stats).or_else(|| stats.label_rows(label));
            }
            return stats.label_rows(label);
        }
        return None;
    }
    // A single relationship between two unfiltered endpoints: the
    // relationship type's row count is the pattern's row count.
    if single.where_clause.is_some() {
        return None;
    }
    let [start, end] = &single.nodes[..] else {
        return None;
    };
    let [rel] = &single.rels[..] else {
        return None;
    };
    if !node_is_unfiltered(start)
        || !node_is_unfiltered(end)
        || rel.var_length
        || rel.quantified
        || rel.properties.is_some()
        || rel.where_clause.is_some()
        || rel.types.len() != 1
        || !rel
            .type_expr
            .as_ref()
            .is_none_or(LabelExpr::is_static_disjunction)
    {
        return None;
    }
    stats.rel_type_rows(&rel.types[0])
}

/// A pattern's lone node, when the pattern is a single node with no
/// relationships — the scan shape.
pub(super) fn lone_node(pattern: &PathPattern) -> Option<&NodePattern> {
    if pattern.rels.is_empty() && pattern.nodes.len() == 1 {
        Some(&pattern.nodes[0])
    } else {
        None
    }
}

/// A node endpoint the stats say nothing about (no labels, no
/// properties, no predicates) — its count cannot narrow the estimate.
fn node_is_unfiltered(node: &NodePattern) -> bool {
    is_all_nodes_scan(node) && node.properties.is_none() && node.where_clause.is_none()
}

/// One pattern as text: the named-path prefix, then the elements (the
/// lossless form) or the compact node/rel interleave.
pub(super) fn pattern_text(pattern: &PathPattern) -> String {
    let mut text = String::new();
    if let Some(variable) = &pattern.variable {
        text.push_str(variable);
        text.push_str(" = ");
    }
    if !pattern.elements.is_empty() {
        for element in &pattern.elements {
            push_element(&mut text, element);
        }
    } else {
        for (index, node) in pattern.nodes.iter().enumerate() {
            push_node(&mut text, node);
            if let Some(rel) = pattern.rels.get(index) {
                push_rel(&mut text, rel);
            }
        }
    }
    text
}

/// Append one pattern element — a node, a directed relationship, or a
/// quantified group with its hop bounds.
fn push_element(text: &mut String, element: &PatternElement) {
    match element {
        PatternElement::Node(node) => push_node(text, node),
        PatternElement::Relationship(rel) => push_rel(text, rel),
        PatternElement::Quantified(quantified) => {
            text.push('(');
            for element in &quantified.elements {
                push_element(text, element);
            }
            text.push(')');
            push_quantifier(text, quantified.min, quantified.max);
        }
    }
}

/// Append one node: `(variable:Label1:Label2)`.
fn push_node(text: &mut String, node: &NodePattern) {
    text.push('(');
    if let Some(variable) = &node.variable {
        text.push_str(variable);
    }
    for label in &node.labels {
        text.push(':');
        text.push_str(label);
    }
    text.push(')');
}

/// Append one relationship with its direction: `-[v:T1|T2*1..2]->`.
fn push_rel(text: &mut String, rel: &RelPattern) {
    if rel.direction == Direction::Incoming {
        text.push_str("<-");
    } else {
        text.push('-');
    }
    text.push('[');
    if let Some(variable) = &rel.variable {
        text.push_str(variable);
    }
    for (index, rel_type) in rel.types.iter().enumerate() {
        if index > 0 {
            text.push('|');
        }
        text.push(':');
        text.push_str(rel_type);
    }
    if rel.var_length || rel.quantified {
        push_quantifier(text, rel.min_hops, rel.max_hops);
    }
    text.push(']');
    if rel.direction == Direction::Outgoing {
        text.push_str("->");
    } else {
        text.push('-');
    }
}

/// Append a hop-bounds quantifier: `*n` for a bounded exact count, else
/// `*min..max` (`max` empty when the bound is open).
fn push_quantifier(text: &mut String, min: u32, max: Option<u32>) {
    text.push('*');
    match max {
        Some(max) if max == min => text.push_str(&min.to_string()),
        Some(max) => {
            text.push_str(&min.to_string());
            text.push_str("..");
            text.push_str(&max.to_string());
        }
        None => {
            text.push_str(&min.to_string());
            text.push_str("..");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{compact, node, rel};
    use super::*;

    #[test]
    fn scan_subjects_name_their_labels() {
        let labeled = compact(vec![node(Some("n"), &["Person"])], Vec::new());
        assert_eq!(
            match_subject(&[labeled.clone()], false),
            ("NodeByLabelScan".into(), "(n:Person)".to_string())
        );
        let bare = compact(vec![node(None, &[])], Vec::new());
        assert_eq!(
            match_subject(&[bare], false),
            ("AllNodesScan".into(), "()".to_string())
        );
    }

    #[test]
    fn expand_subjects_carry_the_pattern_text_with_bounds() {
        let quantified = compact(
            vec![node(None, &["Person"]), node(None, &["Person"])],
            vec![rel(&["KNOWS"], 1, Some(2))],
        );
        assert_eq!(
            match_subject(&[quantified.clone()], false),
            (
                "Expand".into(),
                "(:Person)-[:KNOWS*1..2]->(:Person)".to_string()
            )
        );
        let fixed = compact(
            vec![node(Some("a"), &[]), node(Some("b"), &[])],
            vec![rel(&["KNOWS"], 1, None)],
        );
        assert_eq!(
            match_subject(&[fixed], false),
            ("Expand".into(), "(a)-[:KNOWS]->(b)".to_string())
        );
        // An open-bounded var-length: `*1..`.
        let mut open = rel(&["KNOWS"], 1, None);
        open.var_length = true;
        let open = compact(vec![node(Some("a"), &[]), node(Some("b"), &[])], vec![open]);
        assert_eq!(
            match_subject(&[open], false),
            ("Expand".into(), "(a)-[:KNOWS*1..]->(b)".to_string())
        );
    }

    #[test]
    fn optional_matches_wrap_their_operator_prefix() {
        let bare = compact(vec![node(None, &[])], Vec::new());
        assert_eq!(
            match_subject(&[bare], true),
            ("Optional(AllNodesScan)".into(), "()".to_string())
        );
        let labeled = compact(vec![node(Some("n"), &["Person"])], Vec::new());
        assert_eq!(
            match_subject(&[labeled], true),
            ("Optional(NodeByLabelScan)".into(), "(n:Person)".to_string())
        );
        let expand = compact(
            vec![node(Some("a"), &[]), node(Some("b"), &[])],
            vec![rel(&["KNOWS"], 1, None)],
        );
        assert_eq!(
            match_subject(&[expand], true),
            ("Optional(Expand)".into(), "(a)-[:KNOWS]->(b)".to_string())
        );
    }

    #[test]
    fn multi_pattern_matches_join_their_texts() {
        let first = compact(vec![node(Some("a"), &["A"])], Vec::new());
        let second = compact(vec![node(Some("b"), &["B"])], Vec::new());
        assert_eq!(
            match_subject(&[first, second], false),
            ("Expand".into(), "(a:A), (b:B)".to_string())
        );
    }

    #[test]
    fn named_paths_and_quantified_groups_render() {
        let mut named = compact(
            vec![node(Some("a"), &[]), node(Some("b"), &[])],
            vec![rel(&["R"], 1, None)],
        );
        named.variable = Some("p".to_string());
        assert_eq!(pattern_text(&named), "p = (a)-[:R]->(b)");
        let mut group = compact(
            vec![node(None, &[]), node(None, &[])],
            vec![rel(&["R"], 1, None)],
        );
        group.elements = vec![
            PatternElement::Node(node(None, &[])),
            PatternElement::Relationship(rel(&["R"], 1, None)),
            PatternElement::Node(node(None, &[])),
        ];
        assert_eq!(pattern_text(&group), "()-[:R]->()");
        // A quantified group renders its bounds: `((a)-[:R]->(b))*1..2`.
        let quantified = PathPattern {
            variable: None,
            elements: vec![PatternElement::Quantified(
                cypherlite_ir::QuantifiedPathPattern {
                    subpath_variable: None,
                    elements: vec![
                        PatternElement::Node(node(Some("a"), &[])),
                        PatternElement::Relationship(rel(&["R"], 1, None)),
                        PatternElement::Node(node(Some("b"), &[])),
                    ],
                    where_clause: None,
                    min: 1,
                    max: Some(2),
                },
            )],
            nodes: Vec::new(),
            rels: Vec::new(),
            where_clause: None,
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        };
        assert_eq!(pattern_text(&quantified), "((a)-[:R]->(b))*1..2");
    }

    /// Stats offering exactly one label count and one rel-type count.
    struct Counting {
        labels: Vec<(&'static str, u64)>,
        rels: Vec<(&'static str, u64)>,
    }

    impl PlanStats for Counting {
        fn property_index_texts(&self) -> Vec<String> {
            Vec::new()
        }
        fn range_indexed_properties(&self) -> Vec<crate::stats::RangeIndexedProperty> {
            Vec::new()
        }
    }

    impl PatternStats for Counting {
        fn label_rows(&self, label: &str) -> Option<u64> {
            self.labels
                .iter()
                .find(|(l, _)| *l == label)
                .map(|(_, rows)| *rows)
        }
        fn equality_rows(&self, _: &str, _: &str, _: &cypherlite_ir::Expr) -> Option<u64> {
            None
        }
        fn contains_rows(&self, _: &str, _: &str, _: &cypherlite_ir::Expr) -> Option<u64> {
            None
        }
        fn rel_type_rows(&self, rel_type: &str) -> Option<u64> {
            self.rels
                .iter()
                .find(|(t, _)| *t == rel_type)
                .map(|(_, rows)| *rows)
        }
    }

    #[test]
    fn estimates_come_from_the_stats_honestly() {
        let stats = Counting {
            labels: vec![("Person", 2)],
            rels: vec![("KNOWS", 3)],
        };
        // A single-label scan: the label's row count.
        let scan = compact(vec![node(Some("n"), &["Person"])], Vec::new());
        assert_eq!(match_estimate(&[scan], &stats), Some(2));
        // A single typed relationship between bare endpoints.
        let expand = compact(
            vec![node(None, &[]), node(None, &[])],
            vec![rel(&["KNOWS"], 1, None)],
        );
        assert_eq!(match_estimate(&[expand], &stats), Some(3));
        // A labeled endpoint narrows past the type count: no signal.
        let narrowed = compact(
            vec![node(None, &["Person"]), node(None, &[])],
            vec![rel(&["KNOWS"], 1, None)],
        );
        assert_eq!(match_estimate(&[narrowed], &stats), None);
        // A quantified path's row count is not the rel count: None.
        let quantified = compact(
            vec![node(None, &[]), node(None, &[])],
            vec![rel(&["KNOWS"], 1, Some(2))],
        );
        assert_eq!(match_estimate(&[quantified], &stats), None);
        // Two patterns: a cartesian — None.
        let pair = vec![
            compact(vec![node(Some("a"), &["Person"])], Vec::new()),
            compact(vec![node(Some("b"), &["Person"])], Vec::new()),
        ];
        assert_eq!(match_estimate(&pair, &stats), None);
        // A label the stats do not count: None, never zero.
        let unknown = compact(vec![node(None, &["City"])], Vec::new());
        assert_eq!(match_estimate(&[unknown], &stats), None);
    }
}
