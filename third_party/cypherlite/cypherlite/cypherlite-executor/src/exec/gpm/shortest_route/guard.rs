//! The route's one guard: the patterns ADR-0186's Decision sends to BFS.

use std::collections::HashSet;

use cypherlite_core::{RelId, Value};
use cypherlite_ir::{
    MatchMode, NodePattern, PathCount, PathMode, PathPattern, PathSelector, PatternElement,
    RelPattern,
};

use crate::eval::{expr_vars, Row};
use crate::shortest::followable;

/// The three elements of a routed pattern and its selector kind.
pub(super) struct Shape<'p> {
    pub start: &'p NodePattern,
    pub rel: &'p RelPattern,
    pub end: &'p NodePattern,
    /// All-path selector (every shortest trail) rather than single-path.
    pub all: bool,
}

/// The one guard: the selectors, match and path modes, and pattern shape of
/// ADR-0186's Decision, an empty incoming used-relationship set, and row
/// bindings the search can honor. `None` keeps the pattern on GPM.
pub(super) fn route_shape<'p>(
    path: &'p PathPattern,
    match_mode: MatchMode,
    used: &HashSet<RelId>,
    row: &Row,
) -> Option<Shape<'p>> {
    if match_mode != MatchMode::DifferentRelationships
        || !used.is_empty()
        || path.where_clause.is_some()
        || !matches!(path.mode, None | Some(PathMode::Walk | PathMode::Trail))
    {
        return None;
    }
    let all = match path.selector.as_ref()? {
        PathSelector::AnyShortest => false,
        PathSelector::AllShortest => true,
        PathSelector::Shortest {
            count: Some(PathCount::Literal(1)),
            group,
        } => *group,
        PathSelector::Shortest { .. } | PathSelector::Any(_) | PathSelector::All => return None,
    };
    let [PatternElement::Node(start), PatternElement::Relationship(rel), PatternElement::Node(end)] =
        path.elements.as_slice()
    else {
        return None;
    };
    if !rel.var_length
        || rel.quantified
        || rel.min_hops > 1
        || rel.where_clause.is_some()
        || !followable(start, rel, end)
        || rel.variable.as_ref().is_some_and(|v| row.contains_key(v))
    {
        return None;
    }
    // GPM checks the end node with the relationship variable already bound;
    // an endpoint map that reads it (or the path variable) stays on GPM.
    let late: Vec<&String> = [&rel.variable, &path.variable]
        .into_iter()
        .flatten()
        .collect();
    let endpoint_ok = |np: &NodePattern| {
        let mut vars = HashSet::new();
        if let Some(props) = &np.properties {
            expr_vars(props, &mut vars);
        }
        np.where_clause.is_none()
            && np
                .variable
                .as_ref()
                .is_none_or(|v| matches!(row.get(v), None | Some(Value::Node(_))))
            && !late.iter().any(|v| vars.contains(*v))
    };
    (endpoint_ok(start) && endpoint_ok(end)).then_some(Shape {
        start,
        rel,
        end,
        all,
    })
}
