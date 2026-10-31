//! The path-pattern payload walk (ADR-0363 decision 13): one post-rewrite
//! enumeration of a pattern's node, relationship and quantified-group
//! payloads, shared by the clause walk and the notification detectors.
//!
//! Split from [`super`] so the clause-tree walker and this pattern walker
//! stay separate concerns; [`super`] re-exports both items so
//! `cypherlite_ir::{walk_pattern_payloads, PatternPayload}` remains the one
//! public path.

use crate::{NodePattern, PathPattern, PatternElement, QuantifiedPathPattern, RelPattern};

/// One node, relationship or quantified-group payload of a path pattern, as
/// [`walk_pattern_payloads`] visits it.
#[derive(Clone, Copy, Debug)]
pub enum PatternPayload<'a> {
    Node(&'a NodePattern),
    Relationship(&'a RelPattern),
    Quantified(&'a QuantifiedPathPattern),
}

/// Visit every node and relationship payload of `pattern` exactly once,
/// quantified groups descended (a group is seen before the payloads nested
/// in it).
///
/// One post-rewrite precedence rules the two representations lowering
/// carries: the compact `nodes`/`rels` view is authoritative whenever it is
/// populated and the pattern does not need the GPM form, because
/// `fold_where` moves `WHERE` equalities and label predicates into that
/// view only; the lossless `elements` sequence is authoritative for
/// quantified/GPM shapes ([`PathPattern::needs_gpm`]) and for legacy IR
/// built with only that view. Walking both would double-visit every payload
/// and would read the pre-fold twins.
pub fn walk_pattern_payloads<'a>(
    pattern: &'a PathPattern,
    on_payload: &mut impl FnMut(PatternPayload<'a>),
) {
    let compact = !pattern.nodes.is_empty() || !pattern.rels.is_empty();
    if compact && (pattern.elements.is_empty() || !pattern.needs_gpm()) {
        for node in &pattern.nodes {
            on_payload(PatternPayload::Node(node));
        }
        for rel in &pattern.rels {
            on_payload(PatternPayload::Relationship(rel));
        }
    } else {
        walk_pattern_elements(&pattern.elements, on_payload);
    }
}

/// The [`PatternElement`] recursion behind [`walk_pattern_payloads`].
fn walk_pattern_elements<'a>(
    elements: &'a [PatternElement],
    on_payload: &mut impl FnMut(PatternPayload<'a>),
) {
    for element in elements {
        match element {
            PatternElement::Node(node) => on_payload(PatternPayload::Node(node)),
            PatternElement::Relationship(rel) => on_payload(PatternPayload::Relationship(rel)),
            PatternElement::Quantified(quantified) => {
                on_payload(PatternPayload::Quantified(quantified));
                walk_pattern_elements(&quantified.elements, on_payload);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::QuantifiedPathPattern;

    fn node(variable: &str, label: &str) -> NodePattern {
        NodePattern {
            variable: Some(variable.into()),
            labels: vec![label.into()],
            label_expr: None,
            properties: None,
            where_clause: None,
        }
    }

    /// The payload visitor owns the post-rewrite precedence: a non-GPM
    /// pattern reads the compact view (where `fold_where` wrote), while a
    /// quantified/GPM shape reads the lossless element sequence and
    /// descends into each group.
    #[test]
    fn walk_pattern_payloads_owns_the_post_rewrite_view_precedence() {
        let element_only = node("element", "Elements");
        let mut folded = PathPattern {
            variable: None,
            nodes: vec![node("compact", "Compact")],
            rels: Vec::new(),
            elements: vec![PatternElement::Node(element_only.clone())],
            where_clause: None,
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        };
        let mut seen = Vec::new();
        walk_pattern_payloads(&folded, &mut |payload| {
            if let PatternPayload::Node(node) = payload {
                seen.push(node.labels[0].clone());
            }
        });
        assert_eq!(
            seen,
            ["Compact"],
            "the compact view wins for a folded shape"
        );

        // A quantified element sequence forces the elements view even though
        // a compact twin is present.
        folded.elements = vec![PatternElement::Quantified(QuantifiedPathPattern {
            subpath_variable: Some("q".into()),
            elements: vec![PatternElement::Node(element_only)],
            where_clause: None,
            min: 1,
            max: None,
        })];
        let mut seen = Vec::new();
        let mut groups = 0;
        walk_pattern_payloads(&folded, &mut |payload| match payload {
            PatternPayload::Node(node) => seen.push(node.labels[0].clone()),
            PatternPayload::Relationship(_) => {}
            PatternPayload::Quantified(_) => groups += 1,
        });
        assert_eq!(seen, ["Elements"], "a quantified shape reads elements");
        assert_eq!(groups, 1, "the group is visited before its payloads");
    }
}
