//! Node candidate selection and relationship expansion (variable-length
//! walks) for the classic `MATCH` path — the forward walker.
//!
//! The pivot / start-selection mechanism (ADR-0350's bound-node pivot,
//! ADR-0351's cheapest-start selector, the backward walker and the
//! `pattern_self_contained` order-safety gate) lives in [`super::pivot`]
//! (LABS-352); this module keeps the walk itself.

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{NodeId, RelId, Result, Value};
use cypherlite_ir::{Direction, Expr, NodePattern, PathPattern, RelPattern};

use crate::ctx::{PathRow, VarWalk};
use crate::eval::{bind_pattern_node, Row};
use crate::overlay::ExecGraph;

use super::candidates::{
    node_candidates, node_ok, resolve_rel_seek, static_rel_type_ok, SeekedRel,
};
use super::far_probe::FarProbe;
use super::gpm::gpm_inline_predicates_ok;
use super::match_clause::{bind_named_path, eval_match_predicate};
use super::pivot::{
    self, bound_pivot, cheapest_start, cheapest_start_expansion, element_estimate, match_backward,
    Selection,
};
use super::project::{finish_match_row, SinkProjection};
use super::pushdown::{residual_ok, MatchCap, Residual};

/// Whether a binding sink wants the walk to keep going.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    Halt,
}

/// One delivery path for a complete binding: collect it, or probe and stop.
pub(crate) trait BindingSink {
    fn offer(&mut self, graph: &ExecGraph<'_>, row: Row, used: &HashSet<RelId>) -> Result<Flow>;
    fn halted(&self) -> bool;
}

/// What the last pattern of a `MATCH` evaluates before a row is stored.
pub(crate) struct MatchReduce<'a> {
    pub projection: &'a SinkProjection<'a>,
    pub patterns: &'a [PathPattern],
    pub where_clause: Option<&'a Expr>,
    pub params: &'a BTreeMap<String, Value>,
    pub cypher_version: Option<u32>,
}

/// The matched-rows payload: each surviving row with its used-rel set
/// (the relationship-uniqueness carrier). The one name for this shape,
/// shared by the walk's sinks, the hub join's side bindings, and the
/// shortest-route matcher (LABS-348: it had three private spellings).
pub(crate) type Matched = Vec<(Row, HashSet<RelId>)>;

/// Collect bindings, optionally stopping once a [`MatchCap`] is full.
pub(crate) struct CollectSink<'out, 'cap, 'data, 'reduce> {
    pub out: &'out mut Matched,
    pub cap: Option<&'cap mut MatchCap<'data>>,
    /// Set on the last pattern when the following clause is projected here.
    pub reduce: Option<&'reduce MatchReduce<'reduce>>,
    /// Intermediate patterns keep the set for the next pattern. The last
    /// pattern's finished rows do not.
    pub keep_used: bool,
    /// Workspace of `out`, released when the collector drops.
    pub charge: cypherlite_core::tokens::WorkspaceCharge,
    /// Running total of `out`. Updated only while a ledger is installed.
    pub meter: crate::workspace::BindingMeter,
}

impl BindingSink for CollectSink<'_, '_, '_, '_> {
    fn offer(&mut self, graph: &ExecGraph<'_>, row: Row, used: &HashSet<RelId>) -> Result<Flow> {
        if let Some(reduce) = self.reduce {
            if !gpm_inline_predicates_ok(
                graph,
                reduce.patterns,
                &row,
                reduce.params,
                reduce.cypher_version,
            )? {
                return Ok(Flow::Continue);
            }
            if let Some(predicate) = reduce.where_clause {
                if eval_match_predicate(
                    graph,
                    &row,
                    predicate,
                    reduce.params,
                    reduce.cypher_version,
                )?
                .truthy()
                    != Some(true)
                {
                    return Ok(Flow::Continue);
                }
            }
        }
        if let Some(cap) = self.cap.as_deref_mut() {
            cap.offer(graph, &row)?;
        }
        let stored = if let Some(reduce) = self.reduce {
            finish_match_row(
                graph,
                &row,
                reduce.projection,
                reduce.params,
                reduce.cypher_version,
            )?
        } else {
            row
        };
        let used = if self.keep_used {
            used.clone()
        } else {
            HashSet::new()
        };
        let used_len = used.len();
        self.out.push((stored, used));
        if cypherlite_core::tokens::workspace_accounting() {
            let mark = self
                .meter
                .push(&self.out.last().expect("binding just pushed").0, used_len);
            if let Err(err) = self.charge.resize(self.meter.bytes) {
                self.out.pop();
                self.meter.unpush(mark);
                return Err(err);
            }
        }
        Ok(if self.halted() {
            Flow::Halt
        } else {
            Flow::Continue
        })
    }

