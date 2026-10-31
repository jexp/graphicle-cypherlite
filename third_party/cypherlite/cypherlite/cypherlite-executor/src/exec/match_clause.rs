//! `MATCH` clause driver: row loop, horizontal aggregates, named paths,
//! and per-path dispatch into the GPM engine or classic expansion.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{Error, NodeId, Path, RelId, Result, Value};
use cypherlite_ir::{path_pattern_vars, Expr, MatchMode, PathPattern};

use crate::ctx::ExecCtx;
use crate::eval::{eval, expr_vars, Row};
use crate::overlay::ExecGraph;

use super::aggregate::SumAcc;
use super::expand::{match_resolved_path, CollectSink, MatchReduce, Matched};
use super::gpm::{gpm_inline_predicates_ok, match_gpm_path};
use super::project::{finish_match_row, SinkProjection};
use super::pushdown::{
    add_gpm_element_names, flatten_and_owned, match_scope, residual_ok, MatchCap, Residual,
};

#[allow(clippy::too_many_arguments)]
pub(crate) fn match_clause(
    ctx: ExecCtx<'_>,
    rows: Vec<Row>,
    optional: bool,
    match_mode: MatchMode,
    patterns: &[PathPattern],
    where_clause: Option<&Expr>,
    mut cap: Option<&mut MatchCap<'_>>,
    projection: Option<&SinkProjection<'_>>,
) -> Result<Vec<Row>> {
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    let mut out = Vec::new();
    let conjuncts = where_clause.map(flatten_and_owned).unwrap_or_default();
    let reduce = projection.map(|projection| MatchReduce {
        projection,
        patterns,
        where_clause,
        params,
        cypher_version,
    });
    for row in rows {
        graph.cancel_tick()?;
        if cap.as_ref().is_some_and(|c| c.full()) {
            break;
        }
        let scope = match_scope(&row, patterns);
        let residual = Residual {
            conjuncts: &conjuncts,
            scope: &scope,
        };
        let mut matched = match_paths(
            ctx,
            patterns,
            match_mode,
            row.clone(),
            &residual,
            cap.as_deref_mut(),
            reduce.as_ref(),
        )?;
        // The sink already applied inline predicates, WHERE, and the projection.
        if projection.is_none() {
            let mut inline_filtered = Vec::with_capacity(matched.len());
            for candidate in matched {
                if gpm_inline_predicates_ok(graph, patterns, &candidate, params, cypher_version)? {
                    inline_filtered.push(candidate);
                }
            }
            matched = inline_filtered;
            if let Some(w) = where_clause {
                let mut filtered = Vec::with_capacity(matched.len());
                for candidate in matched {
                    if eval_match_predicate(graph, &candidate, w, params, cypher_version)?.truthy()
                        == Some(true)
                    {
                        filtered.push(candidate);
                    }
                }
                matched = filtered;
            }
        }
        if matched.is_empty() && optional {
            let mut r = row;
            bind_pattern_nulls(&mut r, patterns);
            if let Some(projection) = projection {
                out.push(finish_match_row(
                    graph,
                    &r,
                    projection,
                    params,
                    cypher_version,
                )?);
            } else {
                out.push(r);
            }
        } else {
            out.extend(matched);
        }
    }
    Row::share_schemas(&mut out);
    Ok(out)
}

