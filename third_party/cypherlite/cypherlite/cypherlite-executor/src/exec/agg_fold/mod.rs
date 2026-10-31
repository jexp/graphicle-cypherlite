//! One-hop aggregate folding (ADR-0036); the streaming accumulators
//! live in [`acc`].

mod acc;

#[cfg(test)]
use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{NodeId, RelId, Result, Value};
use cypherlite_ir::{Clause, Direction, Expr, OrderItem, PathPattern, Projection};
use indexmap::IndexMap;

use crate::ctx::{bind_node_stub, binding};
use crate::eval::{bind_trail, eval, expr_vars, is_agg_expr, Row};
use crate::group_key::GroupKey;
use crate::overlay::{ExecGraph, MaterializeCache};
pub(in crate::exec) use acc::StreamAgg;

/// Shared fold scaffolding: the derived group key beside the key values
/// it was built from, so a repeat of the same key values reuses both.
/// The path fold's per-node key cache (path.rs `FoldCaches.keys`) stores
/// it; the one-hop fold builds one inline through [`CachedKey::from_vals`].
#[derive(Clone)]
struct CachedKey {
    key: Vec<GroupKey>,
    vals: Vec<Value>,
}

impl CachedKey {
    fn from_vals(vals: Vec<Value>) -> Self {
        let key = vals.iter().map(GroupKey::new).collect();
        Self { key, vals }
    }
}

use super::candidates::{node_candidates, node_ok, resolve_rel_seek, SeekedRel};
use super::match_clause::eval_match_predicate;
use super::project::{skip_limit_count, sort_rows_by_order};
use super::pushdown::{flatten_and_owned, match_scope, Residual};
use super::scope::{has_star_item, is_internal_column, projection_collides_with_internal_names};
use super::QueryResult;

/// A residual the one-hop fold cannot decide — a named path, or a conjunct
/// whose scope variables are not on the input row or the hop — keeps the
/// full-match path.
pub(super) fn where_fold_ready(
    where_clause: Option<&Expr>,
    path: &PathPattern,
    rows: &[Row],
) -> bool {
    let Some(expr) = where_clause else {
        return true;
    };
    let hop = fold_hop_vars(path);
    if let Some(name) = &path.variable {
        let mut used = HashSet::new();
        expr_vars(expr, &mut used);
        if used.contains(name) {
            return false;
        }
    }
    let patterns = std::slice::from_ref(path);
    for row in rows {
        let scope = match_scope(row, patterns);
        let mut available = hop.clone();
        for key in row.keys() {
            if !is_internal_column(key) {
                available.insert(key.clone());
            }
        }
        for conjunct in flatten_and_owned(expr) {
            let mut vars = HashSet::new();
            expr_vars(&conjunct, &mut vars);
            vars.retain(|var| scope.contains(var));
            if vars.iter().any(|var| !available.contains(var)) {
                return false;
            }
        }
    }
    true
}

pub(crate) fn fold_hop_vars(path: &PathPattern) -> HashSet<String> {
    let mut v = HashSet::new();
    for n in &path.nodes {
        if let Some(name) = &n.variable {
            v.insert(name.clone());
        }
    }
    for r in &path.rels {
        if let Some(name) = &r.variable {
            v.insert(name.clone());
        }
    }
    v
}

pub(crate) fn expr_within(expr: &Expr, allowed: &HashSet<String>) -> bool {
    let mut used = HashSet::new();
    expr_vars(expr, &mut used);
    used.iter().all(|x| allowed.contains(x))
}

#[cfg(test)]
thread_local! {
    static PATH_FOLD_BINDINGS: Cell<u64> = const { Cell::new(0) };
}

/// Surviving bindings that took the multi-hop aggregate fold since the last take.
#[cfg(test)]
pub(crate) fn take_path_fold_bindings() -> u64 {
    PATH_FOLD_BINDINGS.with(|count| count.replace(0))
}

pub(super) fn note_path_binding() {
    #[cfg(test)]
    PATH_FOLD_BINDINGS.with(|count| count.set(count.get() + 1));
}