    fn halted(&self) -> bool {
        self.cap.as_ref().is_some_and(|cap| cap.full())
    }
}

/// Offer each binding to `accept`. The first `true` halts the walk.
pub(crate) struct ProbeSink<'a> {
    pub accept: &'a dyn Fn(Row, HashSet<RelId>) -> Result<bool>,
    pub halt: &'a Cell<bool>,
}

impl BindingSink for ProbeSink<'_> {
    fn offer(&mut self, _graph: &ExecGraph<'_>, row: Row, used: &HashSet<RelId>) -> Result<Flow> {
        if (self.accept)(row, used.clone())? {
            self.halt.set(true);
            Ok(Flow::Halt)
        } else {
            Ok(Flow::Continue)
        }
    }

    fn halted(&self) -> bool {
        self.halt.get()
    }
}

/// Per-path matching context shared by the node/relationship expansion
/// recursion: graph view, the path pattern, parameters, residuals, and the
/// enclosing statement's CIP-150 `cypher_version`.
#[derive(Clone, Copy)]
pub(crate) struct PathMatch<'a, 'data> {
    pub graph: &'a ExecGraph<'data>,
    pub path: &'a PathPattern,
    pub params: &'a BTreeMap<String, Value>,
    pub cypher_version: Option<u32>,
    pub residual: &'a Residual<'a>,
    pub probe: &'a FarProbe,
    /// LABS-161: the first hop's forced rel id, when an
    /// `elementId(r)`/`id(r)` equality or a row-bound `r` resolves it —
    /// the walk seeks that rel instead of enumerating the adjacency.
    pub rel_seek: Option<RelId>,
}

/// The outcome of the LABS-161 first-hop relationship seek: how an
/// `elementId(r) = …` / `id(r) = …` equality (or a row-bound `r`) on the
/// first rel's variable narrows the walk's first step. The walk anchors
/// through [`first_rel_seek`]; the one-hop aggregate fold decodes the
/// shared [`SeekedRel`] into the same vocabulary, so each caller's
/// gone-conditions are explicit at its own boundary.
pub(crate) enum RelSeek {
    /// Seed `match_node` at this endpoint of the rel (the first node
    /// pattern is free) and force the first hop to the rel.
    Anchored(NodeId, RelId),
    /// The first node keeps its own candidacy (it is row-bound); only
    /// the first hop is forced to the rel.
    Forced(RelId),
    /// The resolved rel cannot match: not visible (deleted or never
    /// existed), rejected by the pattern's static type filter, or —
    /// through the fold's decode — not incident to the row-bound start
    /// node. The pattern matches nothing.
    Gone,
}

