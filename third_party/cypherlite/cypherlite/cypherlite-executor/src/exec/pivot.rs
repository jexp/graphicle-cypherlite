//! The pivot / start-selection mechanism for a `MATCH` path walk
//! (LABS-352): ADR-0350's bound-node pivot, ADR-0351's cheapest-start
//! selector, the backward walker and the `pattern_self_contained`
//! order-safety gate. The forward walker stays in [`super::expand`]; this
//! module owns the mechanism that chooses where a walk starts and walks a
//! pivot's prefix backwards. Pure extraction — no behavior change.

#[cfg(test)]
use std::cell::Cell;
use std::collections::HashSet;

use cypherlite_core::{NodeId, RelId, Result};
use cypherlite_ir::{Direction, PathPattern, RelPattern};

use crate::eval::{bind_pattern_node, Row};

use super::candidates::{node_candidates, node_ok};
use super::expand::{admit_far_node_at, hop_candidate, match_node, BindingSink, PathMatch};
use super::pushdown::residual_ok;

#[cfg(test)]
thread_local! {
    /// ADR-0350 test hook: hops the backward walker took, so a test can
    /// assert the pivot engaged (and, negated, that a fallback did not).
    static PIVOT_BACKWARD_HOPS: Cell<u64> = const { Cell::new(0) };
    /// ADR-0350 test hook: force the index-0 start so a test can compare
    /// the pivot's rows against the un-pivoted expansion.
    static PIVOT_DISABLED: Cell<bool> = const { Cell::new(false) };
}

/// The one test hook for start-selection A/B runs (stellar F-1-13):
/// `with_pivot_disabled` sets it for the closure's duration.
#[cfg(test)]
fn pivot_disabled() -> bool {
    PIVOT_DISABLED.with(Cell::get)
}

#[cfg(test)]
pub(super) fn take_pivot_backward_hops() -> u64 {
    PIVOT_BACKWARD_HOPS.with(|count| count.replace(0))
}

#[cfg(test)]
pub(super) fn with_pivot_disabled<R>(f: impl FnOnce() -> R) -> R {
    /// Restores the flag on the way out — including on unwind, so a
    /// panicking assertion inside the closure cannot leave index-0 starts
    /// forced for every later test on the thread.
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            PIVOT_DISABLED.with(|flag| flag.set(false));
        }
    }
    PIVOT_DISABLED.with(|flag| flag.set(true));
    let _restore = Restore;
    f()
}

/// The walk's chosen start as the metric vocabulary spells it (LABS-369):
/// the selection's element and direction plus the reason class — the
/// mapping lives beside the vocabulary it interprets, so the orchestration
/// site records one value.
pub(super) fn walk_start(
    selection: Option<&Selection>,
    seek_anchored: bool,
) -> crate::ctx::WalkStart {
    match selection {
        Some(Selection::Pivot((k, _))) => crate::ctx::WalkStart {
            element: *k,
            backward: true,
            reason: crate::ctx::StartReason::BoundPivot,
        },
        Some(Selection::Cheapest(k)) => crate::ctx::WalkStart {
            element: *k,
            backward: false,
            reason: crate::ctx::StartReason::Cheapest,
        },
        None => crate::ctx::WalkStart {
            element: 0,
            backward: false,
            reason: if seek_anchored {
                crate::ctx::StartReason::SeekAnchored
            } else {
                crate::ctx::StartReason::IndexZero
            },
        },
    }
}

/// The chosen alternative start (LABS-351): a gated bound pivot (with the
/// row-bound element's id) or a cheapest element index.
pub(super) enum Selection {
    Pivot((usize, NodeId)),
    Cheapest(usize),
}

/// ADR-0351's start loop: enumerate the chosen element's candidates — its
/// own pattern filters and residual conjuncts apply exactly as
/// `match_node`'s per-candidate checks would — bind each, and run the
/// ADR-0350 backward walker from it; the walker's suffix hand-off
/// short-circuits on that binding.
pub(super) fn cheapest_start_expansion(
    pm: &PathMatch<'_, '_>,
    k: usize,
    row: Row,
    used: HashSet<RelId>,
    sink: &mut dyn BindingSink,
) -> Result<()> {
    let PathMatch {
        graph,
        path,
        params,
        cypher_version,
        residual,
        ..
    } = *pm;
    let np = &path.nodes[k];
    let candidates =
        super::phase_seek(|| node_candidates(graph, np, &row, params, cypher_version, residual))?;
    for id in candidates {
        graph.cancel_tick()?;
        if sink.halted() {
            break;
        }
        if !node_ok(graph, id, np, &row, params, cypher_version)? {
            continue;
        }
        let mut row2 = row.clone();
        if !bind_pattern_node(graph, &mut row2, np, id, k, path.variable.is_some()) {
            continue;
        }
        if !residual_ok(graph, &row2, params, cypher_version, residual)? {
            continue;
        }
        // LABS-448: the walk phase, through the one wrapper — inclusive
        // of its nested phases.
        super::phase_walk(|| match_backward(pm, k, k, id, row2, used.clone(), sink))?;
        if sink.halted() {
            break;
        }
    }
    Ok(())
}

