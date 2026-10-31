//! Graph pattern matching engine: quantified path patterns, inline
//! predicates, path modes, and selectors.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use cypherlite_core::{NodeId, Path, RelId, Result, Value};
use cypherlite_ir::{
    Expr, MatchMode, NodePattern, PathMode, PathPattern, PatternElement, RelPattern,
};

use crate::ctx::ExecCtx;
use crate::eval::{eval, expr_vars, label_expr_matches, Row};
use crate::overlay::ExecGraph;

use super::candidates::{node_candidates, node_ok};
use super::create::map_from_expr;
use super::expand::{MatchReduce, Matched};
use super::match_clause::eval_match_predicate;
use super::pushdown::{residual_ok, Residual};
use crate::ctx::bind_node_stub;

mod qpp;
mod selector;
mod shortest_route;

use qpp::gpm_expand_quantified;
pub(crate) use selector::apply_gpm_selector;
use shortest_route::match_shortest_route;

#[derive(Clone)]
pub(crate) struct GpmState {
    pub row: Row,
    pub used: HashSet<RelId>,
    pub current: NodeId,
    pub nodes: Vec<cypherlite_core::Node>,
    pub rels: Vec<cypherlite_core::Relationship>,
}

/// Context shared by the GPM walker: graph view, parameters, the enclosing
/// statement's CIP-150 `cypher_version`, match mode, path mode, and residual
/// predicates.
#[derive(Clone, Copy)]
struct GpmCtx<'a> {
    graph: &'a ExecGraph<'a>,
    params: &'a BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    match_mode: MatchMode,
    path_mode: Option<PathMode>,
    residual: &'a Residual<'a>,
}

/// Variable-length relationship DFS bounds and cursor (GPM trail).
struct GpmRelWalk {
    min: u32,
    max: u32,
    hops: u32,
    trail: Vec<cypherlite_core::Relationship>,
}

pub(crate) fn match_gpm_path(
    ctx: ExecCtx<'_>,
    path: &PathPattern,
    row: Row,
    used: HashSet<RelId>,
    match_mode: MatchMode,
    reduce: Option<&MatchReduce<'_>>,
    keep_used: bool,
) -> Result<Matched> {
    let graph = ctx.graph;
    let params = ctx.params;
    let cypher_version = ctx.cypher_version;
    if let Some(rows) = match_shortest_route(ctx, path, &row, &used, match_mode)? {
        return apply_reduce(graph, rows, reduce, keep_used);
    }
    let residual = Residual::empty();
    let elements = if path.elements.is_empty() {
        return Ok(Vec::new());
    } else {
        &path.elements
    };
    let mut states = if let Some(PatternElement::Node(node)) = elements.first() {
        gpm_start_nodes(graph, node, row, used, params, cypher_version, &residual)?
    } else {
        let anonymous = NodePattern {
            variable: None,
            labels: Vec::new(),
            label_expr: None,
            properties: None,
            where_clause: None,
        };
        gpm_start_nodes(
            graph,
            &anonymous,
            row,
            used,
            params,
            cypher_version,
            &residual,
        )?
    };
    let start = usize::from(matches!(elements.first(), Some(PatternElement::Node(_))));
    let gctx = GpmCtx {
        graph,
        params,
        cypher_version,
        match_mode,
        path_mode: path.mode,
        residual: &residual,
    };
    // LABS-448: the walk phase, through the one wrapper — the whole
    // elements walk this clause performs, inclusive of its nested
    // resolutions (the recursion nests inside the span).
    states = super::phase_walk(|| gpm_walk_elements(&gctx, elements, start, states))?;

    let mut candidates = Vec::new();
    for mut state in states {
        if !gpm_path_mode_allows(path.mode, &state.nodes, &state.rels) {
            continue;
        }
        if let Some(predicate) = path.where_clause.as_deref() {
            if gpm_expr_ready(predicate, &state.row)
                && eval_match_predicate(graph, &state.row, predicate, params, cypher_version)?
                    .truthy()
                    != Some(true)
            {
                continue;
            }
        }
        if let Some(name) = &path.variable {
            state.row.insert(
                name.clone(),
                Value::from_path(Path {
                    nodes: state.nodes.clone(),
                    rels: state.rels.clone(),
                }),
            );
        }
        candidates.push(state);
    }
    let candidates = apply_gpm_selector(candidates, path.selector.as_ref(), params)?;
    apply_reduce(
        graph,
        candidates
            .into_iter()
            .map(|state| (state.row, state.used))
            .collect(),
        reduce,
        keep_used,
    )
}