pub(super) struct FoldReturn<'a> {
    items: &'a [Projection],
    order: &'a [OrderItem],
    skip: Option<&'a Expr>,
    limit: Option<&'a Expr>,
}

/// Shared RETURN gate for both folds: no DISTINCT, no GROUP BY, no `RETURN *`,
/// at least one aggregate, and no user alias in the internal `__` namespace
/// (the reservation [`super::scope::projection_collides_with_internal_names`]
/// owns — the contract and its rationale live there).
pub(super) fn fold_return(suffix: &[Clause]) -> Option<FoldReturn<'_>> {
    let Clause::Return {
        distinct,
        group_by,
        items,
        order,
        skip,
        limit,
    } = suffix.first()?
    else {
        return None;
    };
    if *distinct
        || group_by.is_some()
        || has_star_item(items)
        || projection_collides_with_internal_names(items)
    {
        return None;
    }
    if !items.iter().any(|item| is_agg_expr(&item.expr)) {
        return None;
    }
    Some(FoldReturn {
        items,
        order,
        skip: skip.as_ref(),
        limit: limit.as_ref(),
    })
}

pub(super) struct FoldOutput<'a> {
    items: &'a [Projection],
    order: &'a [OrderItem],
    skip: Option<&'a Expr>,
    limit: Option<&'a Expr>,
    params: &'a BTreeMap<String, Value>,
    cypher_version: Option<u32>,
}

pub(super) fn finish_folded_rows(
    graph: &ExecGraph<'_>,
    output: &FoldOutput<'_>,
    mut out: Vec<Row>,
) -> Result<FoldedReturn> {
    let empty = Row::new();
    let skip_n = match output.skip {
        Some(expr) => skip_limit_count(&eval(
            graph,
            &empty,
            expr,
            output.params,
            output.cypher_version,
        )?)?,
        None => 0,
    };
    let limit_n = match output.limit {
        Some(expr) => Some(skip_limit_count(&eval(
            graph,
            &empty,
            expr,
            output.params,
            output.cypher_version,
        )?)?),
        None => None,
    };
    if !output.order.is_empty() {
        sort_rows_by_order(
            graph,
            output.items,
            output.params,
            output.order,
            &mut out,
            output.cypher_version,
        )?;
    }
    if skip_n > 0 {
        if skip_n >= out.len() {
            out.clear();
        } else {
            out = out.split_off(skip_n);
        }
    }
    if let Some(n) = limit_n {
        out.truncate(n);
    }
    Ok(folded_return(graph, out, output.items))
}

/// ADR-0036: fold one-hop MATCH + streamable RETURN aggregates (no row per edge).
/// A MATCH fold that already built the following RETURN.
pub(crate) struct FoldedReturn {
    pub rows: Vec<Row>,
    pub result: QueryResult,
    /// Clauses after the current MATCH that the fold consumed, including RETURN.
    pub consumed_after: usize,
}

/// The fold's seek outcome — its own vocabulary, exhaustive without the
/// walk-only [`RelSeek::Forced`]: an accepted fold hop is always anchored
/// (the row-bound start stays the anchor), so the fold never yields Forced.
enum FoldSeek {
    /// No seek applies (no rel variable, no equality conjunct, or a shape
    /// the seek does not serve) — the adjacency scan serves the row.
    Decline,
    /// Resolved but matches nothing (not visible, type-filtered, or not
    /// incident to the row-bound start) — the pattern matches nothing; the
    /// row is skipped, never scanned.
    Gone,
    /// The one anchored hop to offer.
    Anchored(NodeId, RelId),
}