pub(crate) fn eval_match_predicate(
    graph: &ExecGraph<'_>,
    row: &Row,
    expr: &Expr,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Value> {
    let rewritten = rewrite_horizontal_aggregates(graph, row, expr, params, cypher_version)?;
    eval(graph, row, &rewritten, params, cypher_version)
}

pub(crate) fn rewrite_horizontal_aggregates(
    graph: &ExecGraph<'_>,
    row: &Row,
    expr: &Expr,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Expr> {
    match expr {
        Expr::Function {
            name,
            distinct,
            args,
        } if !*distinct
            && args.len() == 1
            && matches!(
                name.to_ascii_lowercase().as_str(),
                "count" | "sum" | "avg" | "min" | "max" | "collect"
            ) =>
        {
            let mut vars = HashSet::new();
            expr_vars(&args[0], &mut vars);
            let list_vars: Vec<_> = vars
                .into_iter()
                .filter_map(|name| match row.get(&name) {
                    Some(Value::List(values)) => Some((name, values)),
                    _ => None,
                })
                .collect();
            if list_vars.is_empty() {
                return Ok(expr.clone());
            }
            let len = list_vars
                .iter()
                .map(|(_, values)| values.len())
                .max()
                .unwrap_or(0);
            let mut values = Vec::new();
            for index in 0..len {
                let mut iteration = row.clone();
                for (name, items) in &list_vars {
                    iteration.insert(
                        name.clone(),
                        items.get(index).cloned().unwrap_or(Value::Null),
                    );
                }
                values.push(eval(graph, &iteration, &args[0], params, cypher_version)?);
            }
            let value = horizontal_aggregate(name, values)?;
            Ok(Expr::RuntimeValue(value))
        }
        Expr::Unary { op, expr } => Ok(Expr::Unary {
            op: *op,
            expr: Box::new(rewrite_horizontal_aggregates(
                graph,
                row,
                expr,
                params,
                cypher_version,
            )?),
        }),
        Expr::Binary { op, left, right } => Ok(Expr::Binary {
            op: *op,
            left: Box::new(rewrite_horizontal_aggregates(
                graph,
                row,
                left,
                params,
                cypher_version,
            )?),
            right: Box::new(rewrite_horizontal_aggregates(
                graph,
                row,
                right,
                params,
                cypher_version,
            )?),
        }),
        Expr::Function {
            name,
            distinct,
            args,
        } => Ok(Expr::Function {
            name: name.clone(),
            distinct: *distinct,
            args: args
                .iter()
                .map(|arg| rewrite_horizontal_aggregates(graph, row, arg, params, cypher_version))
                .collect::<Result<_>>()?,
        }),
        other => Ok(other.clone()),
    }
}

pub(crate) fn horizontal_aggregate(name: &str, values: Vec<Value>) -> Result<Value> {
    let name = name.to_ascii_lowercase();
    if name == "collect" {
        return Ok(Value::List(
            values
                .into_iter()
                .filter(|value| !value.is_null())
                .collect::<Vec<_>>()
                .into(),
        ));
    }
    let non_null: Vec<_> = values
        .into_iter()
        .filter(|value| !value.is_null())
        .collect();
    if name == "count" {
        return Ok(Value::Int(non_null.len() as i64));
    }
    if matches!(name.as_str(), "min" | "max") {
        let mut iter = non_null.into_iter();
        let Some(mut best) = iter.next() else {
            return Ok(Value::Null);
        };
        for value in iter {
            let ordering = value.order_by_cmp(&best);
            if (name == "min" && ordering == CmpOrdering::Less)
                || (name == "max" && ordering == CmpOrdering::Greater)
            {
                best = value;
            }
        }
        return Ok(best);
    }
    // ADR-0205: the shared accumulator (F-4-1) — the horizontal path used
    // to check `integer != 0 || has_float` for mixing, silently accepting
    // `sum([0, duration])` where the grouped path and the fold reject it
    // (F-3-1); one accumulator, one behavior.
    let mut acc = SumAcc::new();
    for value in &non_null {
        acc.offer(value, &name)?;
    }
    // Exhaustive dispatch: a name that survived collect/count/min/max is
    // sum or avg — anything else is a planner bug, and must fail loudly
    // instead of silently summing.
    match name.as_str() {
        "sum" => Ok(acc.finish_sum()),
        "avg" => acc.finish_avg(),
        other => Err(Error::internal(format!(
            "unknown horizontal aggregate: {other}"
        ))),
    }
}

pub(crate) fn bind_pattern_nulls(row: &mut Row, patterns: &[PathPattern]) {
    for p in patterns {
        if let Some(v) = &p.variable {
            row.entry(v.clone()).or_insert(Value::Null);
        }
        for n in &p.nodes {
            if let Some(v) = &n.variable {
                row.entry(v.clone()).or_insert(Value::Null);
            }
        }
        for r in &p.rels {
            if let Some(v) = &r.variable {
                row.entry(v.clone()).or_insert(Value::Null);
            }
        }
        let mut names = HashSet::new();
        add_gpm_element_names(&p.elements, &mut names);
        for name in names {
            row.entry(name).or_insert(Value::Null);
        }
    }
}

pub(crate) fn bind_named_path(graph: &ExecGraph<'_>, path: &PathPattern, row: &mut Row) {
    let Some(name) = &path.variable else {
        return;
    };
    let mut nodes = Vec::new();
    let mut rels = Vec::new();
    if let Some(v) = &path.nodes[0].variable {
        if let Some(Value::Node(n)) = row.get(v) {
            nodes.push((**n).clone());
        }
    } else if let Some(Value::Node(n)) = row.get("__node0") {
        nodes.push((**n).clone());
    }
    for (i, rp) in path.rels.iter().enumerate() {
        let chunk: Vec<cypherlite_core::Relationship> =
            match rp.variable.as_ref().and_then(|v| row.get(v)) {
                Some(Value::Relationship(r)) => vec![(**r).clone()],
                Some(Value::List(xs)) => xs
                    .iter()
                    .filter_map(|v| match v {
                        Value::Relationship(r) => Some((**r).clone()),
                        _ => None,
                    })
                    .collect(),
                _ => match row.get(&format!("__trail{i}")) {
                    Some(Value::Relationship(r)) => vec![(**r).clone()],
                    Some(Value::List(xs)) => xs
                        .iter()
                        .filter_map(|v| match v {
                            Value::Relationship(r) => Some((**r).clone()),
                            _ => None,
                        })
                        .collect(),
                    _ => Vec::new(),
                },
            };
        let empty = chunk.is_empty();
        for r in chunk {
            let next_id = if nodes.last().map(|n| n.id) == Some(r.start) {
                r.end
            } else {
                r.start
            };
            rels.push(graph.rel(r.id).unwrap_or(r));
            if let Some(n) = graph.node(next_id) {
                nodes.push(n);
            }
        }
        if empty {
            fill_end_node(path, i, row, &mut nodes);
        }
    }
    if nodes.is_empty() {
        if let Some(v) = path.nodes.last().and_then(|n| n.variable.as_ref()) {
            if let Some(Value::Node(n)) = row.get(v) {
                nodes.push((**n).clone());
            }
        }
    }
    row.insert(name.clone(), Value::from_path(Path { nodes, rels }));
}

pub(crate) fn fill_end_node(
    path: &PathPattern,
    rel_idx: usize,
    row: &Row,
    nodes: &mut Vec<cypherlite_core::Node>,
) {
    if rel_idx + 1 >= path.nodes.len() {
        return;
    }
    let Some(v) = &path.nodes[rel_idx + 1].variable else {
        return;
    };
    if let Some(Value::Node(n)) = row.get(v) {
        if nodes.last().map(|x| x.id) != Some(n.id) {
            nodes.push((**n).clone());
        }
    }
}

/// The minimum side-A row count before the hub join pays for itself:
/// below it the nested loop expands side B once per surviving row anyway
/// (the measured single-left case joined as cheaply as the nested loop),
/// and the join's grouped bookkeeping is pure overhead.
const HUB_JOIN_MIN_SIDE_ROWS: usize = 128;

#[cfg(test)]
thread_local! {
    /// LABS-348 test hook: force `try_hub_join` to decline so a test can
    /// pin the nested loop's behavior on a fixture the join would take.
    static HUB_JOIN_DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// LABS-348 test counter: how many times the hub join took the pair.
    static HUB_JOIN_ENGAGED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// LABS-348 test helper: run `f` with the hub join forced off (the nested
/// loop's plan), restoring the flag afterwards.
#[cfg(test)]
pub(crate) fn with_hub_join_disabled<R>(f: impl FnOnce() -> R) -> R {
    /// Restores the flag on the way out — including on unwind, so a
    /// panicking assertion inside the closure cannot leave the join
    /// disabled for every later test on the thread (pivot.rs's guard).
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            HUB_JOIN_DISABLED.with(|flag| flag.set(false));
        }
    }
    HUB_JOIN_DISABLED.with(|flag| flag.set(true));
    let _restore = Restore;
    f()
}

/// LABS-348 test helper: drain the engagement counter (0 = the nested
/// loop took every pair since the last take).
#[cfg(test)]
pub(crate) fn take_hub_join_engagements() -> u32 {
    HUB_JOIN_ENGAGED.with(|c| c.replace(0))
}

/// LABS-348: detect the hub-join shape — exactly two patterns, each a
/// single unquantified hop between two nodes (so every variable sits at
/// an end), sharing exactly one node variable (the hub), with no side
/// variable already bound in the incoming row, no repeatable-elements
/// mode (relationship uniqueness applies), and no cap/reduce riding the
/// last pattern (the aggregation fold is its own join design; the
/// prototype scope is the plain row-returning MATCH).
fn hub_join_applies<'p>(
    patterns: &'p [PathPattern],
    match_mode: MatchMode,
    row: &Row,
    plain_rows_only: bool,
) -> Option<&'p str> {
    if patterns.len() != 2 || !plain_rows_only {
        return None;
    }
    if match_mode == MatchMode::RepeatableElements {
        return None;
    }
    for p in patterns {
        if p.nodes.len() != 2
            || p.rels.len() != 1
            || p.rels[0].var_length
            || p.rels[0].quantified
            || p.is_shortest_path_function
            || p.needs_gpm()
        {
            return None;
        }
        for v in path_pattern_vars(p) {
            if row.contains_key(v) {
                // A side variable (node or rel) already bound in the
                // incoming row: the nested loop's per-row binding is part
                // of the semantics this prototype does not re-derive.
                return None;
            }
        }
    }
    // The hub is the patterns' one shared variable, and it is a node
    // variable of both (a shared rel variable is not a hub end). The
    // canonical `path_pattern_vars` (cypherlite_ir — the one extraction,
    // LABS-348) covers node AND rel variables; both sides collect
    // into sets so the intersection counts NAMES, not occurrences — a
    // repeated variable in one pattern is one name either way
    // (LABS-348: the occurrence-counting form miscounted repeated names).
    let a: HashSet<&String> = path_pattern_vars(&patterns[0]).collect();
    let b: HashSet<&String> = path_pattern_vars(&patterns[1]).collect();
    let shared: Vec<&String> = a.intersection(&b).copied().collect();
    if shared.len() == 1 && is_shared_node_var(patterns, shared[0].as_str()) {
        Some(shared[0].as_str())
    } else {
        None
    }
}