/// LABS-161, the rel twin of the node id seek: cypherlite element ids are
/// transparent (`5:cypherlite:r<id>` self-encodes the RelId), so
/// `elementId(r) = <expr>` / `id(r) = <expr>` on the first rel's variable
/// — or `r` already bound in the row — resolves the RelId with a parse,
/// no lookup structure and nothing to invalidate. The walk then seeds
/// from that rel's endpoint ([`RelSeek::Anchored`], the analog of
/// `forced: Option<NodeId>` in [`match_node`]) instead of scanning every
/// node and walking each adjacency per row. The decode itself is
/// [`resolve_rel_seek`]; the walk's own policy is the first-node binding:
/// a row-bound node keeps its candidacy and only the hop is forced, and
/// an undirected pattern with a free first node declines — it matches
/// the rel in both orientations, which a single anchor would drop.
pub(crate) fn first_rel_seek(
    graph: &ExecGraph<'_>,
    path: &PathPattern,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    residual: &Residual<'_>,
) -> Option<RelSeek> {
    let rp = path.rels.first()?;
    let (start, end, rid) =
        match resolve_rel_seek(graph, rp, residual.conjuncts, row, params, cypher_version) {
            SeekedRel::Decline => return None,
            SeekedRel::Gone => return Some(RelSeek::Gone),
            SeekedRel::Resolved { start, end, rid } => (start, end, rid),
        };
    match crate::ctx::binding(&path.nodes[0], row) {
        crate::ctx::Binding::Node(_) => Some(RelSeek::Forced(rid)),
        // A mismatching binding matches nothing either way; the walk
        // answers zero rows without the seek.
        crate::ctx::Binding::Mismatch => None,
        crate::ctx::Binding::Unbound | crate::ctx::Binding::Anonymous => {
            let anchor = match rp.direction {
                Direction::Outgoing => start,
                Direction::Incoming => end,
                Direction::Both => return None,
            };
            Some(RelSeek::Anchored(anchor, rid))
        }
    }
}

/// The resolve-then-walk orchestration shared by `match_one_path` and the
/// EXISTS probe: arm the destination prefilter ([`FarProbe::prepare`],
/// #303 — a blocking probe matches nothing), decode the LABS-161 rel seek
/// ([`first_rel_seek`] — `Some(RelSeek::Gone)` means the pattern matches
/// nothing; `None` declines the seek and the adjacency scan runs), then
/// the walk from node 0 into `sink`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn match_resolved_path(
    graph: &ExecGraph<'_>,
    path: &PathPattern,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    residual: &Residual<'_>,
    row: Row,
    used: HashSet<RelId>,
    sink: &mut dyn BindingSink,
) -> Result<()> {
    let probe = FarProbe::prepare(graph, path, &row, params, cypher_version, residual);
    if probe.blocks_path() {
        return Ok(());
    }
    let (forced, rel_seek) =
        match first_rel_seek(graph, path, &row, params, cypher_version, residual) {
            Some(RelSeek::Anchored(anchor, rid)) => (Some(anchor), Some(rid)),
            Some(RelSeek::Forced(rid)) => (None, Some(rid)),
            Some(RelSeek::Gone) => return Ok(()),
            None => (None, None),
        };
    let pm = PathMatch {
        graph,
        path,
        params,
        cypher_version,
        residual,
        probe: &probe,
        rel_seek,
    };
    // ADR-0350: a later element row-bound to a node pivots the expansion to
    // that index (backward prefix walk + forward suffix walk) instead of
    // re-scanning index 0 per outer row. The LABS-161 first-rel seek
    // anchors node 0's walk specifically, so BOTH start-selection
    // mechanisms — the bound pivot and the ADR-0351 cheapest start —
    // stand down in its presence and keep today's index-0 start.
    let pivots_eligible = forced.is_none() && rel_seek.is_none();
    let pivot = if pivots_eligible {
        bound_pivot(path, &row)
    } else {
        None
    };
    // The compound estimate inputs, derived once per row — but only when
    // a start selection can actually run (a bound pivot to gate, or the
    // cheapest-start arm; nothing pays when pivots are ineligible, and
    // nothing pays when the bound pivot wins ungated). The estimate can
    // descend indexes, so a dead derivation per row is not a small cost
    // (stellar F-8-x; q19 measured the unconditional form at 2x).
    let selection = if pivots_eligible {
        let bound = |name: &str| row.get(name).is_some();
        let stats = crate::exec::graph_stats::GraphStats::new(pm.graph.base(), pm.params);
        // The bound pivot only wins the start when it is genuinely
        // cheaper after its first hop's degree scaling (LABS-351,
        // q30's regression): a row-bound element estimates 1, but a
        // bound creator fanning ~95 posts per row is dearer than a
        // 1-hop forward start from the other end — the same compound
        // comparison the cheapest start runs.
        pivot
            .filter(|&(k, _)| {
                element_estimate(&stats, path, &bound, pm.residual.conjuncts, k)
                    < element_estimate(&stats, path, &bound, pm.residual.conjuncts, 0)
            })
            .map(Selection::Pivot)
            // ADR-0351: nothing is row-bound (or the bound pivot lost its
            // gate) — start at the pattern's cheapest element instead of
            // index 0. `or_else`: no pass-through arm (stellar F-11-5).
            .or_else(|| cheapest_start(&pm, &row, &stats).map(Selection::Cheapest))
    } else {
        None
    };
    // LABS-369: name the start this clause's walk chose — the fact an
    // opaque `Expand` hid. Recorded per walk (the clause's FIRST walk
    // wins, so a per-row-varying pivot reports its representative start)
    // and read once at the clause's metric tick.
    // LABS-369: name the start this clause's walk chose — the mapping
    // lives beside the vocabulary it interprets, so this site records one
    // value and the match below only picks the expansion. The clause's
    // FIRST walk wins (a per-row-varying pivot reports its representative
    // start); the metric tick takes it once.
    super::record_walk_start(pivot::walk_start(selection.as_ref(), forced.is_some()));
    match selection {
        Some(Selection::Pivot((k, id))) => match_backward(&pm, k, k, id, row, used, sink),
        Some(Selection::Cheapest(k)) => cheapest_start_expansion(&pm, k, row, used, sink),
        None => match_node(&pm, 0, row, used, forced, sink),
    }
}

