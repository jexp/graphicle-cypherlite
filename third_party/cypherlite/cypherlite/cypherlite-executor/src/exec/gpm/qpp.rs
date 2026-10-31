//! Quantified path patterns: the repetition cursor, the per-variable
//! group accumulators, and the QPP DFS over `gpm_walk_elements` iterations.

use super::*;
use cypherlite_ir::QuantifiedPathPattern;

/// Quantified-path-pattern iteration cursor: repetition count and the
/// per-variable group accumulators.
struct QppCursor {
    count: u32,
    max: u32,
    groups: BTreeMap<String, Vec<Value>>,
}

pub(super) fn gpm_expand_quantified(
    gctx: &GpmCtx<'_>,
    mut state: GpmState,
    qpp: &QuantifiedPathPattern,
    target: Option<&NodePattern>,
) -> Result<Vec<GpmState>> {
    let GpmCtx { graph, .. } = *gctx;
    let mut names = Vec::new();
    gpm_group_names(&qpp.elements, &mut names);
    if let Some(name) = &qpp.subpath_variable {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    // A name a predicate of this quantification references that neither
    // the entering row nor the quantification itself binds — a non-local
    // singleton declared later in the pattern — reads as null inside the
    // quantification (CIP-28 [5], Neo4j-verified); binding it on the
    // walk's base row states that once instead of the evaluator
    // fabricating it (ADR-0335).
    for name in qpp_unbound_predicate_names(qpp, &state.row, &names) {
        state.row.insert(name, Value::Null);
    }
    let groups = names
        .iter()
        .map(|name| (name.clone(), Vec::<Value>::new()))
        .collect::<BTreeMap<_, _>>();
    let max = qpp
        .max
        .unwrap_or_else(|| graph.all_rel_ids().len().saturating_add(1).min(32) as u32);
    let mut out = Vec::new();
    gpm_qpp_dfs(
        gctx,
        state,
        qpp,
        target,
        QppCursor {
            count: 0,
            max,
            groups,
        },
        &names,
        &mut out,
    )?;
    Ok(out)
}

/// The predicate-referenced names of `qpp` that `row` does not bind and the
/// quantification's own group `names` will not bind — the later-declared
/// non-local singletons [`gpm_expand_quantified`] binds to null.
fn qpp_unbound_predicate_names(
    qpp: &QuantifiedPathPattern,
    row: &Row,
    names: &[String],
) -> Vec<String> {
    let mut vars = HashSet::new();
    for element in &qpp.elements {
        let predicate = match element {
            PatternElement::Node(node) => node.where_clause.as_ref(),
            PatternElement::Relationship(rel) => rel.where_clause.as_ref(),
            PatternElement::Quantified(_) => None,
        };
        if let Some(predicate) = predicate {
            cypherlite_ir::expr_vars(predicate, &mut vars);
        }
    }
    if let Some(predicate) = &qpp.where_clause {
        cypherlite_ir::expr_vars(predicate, &mut vars);
    }
    vars.into_iter()
        .filter(|name| !row.contains_key(name) && !names.contains(name))
        .collect()
}

fn gpm_group_names(elements: &[PatternElement], out: &mut Vec<String>) {
    for element in elements {
        let name = match element {
            PatternElement::Node(node) => node.variable.as_ref(),
            PatternElement::Relationship(rel) => rel.variable.as_ref(),
            PatternElement::Quantified(qpp) => qpp.subpath_variable.as_ref(),
        };
        if let Some(name) = name {
            if !out.contains(name) {
                out.push(name.clone());
            }
        }
    }
}

fn gpm_qpp_iteration_predicates_ok(
    graph: &ExecGraph<'_>,
    qpp: &QuantifiedPathPattern,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<bool> {
    for element in &qpp.elements {
        let predicate = match element {
            PatternElement::Node(node) => node.where_clause.as_ref(),
            PatternElement::Relationship(rel) => rel.where_clause.as_ref(),
            PatternElement::Quantified(_) => None,
        };
        if let Some(predicate) = predicate {
            if eval(graph, row, predicate, params, cypher_version)?.truthy() != Some(true) {
                return Ok(false);
            }
        }
    }
    if let Some(predicate) = &qpp.where_clause {
        if eval(graph, row, predicate, params, cypher_version)?.truthy() != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn gpm_qpp_dfs(
    gctx: &GpmCtx<'_>,
    state: GpmState,
    qpp: &QuantifiedPathPattern,
    target: Option<&NodePattern>,
    cursor: QppCursor,
    names: &[String],
    out: &mut Vec<GpmState>,
) -> Result<()> {
    let GpmCtx {
        graph,
        params,
        cypher_version,
        residual,
        ..
    } = *gctx;
    let QppCursor { count, max, groups } = cursor;
    if count >= qpp.min {
        let mut accepted = state.clone();
        for (name, values) in &groups {
            accepted
                .row
                .insert(name.clone(), Value::List(values.clone().into()));
        }
        if gpm_accept_target(
            graph,
            &mut accepted,
            target,
            params,
            cypher_version,
            residual,
        )? {
            out.push(accepted);
        }
    }
    if count >= max || qpp.elements.is_empty() {
        return Ok(());
    }
    let mut clean = state.row.clone();
    for name in names {
        clean.shift_remove(name);
    }
    let Some(PatternElement::Node(first)) = qpp.elements.first() else {
        return Ok(());
    };
    if !node_ok(graph, state.current, first, &clean, params, cypher_version)?
        || !gpm_bind_node(graph, first, state.current, &mut clean)
    {
        return Ok(());
    }
    let local_start = graph.node(state.current).into_iter().collect::<Vec<_>>();
    let iter_start = GpmState {
        row: clean,
        used: state.used.clone(),
        current: state.current,
        nodes: local_start,
        rels: Vec::new(),
    };
    let iterations = gpm_walk_elements(gctx, &qpp.elements, 1, vec![iter_start])?;
    for iteration in iterations {
        if !gpm_qpp_iteration_predicates_ok(graph, qpp, &iteration.row, params, cypher_version)? {
            continue;
        }
        let mut next_groups = groups.clone();
        for name in names {
            if Some(name) == qpp.subpath_variable.as_ref() {
                continue;
            }
            if let Some(value) = iteration.row.get(name) {
                next_groups
                    .entry(name.clone())
                    .or_default()
                    .push(value.clone());
            }
        }
        if let Some(name) = &qpp.subpath_variable {
            next_groups
                .entry(name.clone())
                .or_default()
                .push(Value::from_path(Path {
                    nodes: iteration.nodes.clone(),
                    rels: iteration.rels.clone(),
                }));
        }
        let mut next = state.clone();
        next.row = iteration.row;
        next.used = iteration.used;
        next.current = iteration.current;
        next.nodes.extend(iteration.nodes.into_iter().skip(1));
        next.rels.extend(iteration.rels);
        gpm_qpp_dfs(
            gctx,
            next,
            qpp,
            target,
            QppCursor {
                count: count + 1,
                max,
                groups: next_groups,
            },
            names,
            out,
        )?;
    }
    Ok(())
}