/// One element's degree-scaled start estimate with its adjacent first
/// hop — the comparison both start selections run, spelled once
/// (stellar F-1-1/F-1-2/F-1-4): the bound-pivot gate and
/// `cheapest_start` rank candidates with the same numbers. Element 0's
/// first hop is the path's first rel; element k walks `rels[k - 1]`
/// backward. Element 0 is the start node the parsed-pattern invariant
/// guarantees (F-8-4's boundary, stated once here).
pub(super) fn element_estimate<'a>(
    stats: &crate::exec::graph_stats::GraphStats<'a>,
    path: &PathPattern,
    bound: cypherlite_planner::Bound<'_>,
    conjuncts: impl IntoIterator<Item = &'a cypherlite_ir::Expr>,
    k: usize,
) -> u64 {
    // The walk, derived once (round 2, F-2-1: one branch on `k == 0`, not
    // three spellings of it): k == 0 is the forward walker (seed
    // nodes[0], hop rels[0]); k > 0 is the backward walker's pivot
    // candidate — the walk from nodes[k] back through rels[k-1], the
    // reversed evaluation of the planned spelling (LABS-426).
    let (np, first_hop, reversed) = if k == 0 {
        (
            path.nodes
                .first()
                .expect("a parsed path pattern always has a start node"),
            path.rels.first(),
            false,
        )
    } else {
        (&path.nodes[k], path.rels.get(k - 1), true)
    };
    stats.estimate(np, bound, conjuncts, first_hop, reversed)
}

/// ADR-0351/LABS-351: the pattern's cheapest element when no element is
/// row-bound — scored by the planner's one canonical endpoint ladder
/// ([`cypherlite_planner::endpoint_estimate`] over [`GraphStats`]): a
/// bound variable is 1, then the best measured index signal (RANGE
/// equality hits, TEXT CONTAINS candidates, label rows), then the
/// unmeasured rank sentinels, all on one scale. Only elements carrying
/// a variable are eligible: the start element's row binding is what
/// short-circuits the suffix walk's re-enumeration (`match_node`'s
/// bound arm), and an anonymous element cannot provide it. `None`
/// keeps today's index-0 start — index 0 is already (or ties as)
/// cheapest, or the same fallbacks as [`bound_pivot`] apply: a
/// shortest-path function, or a variable-length/quantified prefix (the
/// backward walker is single-hop only).
pub(super) fn cheapest_start(
    pm: &PathMatch<'_, '_>,
    row: &Row,
    stats: &crate::exec::graph_stats::GraphStats<'_>,
) -> Option<usize> {
    #[cfg(test)]
    if pivot_disabled() {
        return None;
    }
    let path = pm.path;
    if path.is_shortest_path_function {
        return None;
    }
    // The order-safety gate is k-independent and decided once, as an
    // early return before any estimate runs (the estimate can descend
    // an index; a gate that voids every candidate must not pay for it
    // first — stellar F-2-14, and the per-k filter must not re-decide
    // it per candidate index — F-2-4). A non-self-contained pattern
    // takes no alternative start at all.
    if !pattern_self_contained(path, row) {
        return None;
    }
    // The bound view is the one row-dependent input (a closure over the
    // row, no allocation); stats and conjuncts arrive derived. The
    // ladder itself is narrated once, on `GraphStats::estimate`.
    let bound = |name: &str| row.get(name).is_some();
    // The estimates run through the one shared `element_estimate`
    // (the bound-pivot gate ranks with the same numbers — stellar
    // F-1-1/F-1-2/F-1-4).
    let est0 = element_estimate(stats, path, &bound, pm.residual.conjuncts, 0);
    (1..path.nodes.len())
        .filter(|&k| path.nodes[k].variable.is_some() && prefix_plain(path, k))
        .map(|k| {
            (
                k,
                element_estimate(stats, path, &bound, pm.residual.conjuncts, k),
            )
        })
        .filter(|&(_, est)| est < est0)
        .min_by_key(|&(k, est)| (est, k))
        .map(|(k, _)| k)
}