fn apply_reduce(
    graph: &ExecGraph<'_>,
    rows: Matched,
    reduce: Option<&MatchReduce<'_>>,
    keep_used: bool,
) -> Result<Matched> {
    if reduce.is_none() && keep_used {
        return Ok(rows);
    }
    let mut out = Vec::with_capacity(rows.len());
    for (row, used) in rows {
        let row = if let Some(reduce) = reduce {
            if !gpm_inline_predicates_ok(
                graph,
                reduce.patterns,
                &row,
                reduce.params,
                reduce.cypher_version,
            )? {
                continue;
            }
            if let Some(predicate) = reduce.where_clause {
                if super::match_clause::eval_match_predicate(
                    graph,
                    &row,
                    predicate,
                    reduce.params,
                    reduce.cypher_version,
                )?
                .truthy()
                    != Some(true)
                {
                    continue;
                }
            }
            super::project::finish_match_row(
                graph,
                &row,
                reduce.projection,
                reduce.params,
                reduce.cypher_version,
            )?
        } else {
            row
        };
        let used = if keep_used { used } else { HashSet::new() };
        out.push((row, used));
    }
    Ok(out)
}

fn gpm_start_nodes(
    graph: &ExecGraph<'_>,
    node: &NodePattern,
    row: Row,
    used: HashSet<RelId>,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    residual: &Residual<'_>,
) -> Result<Vec<GpmState>> {
    let mut out = Vec::new();
    for id in
        super::phase_seek(|| node_candidates(graph, node, &row, params, cypher_version, residual))?
    {
        graph.cancel_tick()?;
        if !node_ok(graph, id, node, &row, params, cypher_version)? {
            continue;
        }
        let mut bound = row.clone();
        if !gpm_bind_node(graph, node, id, &mut bound) {
            continue;
        }
        if let Some(predicate) = &node.where_clause {
            if gpm_expr_ready(predicate, &bound)
                && eval_match_predicate(graph, &bound, predicate, params, cypher_version)?.truthy()
                    != Some(true)
            {
                continue;
            }
        }
        if !residual_ok(graph, &bound, params, cypher_version, residual)? {
            continue;
        }
        if let Some(value) = graph.node(id) {
            out.push(GpmState {
                row: bound,
                used: used.clone(),
                current: id,
                nodes: vec![value],
                rels: Vec::new(),
            });
        }
    }
    Ok(out)
}

fn gpm_bind_node(graph: &ExecGraph<'_>, node: &NodePattern, id: NodeId, row: &mut Row) -> bool {
    let Some(name) = &node.variable else {
        return true;
    };
    match row.get(name) {
        Some(Value::Node(existing)) => existing.id == id,
        Some(Value::Null) | None => {
            row.insert(name.clone(), bind_node_stub(graph, id));
            true
        }
        _ => false,
    }
}

fn gpm_walk_elements(
    gctx: &GpmCtx<'_>,
    elements: &[PatternElement],
    index: usize,
    mut states: Vec<GpmState>,
) -> Result<Vec<GpmState>> {
    let GpmCtx {
        graph,
        params,
        cypher_version,
        ..
    } = *gctx;
    if index >= elements.len() {
        return Ok(states);
    }
    if let PatternElement::Node(node) = &elements[index] {
        let mut joined = Vec::new();
        for mut state in states.drain(..) {
            if node_ok(
                graph,
                state.current,
                node,
                &state.row,
                params,
                cypher_version,
            )? && gpm_bind_node(graph, node, state.current, &mut state.row)
            {
                if let Some(predicate) = &node.where_clause {
                    if gpm_expr_ready(predicate, &state.row)
                        && eval_match_predicate(
                            graph,
                            &state.row,
                            predicate,
                            params,
                            cypher_version,
                        )?
                        .truthy()
                            != Some(true)
                    {
                        continue;
                    }
                }
                joined.push(state);
            }
        }
        return gpm_walk_elements(gctx, elements, index + 1, joined);
    }
    let (target, advance) = match elements.get(index + 1) {
        Some(PatternElement::Node(node)) => (Some(node), 2),
        _ => (None, 1),
    };
    let mut next = Vec::new();
    for state in states.drain(..) {
        match &elements[index] {
            PatternElement::Relationship(rel) => {
                next.extend(gpm_expand_relationship(gctx, state, rel, target)?)
            }
            PatternElement::Quantified(qpp) => {
                next.extend(gpm_expand_quantified(gctx, state, qpp, target)?)
            }
            PatternElement::Node(_) => unreachable!(),
        }
    }
    gpm_walk_elements(gctx, elements, index + advance, next)
}