/// The name is a node variable of every pattern (the hub must be a
/// shared end, not a shared rel).
fn is_shared_node_var(patterns: &[PathPattern], name: &str) -> bool {
    patterns
        .iter()
        .all(|p| p.nodes.iter().any(|n| n.variable.as_deref() == Some(name)))
}

/// LABS-348's join: each side enumerates once from the base row (its own
/// start selection already optimized by the ADR-0351 ladder), rows group
/// by hub id, and each shared hub emits the cross product of the sides'
/// binding rows — the per-side rows are the payloads. Relationship
/// uniqueness is preserved by skipping pairs whose used-rel sets overlap;
/// the statement residual is applied to every merged row (cross-pattern
/// predicates survive the join).
/// The hub join's outcome: the pair joined (`Joined`, the final rows),
/// or the shape declined with side A already enumerated — its rows and
/// used-rel sets are exactly what the nested loop's pattern-0 iteration
/// would produce (same empty seed used-set, same residual, no
/// cap/reduce, keep_used), so `match_paths` seeds its loop with them
/// instead of paying the side-A expansion twice (LABS-348).
enum HubJoin {
    /// The pair joined; these are the final rows.
    Joined(Vec<Row>),
    /// The shape never applied (the cheap predicate declined before any
    /// enumeration) — the nested loop runs from its own base row.
    DeclinedWithoutSeed,
    /// The shape applied but side A fell below the gate; its already-paid
    /// enumeration seeds the nested loop (no second pattern-0 pass).
    Seeded(Matched),
}

