//! Pattern predicates/comprehensions: walk the pattern, collecting rows.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use cypherlite_core::{Error, NodeId, RelId, Result, Value};
use cypherlite_ir::{Expr, NodePattern, PathPattern, RelPattern};

use crate::ctx::{Binding, ExecCtx, PathRow, VarWalk};
use crate::overlay::ExecGraph;

use super::labels::label_expr_matches;
use super::{eval, Row};

/// The row sink `walk_pattern`/`walk_rel` share: the collected rows plus
/// an optional cap. `full()` gates every recursion level, so a cap of 1
/// is LABS-246's limit-1 existence walk — it stops at the first full
/// match; `None` collects all matches. (Bundled to stay under
/// `clippy::too_many_arguments`.)
pub(crate) struct Collect<'a> {
    limit: Option<usize>,
    out: &'a mut Vec<Row>,
}

impl Collect<'_> {
    fn full(&self) -> bool {
        self.limit.is_some_and(|l| self.out.len() >= l)
    }
}

/// The walk setup `collect_pattern_rows` and `pattern_exists` share:
/// `limit` is the only difference — `None` collects every match, `Some(n)`
/// stops at the first n (LABS-246's existence walk passes `Some(1)`).
fn walk_collect(
    graph: &ExecGraph<'_>,
    row: &Row,
    path: &PathPattern,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    limit: Option<usize>,
) -> Result<Vec<Row>> {
    if path.nodes.is_empty() {
        return Ok(vec![row.clone()]);
    }
    let mut out = Vec::new();
    walk_pattern(
        ExecCtx::new(graph, params, cypher_version),
        path,
        0,
        row.clone(),
        HashSet::new(),
        None,
        &mut Collect {
            limit,
            out: &mut out,
        },
    )?;
    Ok(out)
}