/// LABS-161, the fold's twin of the walk's `first_rel_seek`: an
/// `elementId(r) = …` / `id(r) = …` conjunct (or a row-bound `r`) on the
/// one-hop pattern's rel variable resolves the single hop directly.
/// The decode core — row binding, `rel_seek_expr` over the conjuncts,
/// `rel_ends` visibility, the static type filter — is the shared
/// [`resolve_rel_seek`]; the fold-local policy is only the orientation
/// decline (an undirected pattern matches each rel in both orientations;
/// one anchor would drop the reverse hop, so the scan keeps serving it)
/// and the row-bound start incidence check. The WHERE clause is
/// re-evaluated on the bound row either way, so the seek narrows the
/// search, never the result.
fn fold_rel_seek(
    graph: &ExecGraph<'_>,
    path: &PathPattern,
    conjuncts: &[Expr],
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> FoldSeek {
    let rp = &path.rels[0];
    let (start, end, rid) =
        match resolve_rel_seek(graph, rp, conjuncts, row, params, cypher_version) {
            SeekedRel::Decline => return FoldSeek::Decline,
            SeekedRel::Gone => return FoldSeek::Gone,
            SeekedRel::Resolved { start, end, rid } => (start, end, rid),
        };
    // Pick the direction-oriented start of the rel; the undirected
    // decline lives here (one arm, decided once): an undirected pattern
    // matches each rel in both orientations, and one anchor would drop
    // the reverse hop, so the scan keeps serving it.
    let sid = match rp.direction {
        Direction::Outgoing => start,
        Direction::Incoming => end,
        Direction::Both => return FoldSeek::Decline,
    };
    // A row-bound start stays the anchor: the hop counts only when the
    // rel is incident to it. A null or non-node binding matches nothing,
    // like the scan's empty candidate set — one accept predicate,
    // `Binding::accepts`.
    if !binding(&path.nodes[0], row).accepts(sid) {
        return FoldSeek::Gone;
    }
    FoldSeek::Anchored(sid, rid)
}

/// Bind a pattern node seat's variable to its node stub, keeping an
/// existing binding (the seat's accept check guarantees it is this very
/// id) — the shape `match_node` spells for the walk, stated once for both
/// the fold's seats.
fn bind_node_seat(
    np: &cypherlite_ir::NodePattern,
    row: &mut Row,
    graph: &ExecGraph<'_>,
    id: NodeId,
) {
    if let Some(v) = &np.variable {
        row.entry(v.clone())
            .or_insert_with(|| bind_node_stub(graph, id));
    }
}

/// One one-hop-fold aggregate seat: the prototype paired with its
/// validated operand (`None` for `count(*)`). The pair stays together so
/// the offer site never zips two same-length vectors back by index.
type StreamSeat<'e> = (StreamAgg, Option<&'e Expr>);

/// Streamable prototypes for the aggregate half of the projection, in
/// item order; `None` when any aggregate item is not streamable — the
/// caller declines the fold in the same pass.
fn fresh_stream_aggs<'e>(agg_items: &[&'e Projection]) -> Option<Vec<StreamSeat<'e>>> {
    agg_items.iter().map(|p| StreamAgg::new(&p.expr)).collect()
}