fn try_hub_join(
    ctx: ExecCtx<'_>,
    patterns: &[PathPattern],
    match_mode: MatchMode,
    row: &Row,
    residual: &Residual<'_>,
    plain_rows_only: bool,
) -> Result<HubJoin> {
    // The decline path (the overwhelmingly common one) pays a borrow, not
    // a row clone: the cheap shape checks run before the two clones the
    // join itself needs (LABS-348).
    #[cfg(test)]
    if HUB_JOIN_DISABLED.with(std::cell::Cell::get) {
        return Ok(HubJoin::DeclinedWithoutSeed);
    }
    // Bind once, clone after (LABS-348): the predicate's
    // Some payload is the hub var itself, and the row clone stays
    // behind the gate — the decline path (the common one) pays one
    // predicate call and a borrow, nothing else.
    let Some(hub_var) = hub_join_applies(patterns, match_mode, row, plain_rows_only) else {
        return Ok(HubJoin::DeclinedWithoutSeed);
    };
    let row = row.clone();
    let a_rows = plain_side_rows(ctx, &patterns[0], row.clone(), match_mode, residual)?;
    if a_rows.len() < HUB_JOIN_MIN_SIDE_ROWS {
        // The nested loop is already cheap at this side-A size; the join
        // bookkeeping would be pure overhead (the measured single-left
        // case joined as cheaply as the nested loop). The already-paid
        // side-A expansion seeds it (no second expansion).
        return Ok(HubJoin::Seeded(a_rows));
    }
    // Only side A is grouped — the join drives off side B's rows
    // directly (LABS-348). The emit order is side B's enumeration order outer and
    // the per-hub side-A binding order inner, both deterministic because
    // `match_one_path` enumerates deterministically (LABS-348).
    let a_by_hub = group_by_hub(a_rows, hub_var);
    let b_rows = plain_side_rows(ctx, &patterns[1], row, match_mode, residual)?;
    #[cfg(test)]
    HUB_JOIN_ENGAGED.with(|c| c.set(c.get() + 1));
    let mut out = Vec::new();
    for (b_row, b_used) in &b_rows {
        let hub = row_node_id(b_row, hub_var);
        let Some(a_side) = a_by_hub.get(&hub) else {
            continue;
        };
        for (a_row, a_used) in a_side {
            ctx.graph.cancel_tick()?;
            if !a_used.is_disjoint(b_used) {
                // Relationship uniqueness across the comma pair: the
                // same rel cannot serve both patterns.
                continue;
            }
            let mut merged = a_row.clone();
            for (k, v) in b_row.iter() {
                merged.insert(k.clone(), v.clone());
            }
            if !residual_ok(ctx.graph, &merged, ctx.params, ctx.cypher_version, residual)? {
                continue;
            }
            out.push(merged);
        }
    }
    Ok(HubJoin::Joined(out))
}

