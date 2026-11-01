//! Shared plan-node test fixtures: the pattern builders the plan_node
//! submodules' tests build their subjects from — one spelling of the
//! bare `NodePattern` (and its companions), never a per-module copy.

use cypherlite_ir::{Direction, NodePattern, PathPattern, RelPattern};

/// A bare node pattern with the given variable and labels.
pub(super) fn node(variable: Option<&str>, labels: &[&str]) -> NodePattern {
    NodePattern {
        variable: variable.map(str::to_string),
        labels: labels.iter().map(|s| s.to_string()).collect(),
        label_expr: None,
        properties: None,
        where_clause: None,
    }
}

/// One rel pattern with the given types and quantifier bounds.
pub(super) fn rel(types: &[&str], min: u32, max: Option<u32>) -> RelPattern {
    RelPattern {
        variable: None,
        types: types.iter().map(|s| s.to_string()).collect(),
        type_expr: None,
        direction: Direction::Outgoing,
        properties: None,
        var_length: max.is_some() || min > 1,
        min_hops: min,
        max_hops: max,
        where_clause: None,
        quantified: false,
    }
}

/// A compact alternating pattern: nodes then rels between them.
pub(super) fn compact(nodes: Vec<NodePattern>, rels: Vec<RelPattern>) -> PathPattern {
    PathPattern {
        variable: None,
        elements: Vec::new(),
        nodes,
        rels,
        where_clause: None,
        mode: None,
        selector: None,
        is_shortest_path_function: false,
    }
}