pub(crate) fn try_fold_one_hop_aggs(
    graph: &ExecGraph<'_>,
    rows: &[Row],
    patterns: &[PathPattern],
    where_clause: Option<&Expr>,
    suffix: &[Clause],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Option<FoldedReturn>> {
    // CIP-236: an explicit GROUP BY supplies the grouping keys, so folding
    // by the non-aggregating items would build the wrong groups.
    let Some(ret) = fold_return(suffix) else {
        return Ok(None);
    };
    let items = ret.items;
    // The aggregate/non-aggregate classification of `items`, derived
    // once: the halves, the per-item ranks, and every use below
    // (the hop-vars guard, streamable prototype, group keys, per-item
    // offers, the output loop) all read it — no site re-classifies.
    let is_agg: Vec<bool> = items.iter().map(|p| is_agg_expr(&p.expr)).collect();
    // One pass splits the items into their non-aggregate (key) and
    // aggregate halves and records each item's rank within its half: the
    // output loop maps a column to its half and its position within that
    // half from the rank tables. Nothing re-derives the classification.
    let mut key_items: Vec<&Projection> = Vec::new();
    let mut agg_items: Vec<&Projection> = Vec::new();
    let mut key_rank = vec![0; items.len()];
    let mut agg_rank = vec![0; items.len()];
    for (i, p) in items.iter().enumerate() {
        if is_agg[i] {
            agg_rank[i] = agg_items.len();
            agg_items.push(p);
        } else {
            key_rank[i] = key_items.len();
            key_items.push(p);
        }
    }
    // Build the streamable-aggregate prototype in one pass; a
    // non-streamable aggregate declines the fold.
    let Some(proto_aggs) = fresh_stream_aggs(&agg_items) else {
        return Ok(None);
    };
    if patterns.len() != 1 || patterns[0].rels.len() != 1 {
        return Ok(None);
    }
    let path = &patterns[0];
    let rp = &path.rels[0];
    if path.needs_gpm()
        || rp.var_length
        || rp
            .type_expr
            .as_ref()
            .is_some_and(|e| !e.is_static_disjunction())
        || path.nodes.iter().any(|n| {
            n.label_expr
                .as_ref()
                .is_some_and(|e| !e.is_static_conjunction())
        })
    {
        return Ok(None);
    }
    let hop_vars = fold_hop_vars(path);
    for p in &key_items {
        if !expr_within(&p.expr, &hop_vars) {
            return Ok(None);
        }
    }
    if !where_fold_ready(where_clause, path, rows) {
        return Ok(None);
    }
    let output = FoldOutput {
        items,
        order: ret.order,
        skip: ret.skip,
        limit: ret.limit,
        params,
        cypher_version,
    };
    // The dispatcher tries the path fold (exec/agg_fold/path.rs) first: it
    // serves count, count-distinct, and single-node predicates; this
    // one-hop fold serves exactly what it declined. Sum, CASE, and
    // predicates that mention both ends stay here on the scratch loop.
    let mut groups: IndexMap<Vec<GroupKey>, (Vec<Value>, Vec<StreamSeat>)> = IndexMap::new();
    // One accepted hop (rel property filter, endpoint checks, the whole
    // WHERE re-evaluated on the bound row, then the aggregate offer) —
    // shared by the adjacency scan and the LABS-161 rel seek, which must
    // filter identically.
    let mut offer_hop = |base: &Row, rid: RelId, eid: NodeId| -> Result<()> {
        // The canonical rel-edge policy (type expression and properties,
        // eval/pattern.rs) — the seek must filter identically to the scan.
        if !crate::eval::rel_edge_ok(graph, rid, rp, base, params, cypher_version)? {
            return Ok(());
        }
        // A row-bound end must be this hop's far node; a null or
        // non-node binding matches nothing — the same accept predicate
        // as the fold's row-bound start check, `Binding::accepts`.
        if !binding(&path.nodes[1], base).accepts(eid) {
            return Ok(());
        }
        if !node_ok(graph, eid, &path.nodes[1], base, params, cypher_version)? {
            return Ok(());
        }
        let mut scratch = base.clone();
        // The canonical pre-bound rel-variable reconciliation
        // (`bind_trail`, eval/pattern.rs): a row-bound rel variable
        // accepts only its own id — a differing rid refuses the hop
        // instead of overwriting the binding.
        if !bind_trail(&mut scratch, rp, &[rid], graph, 0, false) {
            return Ok(());
        }
        bind_node_seat(&path.nodes[1], &mut scratch, graph, eid);
        if let Some(where_clause) = where_clause {
            if eval_match_predicate(graph, &scratch, where_clause, params, cypher_version)?.truthy()
                != Some(true)
            {
                return Ok(());
            }
        }
        let vals = key_items
            .iter()
            .map(|p| eval(graph, &scratch, &p.expr, params, cypher_version))
            .collect::<Result<Vec<_>>>()?;
        let CachedKey { key, vals } = CachedKey::from_vals(vals);
        let entry = groups
            .entry(key)
            .or_insert_with(|| (vals, proto_aggs.clone()));
        for (agg, operand) in &mut entry.1 {
            agg.offer(graph, &scratch, *operand, params, cypher_version)?;
        }
        Ok(())
    };
    // One accepted (start, rel) pair — the walk counter, the far node,
    // then the per-hop offer — shared by the adjacency scan (per
    // neighbor) and the LABS-161 rel seek (one pair per row).
    let mut accept = |base: &Row, sid: NodeId, rid: RelId| -> Result<()> {
        graph.cancel_tick()?;
        super::count_neighbor_walk();
        let Some(eid) = graph.hop(sid, rid, rp.direction) else {
            return Ok(());
        };
        offer_hop(base, rid, eid)
    };
    // The start-binding preamble both paths share: the start node's
    // label/property checks, then the row clone with the node stub
    // bound. `None` declines the start.
    let start_base = |row: &Row, sid: NodeId| -> Result<Option<Row>> {
        if !node_ok(graph, sid, &path.nodes[0], row, params, cypher_version)? {
            return Ok(None);
        }
        let mut base = row.clone();
        bind_node_seat(&path.nodes[0], &mut base, graph, sid);
        Ok(Some(base))
    };
    let conjuncts = where_clause.map(flatten_and_owned).unwrap_or_default();
    for row in rows {
        // LABS-161: an elementId()/id() equality on the rel variable (or
        // a row-bound rel) seeks the rel directly instead of scanning
        // every start node's adjacency per row.
        match fold_rel_seek(graph, path, &conjuncts, row, params, cypher_version) {
            FoldSeek::Decline => {}
            FoldSeek::Gone => continue,
            FoldSeek::Anchored(sid, rid) => {
                if let Some(base) = start_base(row, sid)? {
                    accept(&base, sid, rid)?;
                }
                continue;
            }
        }
        // LABS-448: the seek (the fold's pattern starts) then the walk
        // (the neighbor traversal each start feeds), one span per arm —
        // inclusive of the nested phases inside.
        let starts = super::phase_seek(|| {
            node_candidates(
                graph,
                &path.nodes[0],
                row,
                params,
                cypher_version,
                &Residual::empty(),
            )
        })?;
        for sid in starts {
            graph.cancel_tick()?;
            let Some(base) = start_base(row, sid)? else {
                continue;
            };
            super::phase_walk(|| {
                graph.try_for_each_neighbor(sid, rp.direction, &rp.types, |rid| {
                    accept(&base, sid, rid)
                })
            })?;
        }
    }
    // No accepted hop: `groups` stayed empty (the only inserts happen
    // inside the hop offer). All-key items answer from the empty group
    // derivation; aggregate items need the seeded empty-key group.
    if groups.is_empty() {
        if !key_items.is_empty() {
            return Ok(Some(finish_folded_rows(graph, &output, Vec::new())?));
        }
        groups.insert(Vec::new(), (Vec::new(), proto_aggs));
    }
    let out = groups
        .into_values()
        .map(|(key_vals, aggs)| {
            let mut row = Row::new();
            for (i, p) in items.iter().enumerate() {
                let name = p.output_name(i);
                if is_agg[i] {
                    row.insert(name, aggs[agg_rank[i]].0.finish());
                } else {
                    row.insert(name, key_vals[key_rank[i]].clone());
                }
            }
            row
        })
        .collect();
    Ok(Some(finish_folded_rows(graph, &output, out)?))
}

mod path;
mod plan;
pub(crate) use path::try_fold_path_aggs;

fn folded_return(graph: &ExecGraph<'_>, rows: Vec<Row>, items: &[Projection]) -> FoldedReturn {
    FoldedReturn {
        result: result_from_projected(graph, rows.clone(), items),
        rows,
        consumed_after: 1,
    }
}

pub(crate) fn result_from_projected(
    graph: &ExecGraph<'_>,
    rows: Vec<Row>,
    items: &[Projection],
) -> QueryResult {
    let columns: Vec<String> = items
        .iter()
        .enumerate()
        .map(|(i, p)| p.output_name(i))
        .collect();
    // The folded one-hop aggregate path (ADR-0036) binds labels-only entity
    // stubs (ADR-0033) and skips `project_rows`, so the RETURN-time refresh
    // (ADR-0170) has to happen here -- otherwise a projected node or
    // relationship would reach the caller clipped to its labels/type.
    let mut cache = MaterializeCache::default();
    let data: Vec<Vec<Value>> = rows
        .iter()
        .map(|r| {
            columns
                .iter()
                .map(|c| {
                    graph.materialize_cached(r.get(c).cloned().unwrap_or(Value::Null), &mut cache)
                })
                .collect()
        })
        .collect();
    QueryResult::intermediate(columns, data, graph.stats())
}