/// Group one side's rows by hub id (LABS-348). Keyed by hub id for the lookup;
/// the emit order comes from the driving side's enumeration, not the
/// grouped map's (its ordering is unconsumed; only the hub lookup reads it).
fn group_by_hub(rows: Matched, hub_var: &str) -> BTreeMap<NodeId, Matched> {
    let mut by_hub: BTreeMap<NodeId, Matched> = BTreeMap::new();
    for (r, used) in rows {
        let id = row_node_id(&r, hub_var);
        by_hub.entry(id).or_default().push((r, used));
    }
    by_hub
}

/// One plain-side enumeration for the join's sides: the pattern from a
/// base row, a fresh used-set, no cap, no reduce, keep the set. Both
/// sides enumerate identically (LABS-348) — one spelling of the
/// plain-side policy, not one per side.
fn plain_side_rows(
    ctx: ExecCtx<'_>,
    pattern: &PathPattern,
    row: Row,
    match_mode: MatchMode,
    residual: &Residual<'_>,
) -> Result<Matched> {
    match_one_path(
        ctx,
        pattern,
        row,
        HashSet::new(),
        match_mode,
        residual,
        None,
        None,
        true,
    )
}

fn row_node_id(row: &Row, var: &str) -> NodeId {
    match row.get(var) {
        Some(Value::Node(n)) => n.id,
        _ => unreachable!("a bound hub variable always holds a node"),
    }
}