pub(crate) fn collect_pattern_rows(
    graph: &ExecGraph<'_>,
    row: &Row,
    path: &PathPattern,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Vec<Row>> {
    walk_collect(graph, row, path, params, cypher_version, None)
}

/// LABS-246: boolean existence for a pattern predicate — the anchored
/// probe ([`resolved_endpoints_exist`], LABS-193/LABS-249) first, then a
/// limit-1 walk that stops at the first full match for every shape the
/// probe rejects (multi-hop, var-length, named path, GPM-free or not,
/// pre-bound rel, no row-bound endpoint). Existence never materializes
/// more than one row; [`collect_pattern_rows`] is for consumers that
/// need the matches themselves.
pub(crate) fn pattern_exists(
    graph: &ExecGraph<'_>,
    row: &Row,
    path: &PathPattern,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<bool> {
    if let Some(hit) = resolved_endpoints_exist(graph, row, path, params, cypher_version)? {
        return Ok(hit);
    }
    if path.nodes.is_empty() {
        return Ok(true);
    }
    Ok(!walk_collect(graph, row, path, params, cypher_version, Some(1))?.is_empty())
}

/// The rel-edge checks `walk_rel` and the existence probe share: the
/// pattern's type-expression and property constraints for `rid`.
pub(crate) fn rel_edge_ok(
    graph: &ExecGraph<'_>,
    rid: RelId,
    rp: &RelPattern,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<bool> {
    if let Some(type_expr) = &rp.type_expr {
        let Some(rel) = graph.rel_stub(rid) else {
            return Ok(false);
        };
        if !label_expr_matches(
            graph,
            row,
            params,
            type_expr,
            std::slice::from_ref(&rel.rel_type),
            true,
            cypher_version,
        )? {
            return Ok(false);
        }
    }
    if rp.properties.is_some() {
        let Some(rel) = graph.rel(rid) else {
            return Ok(false);
        };
        if !rel_props_ok(graph, &rel, rp, row, params, cypher_version) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// LABS-193/LABS-249: existence probe for a pattern predicate (or an
/// EXISTS { MATCH } body) with at least one row-bound endpoint: anchors
/// on the bound side(s) and walks only the smaller side's adjacency
/// (approximate degree, [`ExecGraph::adj_len`]) of the rel's direction
/// and types — a free endpoint is checked per reached node, never
/// enumerated (enumerating it cost a full label scan with per-node
/// materialization per row, LABS-249). Materializes no rows, stops at
/// the first matching edge. `Ok(None)` = shape not eligible (no
/// row-bound endpoint, a named path variable, a pre-bound rel variable,
/// or anything but one 1-hop rel); the caller falls back to the limit-1
/// walk ([`walk_collect`] with `Some(1)`). An endpoint bound to a null/non-node makes
/// the predicate answer `Ok(Some(false))` directly — the walked path's
/// zero-row semantics, NOT a deferral.
pub(crate) fn resolved_endpoints_exist(
    graph: &ExecGraph<'_>,
    row: &Row,
    path: &PathPattern,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Option<bool>> {
    match probe_plan(graph, row, path) {
        ProbePlan::Ineligible => Ok(None),
        ProbePlan::NoMatch => Ok(Some(false)),
        ProbePlan::Anchored(anchor) => {
            anchored_exists(graph, row, path, params, cypher_version, anchor).map(Some)
        }
    }
}

/// The pattern-check seats, paired with the row each is checked against:
/// the walk checks `nodes[0]`'s pattern before the rel binds and
/// `nodes[1]`'s after bind_trail — positionally, whichever side the
/// probe walks from.
enum Seats {
    /// The anchor fills `nodes[0]`'s seat; `nodes[1]`'s seat is checked
    /// per reached node with the rel bound.
    Forward,
    /// The anchor fills `nodes[1]`'s seat (labels only — the seat guard
    /// bails on any properties); `nodes[0]`'s seat is checked per
    /// reached node with the pre-rel row.
    Flipped,
}

/// The decided probe: the walked adjacency (from/to/dir) plus the seat
/// pairing the orientation match produced, not re-derived below.
enum ProbePlan {
    Ineligible,
    NoMatch,
    Anchored(AnchoredPlan),
}

/// The decided probe, bundled: the walked adjacency plus the seat pairing
/// the orientation match produced.
struct AnchoredPlan {
    from: NodeId,
    to: Option<NodeId>,
    dir: cypherlite_ir::Direction,
    seats: Seats,
}

/// Shape gate, endpoint classification, seat guards, and orientation —
/// everything before the adjacency walk, in one place.
fn probe_plan(graph: &ExecGraph<'_>, row: &Row, path: &PathPattern) -> ProbePlan {
    if path.variable.is_some()
        || path.nodes.len() != 2
        || path.rels.len() != 1
        || path.rels[0].var_length
        // GPM shapes (element predicates, selectors) take the walked path
        || path.needs_gpm()
    {
        return ProbePlan::Ineligible;
    }
    let rp = &path.rels[0];
    // A pre-bound rel variable is rebound by the walked path's bind_trail
    // before the destination's node checks run — not equivalent; walk it.
    if let Some(v) = &rp.variable {
        if row.get(v).is_some() {
            return ProbePlan::Ineligible;
        }
    }
    let binding = |np: &cypherlite_ir::NodePattern| crate::ctx::binding(np, row);
    let e0 = binding(&path.nodes[0]);
    let e1 = binding(&path.nodes[1]);
    // A variable bound to null (or any non-node) matches nothing: the
    // predicate is false (NOT EXISTS negates it) — the walked path's
    // zero-row semantics.
    if matches!(e0, Binding::Mismatch) || matches!(e1, Binding::Mismatch) {
        return ProbePlan::NoMatch;
    }
    // Seat guards: the walk binds each endpoint's variable in seat order;
    // an unbound endpoint's variable binds never here — bail when any
    // property map could reference across that order.
    let unbound0 = matches!(e0, Binding::Unbound);
    let unbound1 = matches!(e1, Binding::Unbound);
    if (unbound0
        && (rp.properties.is_some()
            || rp.type_expr.is_some()
            || path.nodes[0].properties.is_some()
            || path.nodes[1].properties.is_some()))
        || (unbound1 && path.nodes[1].properties.is_some())
    {
        return ProbePlan::Ineligible;
    }
    // Orientation decides anchor, flip, and seat pairing together.
    match (e0, e1) {
        (Binding::Node(a), Binding::Node(b)) => {
            // Both bound: check the direct edge from the smaller side.
            if graph.adj_len(a, rp.direction) <= graph.adj_len(b, rp.direction.flip()) {
                ProbePlan::Anchored(AnchoredPlan {
                    from: a,
                    to: Some(b),
                    dir: rp.direction,
                    seats: Seats::Forward,
                })
            } else {
                ProbePlan::Anchored(AnchoredPlan {
                    from: b,
                    to: Some(a),
                    dir: rp.direction.flip(),
                    seats: Seats::Flipped,
                })
            }
        }
        (Binding::Node(a), Binding::Anonymous) => ProbePlan::Anchored(AnchoredPlan {
            from: a,
            to: None,
            dir: rp.direction,
            seats: Seats::Forward,
        }),
        (Binding::Anonymous, Binding::Node(b)) => ProbePlan::Anchored(AnchoredPlan {
            from: b,
            to: None,
            dir: rp.direction.flip(),
            seats: Seats::Flipped,
        }),
        _ => ProbePlan::Ineligible,
    }
}

/// The anchored adjacency walk. Pattern seats stay positional: the
/// anchor-seat pattern against the pre-rel row, the far-seat pattern per
/// reached node with the rel bound (bind_trail's order).
fn anchored_exists(
    graph: &ExecGraph<'_>,
    row: &Row,
    path: &PathPattern,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    anchor: AnchoredPlan,
) -> Result<bool> {
    let AnchoredPlan {
        from,
        to,
        dir,
        seats,
    } = anchor;
    let rp = &path.rels[0];
    // The anchor's seat: its own pattern, pre-rel row.
    let (anchor_np, far_np) = match seats {
        Seats::Forward => (&path.nodes[0], &path.nodes[1]),
        Seats::Flipped => (&path.nodes[1], &path.nodes[0]),
    };
    if !pattern_node_ok(graph, from, anchor_np, row, params, cypher_version) {
        return Ok(false);
    }
    // Single-rel shape: walk_rel's used/trail rel-dedup has nothing to
    // guard here.
    let found = graph.try_find_neighbor::<cypherlite_core::Error>(from, dir, &rp.types, |rid| {
        graph.cancel_tick()?;
        if !rel_edge_ok(graph, rid, rp, row, params, cypher_version)? {
            return Ok(false);
        }
        let Some(reached) = graph.hop(from, rid, dir) else {
            return Ok(false);
        };
        if to.is_some_and(|to| reached != to) {
            return Ok(false);
        }
        let mut row2 = row.clone();
        // The shape gate excludes pre-bound rel variables, so the
        // reconciliation never fails here; the check stays for uniformity.
        if !bind_trail(&mut row2, rp, std::slice::from_ref(&rid), graph, 0, false) {
            return Ok(false);
        }
        // The far seat: positionally checked — with the rel bound when it
        // is `nodes[1]`'s pattern, with the pre-rel row when flipped
        // (labels only there; the seat guard bailed on any properties).
        let far_row = match seats {
            Seats::Forward => &row2,
            Seats::Flipped => row,
        };
        Ok(pattern_node_ok(
            graph,
            reached,
            far_np,
            far_row,
            params,
            cypher_version,
        ))
    })?;
    Ok(found.is_some())
}

/// `collect` carries the row sink and its cap: once `collect.full()`
/// every recursion level returns without further work (LABS-246's
/// limit-1 existence walk).
pub(crate) fn walk_pattern(
    ctx: ExecCtx<'_>,
    path: &PathPattern,
    node_idx: usize,
    row: Row,
    used: HashSet<RelId>,
    forced: Option<NodeId>,
    collect: &mut Collect<'_>,
) -> Result<()> {
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    let np = &path.nodes[node_idx];
    // LABS-448: the seek phase — resolving the node candidates, the store
    // work behind the `Nodes` kind — through the one wrapper every walker
    // shares. A non-PROFILEd walk pays the thread-local read and nothing
    // else.
    let candidates = if let Some(id) = forced {
        vec![id]
    } else {
        crate::exec::phase_seek(|| {
            pattern_node_candidates(graph, np, &row, params, cypher_version)
        })?
    };
    for id in candidates {
        if collect.full() {
            return Ok(());
        }
        graph.cancel_tick()?;
        if !pattern_node_ok(graph, id, np, &row, params, cypher_version) {
            continue;
        }
        let mut row2 = row.clone();
        if let Some(v) = &np.variable {
            match row2.get(v) {
                Some(Value::Node(n)) if n.id == id => {}
                None => {
                    row2.insert(v.clone(), graph.node_value(id));
                }
                // null and non-node bindings match nothing: a pattern
                // carrying them stops expanding (zero rows)
                _ => continue,
            }
        }
        if node_idx >= path.rels.len() {
            bind_eval_path(graph, path, &mut row2);
            collect.out.push(row2);
            continue;
        }
        let rp = &path.rels[node_idx];
        let (min_h, max_h) = crate::ctx::hop_bounds(rp);
        // LABS-448: the walk phase — following relationships, the store
        // work behind the `Rels`/`Degree` kinds — through the one wrapper,
        // inclusive of the nested phases the recursion performs.
        crate::exec::phase_walk(|| {
            walk_rel(
                ctx,
                path,
                node_idx,
                id,
                PathRow {
                    row: row2,
                    used: used.clone(),
                },
                VarWalk {
                    min_h,
                    max_h,
                    hops: 0,
                    trail: Vec::new(),
                },
                collect,
            )
        })?;
    }
    Ok(())
}

pub(crate) fn walk_rel(
    ctx: ExecCtx<'_>,
    path: &PathPattern,
    rel_idx: usize,
    current: NodeId,
    state: PathRow,
    vw: VarWalk,
    collect: &mut Collect<'_>,
) -> Result<()> {
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    let PathRow { row, used } = state;
    let VarWalk {
        min_h,
        max_h,
        hops,
        trail,
    } = vw;
    let rp = &path.rels[rel_idx];
    if hops >= min_h && hops <= max_h {
        let mut row2 = row.clone();
        let mut used2 = used.clone();
        if !bind_trail(
            &mut row2,
            rp,
            &trail,
            graph,
            rel_idx,
            path.variable.is_some(),
        ) {
            return Ok(());
        }
        for id in &trail {
            used2.insert(*id);
        }
        walk_pattern(ctx, path, rel_idx + 1, row2, used2, Some(current), collect)?;
    }
    if hops >= max_h || collect.full() {
        return Ok(());
    }
    // try_find_neighbor, not try_for_each_neighbor: the recursion that
    // fills the sink answers Ok(true) on the way out (below), so a limit-1
    // existence walk does not even visit the edges past the match. Without
    // a limit the closure never answers true — the walk is the old
    // exhaustive one.
    graph.try_find_neighbor::<Error>(current, rp.direction, &rp.types, |rid| {
        graph.cancel_tick()?;
        if used.contains(&rid) || trail.contains(&rid) {
            return Ok(false);
        }
        if !rel_edge_ok(graph, rid, rp, &row, params, cypher_version)? {
            return Ok(false);
        }
        let Some(next) = graph.hop(current, rid, rp.direction) else {
            return Ok(false);
        };
        let mut trail2 = trail.clone();
        trail2.push(rid);
        walk_rel(
            ctx,
            path,
            rel_idx,
            next,
            PathRow {
                row: row.clone(),
                used: used.clone(),
            },
            VarWalk {
                min_h,
                max_h,
                hops: hops + 1,
                trail: trail2,
            },
            collect,
        )?;
        // The recursion filled the sink: stop the neighbor loop now
        // (Ok(true) is a stop sentinel — the found rel is discarded).
        Ok(collect.full())
    })?;
    Ok(())
}

pub(crate) fn rel_props_ok(
    graph: &ExecGraph<'_>,
    rel: &cypherlite_core::Relationship,
    rp: &RelPattern,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> bool {
    let Ok(want) = pattern_prop_map(graph, row, rp.properties.as_ref(), params, cypher_version)
    else {
        return false;
    };
    for (k, v) in want {
        match rel.properties.get(&k) {
            Some(have) if have.eq_non_null(&v) => {}
            _ => return false,
        }
    }
    true
}

/// Binds the trail to the pattern's rel variable (or the anonymous
/// `__trail` slot when `keep_anon`). Returns false when a pre-bound rel
/// variable holds a different relationship than the trail's single hop —
/// the path does not match; true otherwise.
pub(crate) fn bind_trail(
    row: &mut Row,
    rp: &RelPattern,
    trail: &[RelId],
    graph: &ExecGraph<'_>,
    rel_idx: usize,
    keep_anon: bool,
) -> bool {
    let stub = |id: RelId| crate::ctx::bind_rel_stub(graph, id);
    if let Some(v) = &rp.variable {
        if rp.var_length || trail.len() != 1 {
            row.insert(
                v.clone(),
                Value::List(trail.iter().copied().map(stub).collect::<Vec<_>>().into()),
            );
        } else if let Some(&rid) = trail.first() {
            // Pre-bound reconciliation: an existing binding must name the
            // same relationship.
            if let Some(Value::Relationship(r)) = row.get(v) {
                if r.id != rid {
                    return false;
                }
            }
            row.insert(v.clone(), stub(rid));
        }
    } else if keep_anon && !trail.is_empty() {
        row.insert(
            format!("__trail{rel_idx}"),
            if trail.len() == 1 {
                stub(trail[0])
            } else {
                Value::List(trail.iter().copied().map(stub).collect::<Vec<_>>().into())
            },
        );
    }
    true
}

pub(crate) fn bind_eval_path(graph: &ExecGraph<'_>, path: &PathPattern, row: &mut Row) {
    let Some(name) = &path.variable else {
        return;
    };
    let mut nodes = Vec::new();
    let mut rels = Vec::new();
    if let Some(v) = path.nodes.first().and_then(|n| n.variable.as_ref()) {
        if let Some(Value::Node(n)) = row.get(v) {
            nodes.push(graph.node(n.id).unwrap_or_else(|| (**n).clone()));
        }
    }
    // The Relationship/List chunk extraction shared by the named rel
    // variable and the `__trail{i}` fallback.
    let rel_chunk = |binding: Option<&Value>| -> Vec<cypherlite_core::Relationship> {
        match binding {
            Some(Value::Relationship(r)) => vec![(**r).clone()],
            Some(Value::List(xs)) => xs
                .iter()
                .filter_map(|v| match v {
                    Value::Relationship(r) => Some((**r).clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    };
    for (i, rp) in path.rels.iter().enumerate() {
        let chunk: Vec<_> = match rp.variable.as_ref().and_then(|v| row.get(v)) {
            Some(_) => rel_chunk(rp.variable.as_ref().and_then(|v| row.get(v))),
            None => rel_chunk(row.get(&format!("__trail{i}"))),
        };
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
    }
    row.insert(
        name.clone(),
        Value::from_path(cypherlite_core::Path { nodes, rels }),
    );
}

/// The NodeId `np`'s variable is bound to in `row`, if any: the one
/// candidate a bound variable leaves `pattern_node_candidates`.
pub(crate) fn bound_node(np: &cypherlite_ir::NodePattern, row: &Row) -> Option<NodeId> {
    match crate::ctx::binding(np, row) {
        crate::ctx::Binding::Node(id) => Some(id),
        _ => None,
    }
}

/// The eval walk engine's node candidates — delegated to the one counted
/// funnel ([`crate::exec::candidates::node_candidates`]) with an empty
/// residual, exactly as the agg fold reads it: the eval paths (pattern
/// comprehensions, EXISTS predicates, expression-form `shortestPath`) then
/// count their node-store reads in PROFILE `Db Hits` like every other
/// match, instead of enumerating past the counter (LABS-375).
pub(crate) fn pattern_node_candidates(
    graph: &ExecGraph<'_>,
    np: &cypherlite_ir::NodePattern,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Vec<NodeId>> {
    crate::exec::candidates::node_candidates(
        graph,
        np,
        row,
        params,
        cypher_version,
        &crate::exec::pushdown::Residual::empty(),
    )
}

pub(crate) fn pattern_prop_map(
    graph: &ExecGraph<'_>,
    row: &Row,
    properties: Option<&Expr>,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<BTreeMap<String, Value>> {
    let Some(e) = properties else {
        return Ok(BTreeMap::new());
    };
    match eval(graph, row, e, params, cypher_version)? {
        Value::Map(m) => Ok(Arc::unwrap_or_clone(m)
            .into_iter()
            .filter(|(_, v)| !v.is_null())
            .collect()),
        Value::Null => Ok(BTreeMap::new()),
        // a non-map constraint would silently match everything
        _ => Err(Error::ty("node pattern properties must be a map")),
    }
}

pub(crate) fn pattern_node_ok(
    graph: &ExecGraph<'_>,
    id: NodeId,
    np: &cypherlite_ir::NodePattern,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> bool {
    let Some(n) = graph.node(id) else {
        return false;
    };
    if let Some(label_expr) = &np.label_expr {
        if !label_expr_matches(
            graph,
            row,
            params,
            label_expr,
            &n.labels,
            false,
            cypher_version,
        )
        .unwrap_or(false)
        {
            return false;
        }
    }
    let Ok(want) = pattern_prop_map(graph, row, np.properties.as_ref(), params, cypher_version)
    else {
        return false;
    };
    for (k, v) in want {
        match n.properties.get(&k) {
            Some(have) if have.eq_non_null(&v) => {}
            _ => return false,
        }
    }
    true
}

/// The one node-binding policy, shared by the forward walk, the backward
/// walker, and the cheapest start: reconcile an existing binding of the
/// element's variable (a matching node is kept, anything else — null and
/// non-node bindings included — fails this candidate), else bind the node
/// stub; an anonymous element of a named path binds its `__node{idx}`
/// stub. `false` means this candidate is rejected.
pub(crate) fn bind_pattern_node(
    graph: &ExecGraph<'_>,
    row: &mut Row,
    np: &NodePattern,
    id: NodeId,
    idx: usize,
    named: bool,
) -> bool {
    if let Some(v) = &np.variable {
        match row.get(v) {
            Some(Value::Node(n)) if n.id == id => true,
            None => {
                row.insert(v.clone(), crate::ctx::bind_node_stub(graph, id));
                true
            }
            // null and non-node bindings match nothing: a pattern
            // carrying them stops expanding (zero rows)
            _ => false,
        }
    } else if named {
        row.insert(
            format!("__node{idx}"),
            crate::ctx::bind_node_stub(graph, id),
        );
        true
    } else {
        true
    }
}