/// The one eligibility fact both start selections consult: the prefix
/// `rels[..k]` is plain — every hop single, no variable-length and no
/// quantified path — so the backward walker can walk it (LABS-351,
/// stellar F-1-4).
fn prefix_plain(path: &PathPattern, k: usize) -> bool {
    !path.rels[..k]
        .iter()
        .any(|rp| rp.var_length || rp.quantified)
}

/// ADR-0350: the smallest pattern index `k > 0` whose variable is row-bound
/// to a node — the pivot the expansion starts at instead of rescanning
/// index 0. `None` keeps today's index-0 start: no such element, a prefix
/// rel that is variable-length or quantified (the backward walker is
/// single-hop only in this first cut — a documented gap, not a silent
/// fallback), or a shortest-path function (its uniqueness semantics are the
/// GPM machinery's). `RepeatableElements` and GPM paths never reach
/// `match_resolved_path`: `match_one_path` and `exists_probe`'s
/// `plain_match` route them away before this point.
pub(super) fn bound_pivot(path: &PathPattern, row: &Row) -> Option<(usize, NodeId)> {
    #[cfg(test)]
    if pivot_disabled() {
        return None;
    }
    if path.is_shortest_path_function {
        return None;
    }
    for k in 1..path.nodes.len() {
        // The canonical row-bound-node classification (the same one the
        // walk's candidate checks read), never a second `Value::Node`
        // shape-match here.
        if let Some(id) = crate::eval::bound_node(&path.nodes[k], row) {
            return (prefix_plain(path, k) && pattern_self_contained(path, row)).then_some((k, id));
        }
    }
    None
}

/// The backward walk's evaluation-order gate (the stellar High finding on
/// #346): the forward walk binds the pattern's elements left to right, so
/// an element's predicate may legally reference another element's
/// variable and see it bound. Both pivots bind in a different order — the
/// bound-node pivot walks the prefix right to left, and the cheapest
/// start evaluates its start element before anything else binds — so the
/// same reference can see an unbound (null) or differently-bound variable
/// and silently change the result. Both pivots are therefore only taken
/// when the pattern is **element-wise self-contained**: no element's
/// predicates reference another element's variable. A variable the
/// incoming row already binds (an outer pattern's binding) is safe in
/// every order.
/// `pub(super)` for the pivot tests' direct, planner-independent unit
/// coverage of the gate.
pub(super) fn pattern_self_contained(path: &PathPattern, row: &Row) -> bool {
    let node_vars: Vec<Option<&String>> = path.nodes.iter().map(|n| n.variable.as_ref()).collect();
    let rel_vars: Vec<Option<&String>> = path.rels.iter().map(|rp| rp.variable.as_ref()).collect();
    let entry_bound = |v: &str| row.get(v).is_some();
    // The one cross-reference predicate, shared by the node and the rel
    // loop — a correctness gate never spelled twice.
    let cross_reference = |refs: &HashSet<String>, own: Option<&str>| -> bool {
        refs.iter().any(|v| {
            Some(v.as_str()) != own
                && (node_vars.iter().any(|nv| nv.is_some_and(|n| n == v))
                    || rel_vars.iter().any(|rv| rv.is_some_and(|r| r == v)))
                && !entry_bound(v)
        })
    };
    let mut refs = HashSet::new();
    for (i, np) in path.nodes.iter().enumerate() {
        refs.clear();
        if let Some(e) = &np.properties {
            crate::eval::expr_vars(e, &mut refs);
        }
        if let Some(e) = &np.where_clause {
            crate::eval::expr_vars(e, &mut refs);
        }
        if let Some(le) = &np.label_expr {
            crate::eval::label_expr_vars(le, &mut refs);
        }
        let own: Option<&str> = node_vars.get(i).cloned().flatten().map(String::as_str);
        if cross_reference(&refs, own) {
            return false;
        }
    }
    for (i, rp) in path.rels.iter().enumerate() {
        refs.clear();
        if let Some(e) = &rp.properties {
            crate::eval::expr_vars(e, &mut refs);
        }
        if let Some(e) = &rp.where_clause {
            crate::eval::expr_vars(e, &mut refs);
        }
        if let Some(te) = &rp.type_expr {
            crate::eval::label_expr_vars(te, &mut refs);
        }
        let own: Option<&str> = rel_vars.get(i).cloned().flatten().map(String::as_str);
        if cross_reference(&refs, own) {
            return false;
        }
    }
    true
}