fn gpm_expand_relationship(
    gctx: &GpmCtx<'_>,
    state: GpmState,
    rel: &RelPattern,
    target: Option<&NodePattern>,
) -> Result<Vec<GpmState>> {
    let GpmCtx { graph, .. } = *gctx;
    let min = if rel.var_length { rel.min_hops } else { 1 };
    let max = if rel.var_length {
        rel.max_hops
            .unwrap_or_else(|| graph.all_rel_ids().len().max(1) as u32)
    } else {
        1
    };
    let mut out = Vec::new();
    gpm_rel_dfs(
        gctx,
        state,
        rel,
        target,
        GpmRelWalk {
            min,
            max,
            hops: 0,
            trail: Vec::new(),
        },
        &mut out,
    )?;
    Ok(out)
}

fn gpm_rel_dfs(
    gctx: &GpmCtx<'_>,
    state: GpmState,
    rel: &RelPattern,
    target: Option<&NodePattern>,
    walk: GpmRelWalk,
    out: &mut Vec<GpmState>,
) -> Result<()> {
    let GpmCtx {
        graph,
        params,
        cypher_version,
        match_mode,
        path_mode,
        residual,
    } = *gctx;
    let GpmRelWalk {
        min,
        max,
        hops,
        trail,
    } = walk;
    if hops >= min {
        let mut accepted = state.clone();
        if let Some(name) = &rel.variable {
            let value = if rel.var_length {
                Value::List(
                    trail
                        .iter()
                        .cloned()
                        .map(Value::from_relationship)
                        .collect::<Vec<_>>()
                        .into(),
                )
            } else {
                trail
                    .last()
                    .cloned()
                    .map(Value::from_relationship)
                    .unwrap_or(Value::Null)
            };
            match accepted.row.get(name) {
                Some(existing) if !existing.eq_non_null(&value) => {}
                _ => {
                    accepted.row.insert(name.clone(), value);
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
            }
        } else if gpm_accept_target(
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
    if hops >= max {
        return Ok(());
    }
    graph.try_for_each_neighbor(state.current, rel.direction, &rel.types, |rid| {
        graph.cancel_tick()?;
        let enforce_unique = match_mode == MatchMode::DifferentRelationships
            || matches!(
                path_mode,
                Some(PathMode::Trail | PathMode::Acyclic | PathMode::Simple)
            );
        if enforce_unique && state.used.contains(&rid) {
            return Ok(());
        }
        let Some(rel_value) = graph.rel(rid) else {
            return Ok(());
        };
        if let Some(type_expr) = &rel.type_expr {
            if !label_expr_matches(
                graph,
                &state.row,
                params,
                type_expr,
                std::slice::from_ref(&rel_value.rel_type),
                true,
                cypher_version,
            )? {
                return Ok(());
            }
        }
        if let Some(properties) = &rel.properties {
            let wanted = map_from_expr(graph, &state.row, properties, params, cypher_version)?;
            if wanted.iter().any(|(key, value)| {
                rel_value
                    .properties
                    .get(key)
                    .is_none_or(|actual| !actual.eq_non_null(value))
            }) {
                return Ok(());
            }
        }
        if let Some(predicate) = &rel.where_clause {
            let mut predicate_row = state.row.clone();
            if let Some(name) = &rel.variable {
                predicate_row.insert(name.clone(), Value::from_relationship(rel_value.clone()));
            }
            if gpm_expr_ready(predicate, &predicate_row)
                && eval_match_predicate(graph, &predicate_row, predicate, params, cypher_version)?
                    .truthy()
                    != Some(true)
            {
                return Ok(());
            }
        }
        let Some(next_id) = graph.hop(state.current, rid, rel.direction) else {
            return Ok(());
        };
        let mut next = state.clone();
        next.current = next_id;
        next.used.insert(rid);
        next.rels.push(rel_value.clone());
        if let Some(node) = graph.node(next_id) {
            next.nodes.push(node);
        }
        let mut next_trail = trail.clone();
        next_trail.push(rel_value);
        gpm_rel_dfs(
            gctx,
            next,
            rel,
            target,
            GpmRelWalk {
                min,
                max,
                hops: hops + 1,
                trail: next_trail,
            },
            out,
        )
    })?;
    Ok(())
}

fn gpm_accept_target(
    graph: &ExecGraph<'_>,
    state: &mut GpmState,
    target: Option<&NodePattern>,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    residual: &Residual<'_>,
) -> Result<bool> {
    let Some(node) = target else {
        return residual_ok(graph, &state.row, params, cypher_version, residual);
    };
    if !node_ok(
        graph,
        state.current,
        node,
        &state.row,
        params,
        cypher_version,
    )? || !gpm_bind_node(graph, node, state.current, &mut state.row)
    {
        return Ok(false);
    }
    if let Some(predicate) = &node.where_clause {
        if gpm_expr_ready(predicate, &state.row)
            && eval_match_predicate(graph, &state.row, predicate, params, cypher_version)?.truthy()
                != Some(true)
        {
            return Ok(false);
        }
    }
    residual_ok(graph, &state.row, params, cypher_version, residual)
}

fn gpm_expr_ready(expr: &Expr, row: &Row) -> bool {
    let mut vars = HashSet::new();
    expr_vars(expr, &mut vars);
    vars.iter().all(|name| row.contains_key(name))
}

pub(crate) fn gpm_inline_predicates_ok(
    graph: &ExecGraph<'_>,
    patterns: &[PathPattern],
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<bool> {
    fn check_elements(
        graph: &ExecGraph<'_>,
        elements: &[PatternElement],
        row: &Row,
        params: &BTreeMap<String, Value>,
        cypher_version: Option<u32>,
    ) -> Result<bool> {
        for element in elements {
            match element {
                PatternElement::Node(node) => {
                    if let Some(predicate) = &node.where_clause {
                        if eval(graph, row, predicate, params, cypher_version)?.truthy()
                            != Some(true)
                        {
                            return Ok(false);
                        }
                    }
                }
                PatternElement::Relationship(rel) if rel.quantified => {
                    if let (Some(name), Some(predicate)) = (&rel.variable, &rel.where_clause) {
                        let values = match row.get(name) {
                            Some(Value::List(values)) => Arc::clone(values),
                            _ => Default::default(),
                        };
                        for value in values.iter() {
                            let mut iteration = row.clone();
                            iteration.insert(name.clone(), value.clone());
                            if eval(graph, &iteration, predicate, params, cypher_version)?.truthy()
                                != Some(true)
                            {
                                return Ok(false);
                            }
                        }
                    }
                }
                PatternElement::Relationship(rel) => {
                    if let Some(predicate) = &rel.where_clause {
                        if eval(graph, row, predicate, params, cypher_version)?.truthy()
                            != Some(true)
                        {
                            return Ok(false);
                        }
                    }
                }
                // QPP predicates are checked against singleton iteration
                // bindings while the quantified operand is expanded.
                PatternElement::Quantified(_) => {}
            }
        }
        Ok(true)
    }
    for path in patterns {
        if !check_elements(graph, &path.elements, row, params, cypher_version)? {
            return Ok(false);
        }
        if let Some(predicate) = path.where_clause.as_deref() {
            if eval(graph, row, predicate, params, cypher_version)?.truthy() != Some(true) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

pub(crate) fn gpm_path_mode_allows(
    mode: Option<PathMode>,
    nodes: &[cypherlite_core::Node],
    rels: &[cypherlite_core::Relationship],
) -> bool {
    match mode.unwrap_or(PathMode::Walk) {
        PathMode::Walk => true,
        PathMode::Trail => {
            let mut seen = HashSet::new();
            rels.iter().all(|rel| seen.insert(rel.id))
        }
        PathMode::Acyclic => {
            let mut seen = HashSet::new();
            nodes.iter().all(|node| seen.insert(node.id))
        }
        PathMode::Simple => {
            let mut seen = HashSet::new();
            let last = nodes.len().saturating_sub(1);
            for (index, node) in nodes.iter().enumerate() {
                if index == last && nodes.first().is_some_and(|first| first.id == node.id) {
                    continue;
                }
                if !seen.insert(node.id) {
                    return false;
                }
            }
            true
        }
    }
}

#[cfg(test)]
mod gpm_tests;
