//! The plan-warning detection at tree construction (ADR-0344, typed node
//! warnings): the MATCH-shape checks that construct the typed warnings a
//! plan node carries. The vocabulary itself — the [`PlanWarning`] enum,
//! its names, message texts, and notification projection — lives in the
//! model beside the notification vocabulary it projects into and the
//! wire parse it coordinates (LABS-461: one warning type); this module
//! is its engine-side construction site.
//!
//! [`PlanWarning::EstimationMiss`] is not detected at MATCH shape —
//! [`estimation_miss_warning`] derives it where the PROFILE metrics are
//! stamped (the same 10x policy the ratio cell's warning marker reads,
//! never a second computation).

use cypherlite_ir::{walk_pattern_payloads, PathPattern, PatternPayload};
use cypherlite_model::wire::estimation_miss;
pub use cypherlite_model::wire::PlanWarning;

use crate::stats::PlanStats;

/// The `EstimationMiss` warning (typed node warnings), derived from the
/// one 10x overshoot policy ([`estimation_miss`]) the
/// ratio cell's warning marker reads — never a second computation — so
/// the warning fires exactly where the cell carries the `!`. `None`
/// where the policy does not fire or the operator carries no estimate to
/// judge.
pub(super) fn estimation_miss_warning(
    estimated: Option<u64>,
    actual: Option<u64>,
) -> Option<PlanWarning> {
    match (estimated, actual) {
        (Some(estimated), Some(actual)) if estimation_miss(estimated, actual) => {
            Some(PlanWarning::EstimationMiss)
        }
        _ => None,
    }
}

/// The warnings a `MATCH` subject carries, detected at tree construction
/// (typed node warnings): a multi-pattern `MATCH` whose patterns share no
/// join anchor, and a label-less scan while a property index exists. The
/// stats contract ([`PlanStats`], the one every plan-tree stats read
/// reads) supplies the store's property indexes — the suggestions the
/// `AllNodesScan` warning names.
pub(super) fn match_warnings(patterns: &[PathPattern], stats: &dyn PlanStats) -> Vec<PlanWarning> {
    let mut warnings = Vec::new();
    if patterns.len() > 1 && !patterns_share_an_anchor(patterns) {
        warnings.push(PlanWarning::CartesianProduct);
    }
    // The label-less scan: the same subject shape the operator naming
    // derives (`lone_node_of`), never a second shape derivation.
    if let Some(node) = super::subject::lone_node_of(patterns) {
        if super::subject::is_all_nodes_scan(node) {
            // The suggestion names the first property index the store
            // carries, in the catalog's own listing order.
            if let Some(index) = stats.property_index_texts().first() {
                warnings.push(PlanWarning::AllNodesScan {
                    index: index.clone(),
                });
            }
        }
    }
    warnings
}

/// Whether the patterns share a join anchor: a node variable that appears
/// in more than one pattern. A variable reused within one pattern joins
/// inside it and is not an anchor across patterns.
fn patterns_share_an_anchor(patterns: &[PathPattern]) -> bool {
    let per_pattern: Vec<Vec<&str>> = patterns.iter().map(pattern_node_variables).collect();
    per_pattern.iter().enumerate().any(|(i, variables)| {
        variables.iter().any(|variable| {
            per_pattern
                .iter()
                .enumerate()
                .any(|(j, others)| j != i && others.contains(variable))
        })
    })
}

/// A pattern's node variables, read through the IR's one payload visitor —
/// the post-rewrite view precedence and quantified-group recursion live
/// there, not here.
fn pattern_node_variables(pattern: &PathPattern) -> Vec<&str> {
    let mut variables = Vec::new();
    walk_pattern_payloads(pattern, &mut |payload| {
        if let PatternPayload::Node(node) = payload {
            if let Some(variable) = node.variable.as_deref() {
                variables.push(variable);
            }
        }
    });
    variables
}

#[cfg(test)]
mod tests {
    use cypherlite_ir::NodePattern;

    use super::super::fixtures::node;
    use super::*;

    /// A lone-node pattern — the scan shape.
    fn lone(node: NodePattern) -> PathPattern {
        PathPattern {
            variable: None,
            elements: Vec::new(),
            nodes: vec![node],
            rels: Vec::new(),
            where_clause: None,
            mode: None,
            selector: None,
            is_shortest_path_function: false,
        }
    }

    #[test]
    fn patterns_without_a_shared_anchor_do_not_join() {
        let disjoint = vec![
            lone(node(Some("a"), &["Person"])),
            lone(node(Some("b"), &["City"])),
        ];
        assert!(!patterns_share_an_anchor(&disjoint), "no shared variable");
        let anchored = vec![
            lone(node(Some("a"), &["Person"])),
            lone(node(Some("a"), &["City"])),
        ];
        assert!(patterns_share_an_anchor(&anchored), "a joins both");
        // A variable reused within one pattern only: not an anchor
        // across patterns.
        let mut within = lone(node(Some("a"), &["Person"]));
        within.nodes.push(node(Some("a"), &["City"]));
        assert!(
            !patterns_share_an_anchor(&[within, lone(node(Some("b"), &[]))]),
            "a within one pattern is not a cross-pattern anchor"
        );
    }
}