/// ADR-0350: the backward walker for a pivot's prefix. From the bound node
/// at `idx` (its binding already in `row`, id `from`), traverse
/// `path.rels[idx - 1]` one single-hop rel at a time with the direction
/// flipped, binding the rel and the reached node under their **original**
/// indices so `bind_named_path` and the `__node{i}`/`__trail{i}` keys stay
/// correct (that is why this walker exists instead of a reversed pattern).
/// At `idx - 1 == 0` the prefix is complete and the suffix forward walk
/// (`match_node` at `pivot`, which sees the pivot's row binding as a
/// single-id candidate) finishes the path, `bind_named_path` firing exactly
/// once with the original indices. The far-node prefilter still applies:
/// [`admit_far_node_at`] arms node `idx - 1` with the flipped direction's
/// frontier, exactly as `expand_rel` arms the forward destination.
pub(super) fn match_backward(
    pm: &PathMatch<'_, '_>,
    idx: usize,
    pivot: usize,
    from: NodeId,
    row: Row,
    used: HashSet<RelId>,
    sink: &mut dyn BindingSink,
) -> Result<()> {
    let PathMatch { graph, path, .. } = *pm;
    let rp = &path.rels[idx - 1];
    // The pivot guarantees a single-hop prefix rel; hop_candidate's checks
    // below are the forward walk's, spelled once, walked the other way —
    // the flipped direction travels as a value, so the rel pattern itself
    // is never cloned per step.
    let back = rp.direction.flip();
    if !admit_far_node_at(pm, idx - 1, rp, back, from, None) {
        return Ok(());
    }
    graph.try_for_each_neighbor(from, back, &rp.types, |rid| {
        backward_candidate(pm, idx, pivot, rp, back, from, &row, &used, rid, sink)
    })
}

/// One candidate backward hop — the per-candidate step of
/// [`match_backward`], mirroring `walk_candidate` for a single-hop rel: the
/// tick/halt prelude, [`hop_candidate`]'s ordered checks over the flipped
/// rel, then the rel binding (`bind_trail`, pre-bound reconciliation
/// included), the node binding under the original index (an existing
/// non-matching binding fails this candidate), `node_ok` + `residual_ok`,
/// and the recursion toward index 0.
#[allow(clippy::too_many_arguments)]
fn backward_candidate(
    pm: &PathMatch<'_, '_>,
    idx: usize,
    pivot: usize,
    rp: &RelPattern,
    traverse: Direction,
    from: NodeId,
    row: &Row,
    used: &HashSet<RelId>,
    rid: RelId,
    sink: &mut dyn BindingSink,
) -> Result<()> {
    let PathMatch {
        graph,
        path,
        params,
        cypher_version,
        residual,
        ..
    } = *pm;
    graph.cancel_tick()?;
    if sink.halted() {
        return Ok(());
    }
    if idx - 1 > 0 {
        super::count_hop_expand();
    }
    let Some(prev) = hop_candidate(
        graph,
        row,
        params,
        cypher_version,
        rp,
        traverse,
        used,
        &[],
        from,
        rid,
    )?
    else {
        return Ok(());
    };
    #[cfg(test)]
    PIVOT_BACKWARD_HOPS.with(|count| count.set(count.get() + 1));
    let rel_idx = idx - 1;
    let mut row2 = row.clone();
    // bind_trail spells the rel binding once for both walk directions; the
    // original (unflipped) rel pattern carries the variable.
    if !crate::eval::bind_trail(
        &mut row2,
        &path.rels[rel_idx],
        std::slice::from_ref(&rid),
        graph,
        rel_idx,
        path.variable.is_some(),
    ) {
        return Ok(());
    }
    let mut used2 = used.clone();
    used2.insert(rid);
    let np = &path.nodes[rel_idx];
    // `node_ok` runs BEFORE the node's own binding, matching
    // `match_node`'s order — a self-referencing property map
    // (`(a {p: a.q})`) must evaluate identically in both walk
    // directions, and binding first would let the backward walk see
    // the candidate's own value where the forward walk sees unbound.
    if !node_ok(graph, prev, np, &row2, params, cypher_version)? {
        return Ok(());
    }
    if !bind_pattern_node(graph, &mut row2, np, prev, rel_idx, path.variable.is_some()) {
        return Ok(());
    }
    if !residual_ok(graph, &row2, params, cypher_version, residual)? {
        return Ok(());
    }
    if rel_idx == 0 {
        // Prefix complete: the suffix forward walk from the pivot finishes
        // the path (`bind_named_path` fires at its end as today).
        match_node(pm, pivot, row2, used2, None, sink)
    } else {
        match_backward(pm, rel_idx, pivot, prev, row2, used2, sink)
    }
}