pub(crate) fn match_paths(
    ctx: ExecCtx<'_>,
    patterns: &[PathPattern],
    match_mode: MatchMode,
    row: Row,
    residual: &Residual<'_>,
    mut cap: Option<&mut MatchCap<'_>>,
    reduce: Option<&MatchReduce<'_>>,
) -> Result<Vec<Row>> {
    // LABS-348: the two-pattern comma join through one shared hub joins
    // by intersecting at the hub instead of re-expanding pattern 2 for
    // every pattern-1 row (the nested loop re-applies the side filter
    // per row: the measured adversarial shape paid 25M store touches
    // for 250k result rows). `None` keeps the nested loop below.
    // The decline carries the already-enumerated side-A rows (or an
    // empty seed when the shape never applied); they are exactly the
    // nested loop's pattern-0 output, so the loop starts at pattern 1
    // with them as its current rows instead of re-expanding pattern 0.
    let seeded = match try_hub_join(
        ctx,
        patterns,
        match_mode,
        &row,
        residual,
        cap.is_none() && reduce.is_none(),
    )? {
        HubJoin::Joined(rows) => return Ok(rows),
        HubJoin::DeclinedWithoutSeed => None,
        HubJoin::Seeded(rows) => Some(rows),
    };
    let (mut current, first) = match seeded {
        Some(rows) => (rows, 1),
        None => (vec![(row, HashSet::<RelId>::new())], 0),
    };
    let account = cypherlite_core::tokens::workspace_accounting();
    let mut batch = cypherlite_core::tokens::WorkspaceCharge::retain_with(|| {
        crate::workspace::binding_bytes(&current)
    })?;
    let last = patterns.len().saturating_sub(1);
    for (pi, p) in patterns.iter().enumerate().skip(first) {
        let mut next = Vec::new();
        let mut next_charge = cypherlite_core::tokens::WorkspaceCharge::empty();
        let mut next_meter = crate::workspace::BindingMeter::default();
        // Draining `current` keeps its allocation until the loop ends.
        // `batch` stays reserved across that overlap and is released when
        // `next_charge` replaces it, after `current` has been dropped.
        let last_pat = pi == last;
        for (r, used) in current {
            ctx.graph.cancel_tick()?;
            if last_pat && cap.as_ref().is_some_and(|c| c.full()) {
                break;
            }
            let prior_used = used.clone();
            let seed_used =
                if match_mode == MatchMode::RepeatableElements || p.is_shortest_path_function {
                    HashSet::new()
                } else {
                    used
                };
            let mut matched = match_one_path(
                ctx,
                p,
                r,
                seed_used,
                match_mode,
                residual,
                if last_pat { cap.as_deref_mut() } else { None },
                if last_pat { reduce } else { None },
                !last_pat,
            )?;
            if p.is_shortest_path_function && !last_pat {
                for (_, used) in &mut matched {
                    *used = prior_used.clone();
                }
            }
            if account {
                for (row, used) in &matched {
                    next_meter.push(row, used.len());
                }
                next_charge.resize(next_meter.bytes)?;
                cypherlite_core::tokens::peak_hold("expand");
            }
            next.extend(matched);
        }
        if match_mode == MatchMode::RepeatableElements {
            for (_, used) in &mut next {
                used.clear();
            }
            if account {
                next_charge.resize_with(|| crate::workspace::binding_bytes(&next))?;
                cypherlite_core::tokens::peak_hold("expand");
            }
        }
        current = next;
        batch = next_charge;
        if current.is_empty() {
            return Ok(Vec::new());
        }
    }
    drop(batch);
    Ok(current.into_iter().map(|(r, _)| r).collect())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn match_one_path(
    ctx: ExecCtx<'_>,
    path: &PathPattern,
    row: Row,
    used: HashSet<RelId>,
    match_mode: MatchMode,
    residual: &Residual<'_>,
    cap: Option<&mut MatchCap<'_>>,
    reduce: Option<&MatchReduce<'_>>,
    keep_used: bool,
) -> Result<Matched> {
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    if match_mode == MatchMode::RepeatableElements || path.needs_gpm() {
        return match_gpm_path(ctx, path, row, used, match_mode, reduce, keep_used);
    }
    let mut out = Vec::new();
    let mut sink = CollectSink {
        out: &mut out,
        cap,
        reduce,
        keep_used,
        charge: cypherlite_core::tokens::WorkspaceCharge::empty(),
        meter: crate::workspace::BindingMeter::default(),
    };
    // LABS-161: an elementId()/id() equality (or a row-bound variable) on
    // the first rel seeks the rel directly instead of scanning every
    // node's adjacency; a Gone seek matches nothing (zero rows).
    match_resolved_path(
        graph,
        path,
        params,
        cypher_version,
        residual,
        row,
        used,
        &mut sink,
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::duration;
    use super::*;

    // ADR-0205: horizontal aggregates skip nulls, sum() over zero non-null
    // values is 0, and sum()/avg() accept durations.
    #[test]
    fn horizontal_sum_and_avg_skip_nulls() {
        let vals = vec![Value::Int(1), Value::Null, Value::Int(3)];
        assert_eq!(
            horizontal_aggregate("sum", vals.clone()).unwrap(),
            Value::Int(4)
        );
        assert_eq!(
            horizontal_aggregate("avg", vals).unwrap(),
            Value::Float(2.0)
        );
        assert_eq!(
            horizontal_aggregate("sum", vec![Value::Null, Value::Null]).unwrap(),
            Value::Int(0)
        );
        assert_eq!(
            horizontal_aggregate("avg", vec![Value::Null]).unwrap(),
            Value::Null
        );
        assert_eq!(
            horizontal_aggregate("collect", vec![Value::Int(1), Value::Null]).unwrap(),
            Value::List(vec![Value::Int(1)].into())
        );
        assert_eq!(
            horizontal_aggregate("count", vec![Value::Null, Value::Int(1)]).unwrap(),
            Value::Int(1)
        );
    }

    #[test]
    fn horizontal_sum_and_avg_accept_durations() {
        let vals = vec![
            Value::Null,
            duration(0, 0, 1_000_000_000),
            duration(0, 0, 3_000_000_000),
        ];
        assert_eq!(
            horizontal_aggregate("sum", vals.clone()).unwrap(),
            duration(0, 0, 4_000_000_000)
        );
        assert_eq!(
            horizontal_aggregate("avg", vals).unwrap(),
            duration(0, 0, 2_000_000_000)
        );
        assert!(
            horizontal_aggregate("sum", vec![duration(0, 0, 1), Value::Int(1)]).is_err(),
            "mixing durations and numbers is a type error"
        );
    }

    #[test]
    fn horizontal_aggregate_unknown_name_errors_instead_of_summing() {
        let error = horizontal_aggregate("percentile", vec![Value::Int(1)]).unwrap_err();
        assert!(
            matches!(error, Error::Internal(_)),
            "an unknown aggregate name is an internal error, not a silent sum: {error}"
        );
        assert!(
            error.to_string().contains("percentile"),
            "the error names the offending aggregate: {error}"
        );
    }
}