/// `None` means the forced id is outside the far-node index prefilter.
fn forced_candidate(np: &NodePattern, probe: &FarProbe, id: NodeId) -> Option<Vec<NodeId>> {
    if np
        .variable
        .as_ref()
        .is_some_and(|var| probe.rejects(var, id))
    {
        #[cfg(test)]
        super::far_probe::count_far_node_reject();
        return None;
    }
    Some(vec![id])
}

/// Arm the forward hop's destination prefilter (`node rel_idx + 1`).
/// `false` means that node cannot match.
fn admit_far_node(
    pm: &PathMatch<'_, '_>,
    rel_idx: usize,
    from: NodeId,
    frontier: Option<usize>,
) -> bool {
    let rel = &pm.path.rels[rel_idx];
    admit_far_node_at(pm, rel_idx + 1, rel, rel.direction, from, frontier)
}

/// The prefilter arm shared by the forward hop ([`admit_far_node`], far
/// node `rel_idx + 1`) and ADR-0350's backward walker (far node `node_idx`
/// behind the current node): arm `path.nodes[node_idx]`'s prefilter for the
/// rel about to be traversed in `traverse` direction. `false` means that
/// node cannot match.
pub(super) fn admit_far_node_at(
    pm: &PathMatch<'_, '_>,
    node_idx: usize,
    rel: &RelPattern,
    traverse: Direction,
    from: NodeId,
    frontier: Option<usize>,
) -> bool {
    let PathMatch {
        graph, path, probe, ..
    } = *pm;
    let Some(var) = path.nodes[node_idx].variable.as_ref() else {
        return true;
    };
    if !probe.covers(var) {
        return true;
    }
    // A variable-length walk has no one-hop bound, so the frontier is unbounded.
    if rel.var_length {
        return probe.admit(graph, var, usize::MAX);
    }
    if !probe.needs_frontier(var) {
        return probe.admit(graph, var, 0);
    }
    let frontier = frontier.unwrap_or_else(|| graph.typed_adj_len(from, traverse, &rel.types));
    probe.admit(graph, var, frontier)
}

pub(crate) fn match_node(
    pm: &PathMatch<'_, '_>,
    idx: usize,
    row: Row,
    used: HashSet<RelId>,
    forced: Option<NodeId>,
    sink: &mut dyn BindingSink,
) -> Result<()> {
    let PathMatch {
        graph,
        path,
        params,
        cypher_version,
        residual,
        probe,
        rel_seek: _,
    } = *pm;
    let np = &path.nodes[idx];
    let candidates = match forced {
        Some(id) => match forced_candidate(np, probe, id) {
            Some(ids) => ids,
            None => return Ok(()),
        },
        None => super::phase_seek(|| {
            node_candidates(graph, np, &row, params, cypher_version, residual)
        })?,
    };
    for id in candidates {
        graph.cancel_tick()?;
        if sink.halted() {
            break;
        }
        if !node_ok(graph, id, np, &row, params, cypher_version)? {
            continue;
        }
        let mut row2 = row.clone();
        if !bind_pattern_node(graph, &mut row2, np, id, idx, path.variable.is_some()) {
            continue;
        }
        if !residual_ok(graph, &row2, params, cypher_version, residual)? {
            continue;
        }
        if idx >= path.rels.len() {
            bind_named_path(graph, path, &mut row2);
            if sink.offer(graph, row2, &used)? == Flow::Halt {
                return Ok(());
            }
        } else {
            // LABS-448: the walk phase, through the one wrapper —
            // inclusive of the nested phases the recursion performs.
            super::phase_walk(|| expand_rel(pm, idx, id, row2, used.clone(), sink))?;
            if sink.halted() {
                return Ok(());
            }
        }
    }
    Ok(())
}

pub(crate) fn expand_bound_rel_list(
    pm: &PathMatch<'_, '_>,
    rel_idx: usize,
    from: NodeId,
    row: Row,
    used: HashSet<RelId>,
    ids: &[RelId],
    sink: &mut dyn BindingSink,
) -> Result<()> {
    let PathMatch { graph, path, .. } = *pm;
    let rp = &path.rels[rel_idx];
    if !admit_far_node(pm, rel_idx, from, Some(ids.len())) {
        return Ok(());
    }
    let hops = ids.len() as u32;
    if hops < rp.min_hops || rp.max_hops.is_some_and(|m| hops > m) {
        return Ok(());
    }
    let mut current = from;
    let mut trail = Vec::new();
    let mut used2 = used.clone();
    for &rid in ids {
        if used2.contains(&rid) || trail.contains(&rid) {
            return Ok(());
        }
        let Some(next) = graph.hop(current, rid, rp.direction) else {
            return Ok(());
        };
        trail.push(rid);
        used2.insert(rid);
        current = next;
    }
    let mut row2 = row;
    if let Some(v) = &rp.variable {
        let rels: Vec<_> = trail
            .iter()
            .map(|id| crate::ctx::bind_rel_stub(graph, *id))
            .collect();
        row2.insert(v.clone(), Value::List(rels.into()));
    } else if path.variable.is_some() {
        let rels: Vec<_> = trail
            .iter()
            .map(|id| crate::ctx::bind_rel_stub(graph, *id))
            .collect();
        row2.insert(format!("__trail{rel_idx}"), Value::List(rels.into()));
    }
    match_node(pm, rel_idx + 1, row2, used2, Some(current), sink)
}

pub(crate) fn expand_rel(
    pm: &PathMatch<'_, '_>,
    rel_idx: usize,
    from: NodeId,
    row: Row,
    used: HashSet<RelId>,
    sink: &mut dyn BindingSink,
) -> Result<()> {
    let PathMatch { path, .. } = *pm;
    let rp = &path.rels[rel_idx];
    if rp.var_length {
        if let Some(v) = &rp.variable {
            if let Some(Value::List(xs)) = row.get(v) {
                let ids: Option<Vec<RelId>> = xs
                    .iter()
                    .map(|x| match x {
                        Value::Relationship(r) => Some(r.id),
                        _ => None,
                    })
                    .collect();
                if let Some(ids) = ids {
                    return expand_bound_rel_list(pm, rel_idx, from, row, used, &ids, sink);
                }
            }
        }
    }
    if !admit_far_node(pm, rel_idx, from, None) {
        return Ok(());
    }
    let (min_h, max_h) = crate::ctx::hop_bounds(rp);
    // LABS-161: resolve the forced first hop before entering the walk, so
    // walk stays coordinate-free — the seeked rel goes through the same
    // per-candidate step ([`walk_candidate`]) the enumeration applies per
    // neighbor (`resolve_rel_seek` already ran the static type filter in
    // place of the adjacency's; the re-check inside is one cheap
    // comparison). A seek only exists for a fixed-length first rel
    // (min_h = max_h = 1), so no zero-hop binding coexists with it.
    if rel_idx == 0 {
        if let Some(rid) = pm.rel_seek {
            return walk_candidate(
                pm,
                rel_idx,
                &row,
                &used,
                min_h,
                max_h,
                0,
                &[],
                from,
                rid,
                sink,
            );
        }
    }
    walk(
        pm,
        rel_idx,
        from,
        PathRow { row, used },
        VarWalk {
            min_h,
            max_h,
            hops: 0,
            trail: Vec::new(),
        },
        sink,
    )
}

/// The ordered per-hop checks both the adjacency enumeration and the
/// LABS-161 first-hop rel seek apply to one candidate rel: not already
/// used on this walk, allowed by the pattern's static type filter (the
/// enumeration's adjacency applied it while scanning; one cheap rel_type
/// comparison on already-filtered neighbors keeps the check in one
/// place), the rel edge predicate, and `hop` resolving the next node.
/// Returns the next node when the hop is offered.
#[allow(clippy::too_many_arguments)]
pub(super) fn hop_candidate(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    rp: &RelPattern,
    traverse: Direction,
    used: &HashSet<RelId>,
    trail: &[RelId],
    current: NodeId,
    rid: RelId,
) -> Result<Option<NodeId>> {
    if used.contains(&rid) || trail.contains(&rid) {
        return Ok(None);
    }
    if !static_rel_type_ok(graph, rid, &rp.types) {
        return Ok(None);
    }
    if !crate::eval::rel_edge_ok(graph, rid, rp, row, params, cypher_version)? {
        return Ok(None);
    }
    // The accepted-candidate counter, incremented once per hop the caller
    // takes — the one place the walk's two entry paths (the LABS-161 seek
    // and the adjacency enumeration) both go through.
    match graph.hop(current, rid, traverse) {
        Some(next) => {
            super::count_neighbor_walk();
            Ok(Some(next))
        }
        None => Ok(None),
    }
}

/// One candidate hop from the walk's current state — the per-candidate
/// step shared by the adjacency enumeration inside `walk` and the
/// LABS-161 first-hop rel seek in `expand_rel` (the walk's two entry
/// paths): the tick/halt prelude, the ordered checks of
/// [`hop_candidate`], then the walk recursion with the hop taken.
#[allow(clippy::too_many_arguments)]
fn walk_candidate(
    pm: &PathMatch<'_, '_>,
    rel_idx: usize,
    row: &Row,
    used: &HashSet<RelId>,
    min_h: u32,
    max_h: u32,
    hops: u32,
    trail: &[RelId],
    current: NodeId,
    rid: RelId,
    sink: &mut dyn BindingSink,
) -> Result<()> {
    let PathMatch {
        graph,
        path,
        params,
        cypher_version,
        ..
    } = *pm;
    graph.cancel_tick()?;
    if sink.halted() {
        return Ok(());
    }
    if rel_idx > 0 {
        super::count_hop_expand();
    }
    let rp = &path.rels[rel_idx];
    let Some(next_node) = hop_candidate(
        graph,
        row,
        params,
        cypher_version,
        rp,
        rp.direction,
        used,
        trail,
        current,
        rid,
    )?
    else {
        return Ok(());
    };
    let mut trail2 = trail.to_vec();
    trail2.push(rid);
    walk(
        pm,
        rel_idx,
        next_node,
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
        sink,
    )
}

pub(crate) fn walk(
    pm: &PathMatch<'_, '_>,
    rel_idx: usize,
    current: NodeId,
    state: PathRow,
    vw: VarWalk,
    sink: &mut dyn BindingSink,
) -> Result<()> {
    let PathMatch { graph, path, .. } = *pm;
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
        // Shared trail binding (eval/pattern.rs), pre-bound
        // reconciliation included.
        if !crate::eval::bind_trail(
            &mut row2,
            rp,
            &trail,
            graph,
            rel_idx,
            path.variable.is_some(),
        ) {
            return Ok(());
        }
        if hops >= min_h {
            for id in &trail {
                used2.insert(*id);
            }
            // build path variable? skip
            match_node(pm, rel_idx + 1, row2, used2, Some(current), sink)?;
            if sink.halted() {
                return Ok(());
            }
        }
    }
    if hops >= max_h {
        return Ok(());
    }
    graph.try_for_each_neighbor(current, rp.direction, &rp.types, |rid| {
        walk_candidate(
            pm, rel_idx, &row, &used, min_h, max_h, hops, &trail, current, rid, sink,
        )
    })?;
    Ok(())
}
