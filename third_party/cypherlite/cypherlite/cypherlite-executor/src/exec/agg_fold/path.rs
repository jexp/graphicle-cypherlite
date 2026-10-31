//! Fixed-length neighborhood aggregate fold.

use std::collections::{BTreeMap, HashMap};

use cypherlite_core::{Error, NodeId, RelId, Result, Value};
use cypherlite_ir::{Clause, Expr, NodePattern, PathPattern, Projection, RelPattern};
use indexmap::IndexMap;

use crate::ctx::{binding, Binding};
use crate::eval::{binary, eval, rel_props_ok, Row};
use crate::group_key::GroupKey;
use crate::overlay::ExecGraph;

use super::super::candidates::{node_candidates, node_ok};
use super::super::pushdown::Residual;
use super::plan::{compile_path, KeyPlan, PathItem, PathPlan, PathPred};
use super::{
    finish_folded_rows, fold_return, note_path_binding, CachedKey, FoldOutput, FoldedReturn,
    StreamAgg,
};

struct HopEnd {
    start: NodeId,
    end: NodeId,
    rel: RelId,
}

/// The path fold's graph-scoped caches: a node's properties and its
/// cached key are row-independent, so one map warms across the whole fold.
/// The row-scoped predicate verdicts live on [`FoldEnv`], which is built
/// fresh per row — a `PathPred::Cmp` verdict binds its other operand to
/// that row, so it cannot leak into the next by construction.
struct FoldCaches {
    props: rustc_hash::FxHashMap<NodeId, HashMap<String, Value>>,
    keys: rustc_hash::FxHashMap<NodeId, CachedKey>,
}

struct FoldEnv<'a> {
    graph: &'a ExecGraph<'a>,
    row: &'a Row,
    params: &'a BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    plan: &'a PathPlan,
    caches: &'a mut FoldCaches,
    /// The row-scoped predicate verdicts: `PathPred::Cmp`'s other operand
    /// evaluates against this row, so a verdict is valid only within it.
    /// The env is constructed fresh per row, so the map starts empty.
    preds: rustc_hash::FxHashMap<(usize, NodeId), bool>,
}

struct HopWalk<'a> {
    path: &'a PathPattern,
    rel_idx: usize,
    start_slot: usize,
    end_slot: usize,
    bound_start: Option<NodeId>,
    bound_end: Option<NodeId>,
    skip_rel: Option<RelId>,
}

fn path_pred_holds(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    pred: &PathPred,
    id: NodeId,
) -> Result<bool> {
    match pred {
        PathPred::Null { key, is_null } => {
            let value = graph.node_prop(id, key).unwrap_or(Value::Null);
            Ok(value.is_null() == *is_null)
        }
        PathPred::Cmp { key, op, other } => {
            let prop = graph.node_prop(id, key).unwrap_or(Value::Null);
            let other = eval(graph, row, other, params, cypher_version)?;
            Ok(binary(*op, prop, other)?.truthy() == Some(true))
        }
    }
}

fn slot_preds_hold(env: &mut FoldEnv<'_>, slot: usize, id: NodeId) -> Result<bool> {
    if let Some(&ok) = env.preds.get(&(slot, id)) {
        return Ok(ok);
    }
    let mut ok = true;
    for pred in &env.plan.conjuncts[slot].preds {
        if !path_pred_holds(env.graph, env.row, env.params, env.cypher_version, pred, id)? {
            ok = false;
            break;
        }
    }
    env.preds.insert((slot, id), ok);
    Ok(ok)
}

fn accept_slot(env: &mut FoldEnv<'_>, node: &NodePattern, slot: usize, id: NodeId) -> Result<bool> {
    // The canonical row-binding classification (ctx.rs): a row-bound node
    // accepts only itself, a null/non-node binding accepts nothing, an
    // absent or nameless variable accepts every node.
    if !binding(node, env.row).accepts(id) {
        return Ok(false);
    }
    if !node_ok(env.graph, id, node, env.row, env.params, env.cypher_version)? {
        return Ok(false);
    }
    slot_preds_hold(env, slot, id)
}

/// Fills `out` with this origin's accepted neighbors (cleared at entry,
/// so the caller's buffer is reused across calls without carrying
/// anything over — LABS-337's dealloc fix).
fn collect_neighbors(
    env: &FoldEnv<'_>,
    origin: NodeId,
    rel: &RelPattern,
    reverse: bool,
    skip_rel: Option<RelId>,
    out: &mut Vec<(RelId, NodeId)>,
) -> Result<()> {
    out.clear();
    let dir = if reverse {
        rel.direction.flip()
    } else {
        rel.direction
    };
    let graph = env.graph;
    graph.try_for_each_neighbor(origin, dir, &rel.types, |rid| {
        graph.cancel_tick()?;
        if skip_rel == Some(rid) {
            return Ok::<(), Error>(());
        }
        // Counted per relationship the walk traverses (the one counting
        // point, `count_neighbor_walk`'s home): after the skip filter, so
        // a re-visited edge the pattern excludes is not counted as a walk.
        super::super::count_neighbor_walk();
        // The canonical rel-property policy (eval/pattern.rs) — the same
        // filter `rel_edge_ok` applies for the one-hop fold.
        if rel.properties.is_some() {
            let Some(stored) = graph.rel(rid) else {
                return Ok::<(), Error>(());
            };
            if !rel_props_ok(graph, &stored, rel, env.row, env.params, env.cypher_version) {
                return Ok::<(), Error>(());
            }
        }
        if let Some(id) = graph.hop(origin, rid, dir) {
            out.push((rid, id));
        }
        Ok::<(), Error>(())
    })?;
    Ok(())
}

/// One end's bound-id resolution: the walk's explicit override (a
/// previously walked hop's bound id) wins, then the canonical row
/// binding (ctx.rs) — a bound node is the single candidate, a
/// null/non-node binding matches nothing (outer `None`), an absent or
/// nameless variable is free.
fn resolve_bound(over: Option<NodeId>, node: &NodePattern, row: &Row) -> Option<Option<NodeId>> {
    match over {
        Some(id) => Some(Some(id)),
        None => match binding(node, row) {
            Binding::Node(id) => Some(Some(id)),
            Binding::Mismatch => None,
            Binding::Unbound | Binding::Anonymous => Some(None),
        },
    }
}

fn expand_hop(env: &mut FoldEnv<'_>, walk: &HopWalk<'_>) -> Result<Vec<HopEnd>> {
    let start_node = &walk.path.nodes[walk.rel_idx];
    let end_node = &walk.path.nodes[walk.rel_idx + 1];
    let rel = &walk.path.rels[walk.rel_idx];
    let Some(bound_start) = resolve_bound(walk.bound_start, start_node, env.row) else {
        return Ok(Vec::new());
    };
    let Some(bound_end) = resolve_bound(walk.bound_end, end_node, env.row) else {
        return Ok(Vec::new());
    };
    // One exhaustive decision over the resolved bounds: which side anchors
    // the walk, the orientation it dictates, and the anchor list — produced
    // together, so the three cannot drift apart. The bound end anchors a
    // reversed walk (the neighbors are the starts), exactly as the starts
    // anchor the forward walk; walk_hop's own anchor acceptance handles an
    // unaccepted anchor (the verdict is memoized in env.preds).
    let start_side = HopSide {
        node: start_node,
        slot: walk.start_slot,
    };
    let end_side = HopSide {
        node: end_node,
        slot: walk.end_slot,
    };
    let (anchors, orientation) = match (bound_start, bound_end) {
        (Some(start_id), end_bound) => (
            vec![start_id],
            HopOrientation::Forward {
                anchor: start_side,
                target: end_side,
                bound_target: end_bound,
            },
        ),
        // The bound end is the single anchor of a reversed walk.
        (None, Some(end_id)) => (
            vec![end_id],
            HopOrientation::Reversed {
                anchor: end_side,
                target: start_side,
            },
        ),
        (None, None) => (
            // LABS-448: the seek — the fold's unanchored pattern starts.
            super::super::phase_seek(|| {
                node_candidates(
                    env.graph,
                    start_node,
                    env.row,
                    env.params,
                    env.cypher_version,
                    &Residual::with_conjuncts(&env.plan.conjuncts[walk.start_slot].exprs),
                )
            })?,
            HopOrientation::Forward {
                anchor: start_side,
                target: end_side,
                bound_target: None,
            },
        ),
    };
    walk_hop(
        env,
        HopRel {
            pattern: rel,
            skip: walk.skip_rel,
        },
        anchors,
        orientation,
    )
}

/// One side of an oriented hop expansion: the pattern and its slot.
struct HopSide<'a> {
    node: &'a NodePattern,
    slot: usize,
}

/// The hop's relationship: its pattern plus the DIFFERENT RELATIONSHIPS
/// skip.
struct HopRel<'a> {
    pattern: &'a RelPattern,
    skip: Option<RelId>,
}

/// The walk's orientation: which side anchors it and which is the target —
/// the two valid combinations of the walk's direction knobs, decided once
/// by the resolved bounds, so a mismatched triple cannot be constructed.
enum HopOrientation<'a> {
    /// The start side anchors; the ends are the walk's targets.
    Forward {
        anchor: HopSide<'a>,
        target: HopSide<'a>,
        /// The row binding the end side resolved to, filtering the ends.
        bound_target: Option<NodeId>,
    },
    /// The bound end anchors; the walk runs reversed and the starts are
    /// the targets (nothing filters them — the anchor IS the bound).
    Reversed {
        anchor: HopSide<'a>,
        target: HopSide<'a>,
    },
}

/// The one oriented hop expansion: each accepted anchor node's
/// relationships (collected forward, or reversed when the anchor is the
/// path's end), each neighbor accepted on the target side and emitted as a
/// `HopEnd` with the roles the orientation dictates. `bound_target`
/// filters the neighbor side when the pattern bound it.
fn walk_hop(
    env: &mut FoldEnv<'_>,
    rel: HopRel<'_>,
    anchors: impl IntoIterator<Item = NodeId>,
    orientation: HopOrientation<'_>,
) -> Result<Vec<HopEnd>> {
    let (anchor, reverse, target, bound_target) = match orientation {
        HopOrientation::Forward {
            anchor,
            target,
            bound_target,
        } => (anchor, false, target, bound_target),
        HopOrientation::Reversed { anchor, target } => (anchor, true, target, None),
    };
    let mut hops = Vec::new();
    // One neighbor buffer reused across the anchors: the previous
    // per-anchor `collect_neighbors` Vec was a dealloc hot spot on q19
    // (LABS-337).
    let mut found = Vec::new();
    for anchor_id in anchors {
        env.graph.cancel_tick()?;
        if !accept_slot(env, anchor.node, anchor.slot, anchor_id)? {
            continue;
        }
        collect_neighbors(env, anchor_id, rel.pattern, reverse, rel.skip, &mut found)?;
        for &(rid, other_id) in &found {
            if bound_target.is_some_and(|id| id != other_id) {
                continue;
            }
            if accept_slot(env, target.node, target.slot, other_id)? {
                // The orientation names the roles once: reversed, the
                // anchor is the hop's end.
                let (start, end) = if reverse {
                    (other_id, anchor_id)
                } else {
                    (anchor_id, other_id)
                };
                hops.push(HopEnd {
                    start,
                    end,
                    rel: rid,
                });
            }
        }
    }
    Ok(hops)
}

fn path_prop(graph: &ExecGraph<'_>, caches: &mut FoldCaches, id: NodeId, key: &str) -> Value {
    let props = caches.props.entry(id).or_default();
    if let Some(found) = props.get(key) {
        return found.clone();
    }
    let value = graph.node_prop(id, key).unwrap_or(Value::Null);
    props.insert(key.to_string(), value.clone());
    value
}

fn path_key(
    graph: &ExecGraph<'_>,
    plan: &PathPlan,
    caches: &mut FoldCaches,
    ids: &[NodeId],
) -> CachedKey {
    let vals: Vec<Value> = plan
        .items
        .iter()
        .filter_map(|item| match item {
            PathItem::Key { slot, prop } => Some(path_prop(graph, caches, ids[*slot], prop)),
            _ => None,
        })
        .collect();
    CachedKey::from_vals(vals)
}

/// One path-fold aggregate seat: the prototype paired with its value
/// source (`None` for `count(*)` and the argument-free `count()`,
/// `Some((slot, prop))` for a property read), built once by
/// [`fresh_path_aggs`] so no site re-walks the `PathItem` vocabulary.
type AggSeat = (StreamAgg, Option<(usize, String)>);

fn offer_aggs(env: &mut FoldEnv<'_>, aggs: &mut [AggSeat], ids: &[NodeId]) -> Result<()> {
    for (agg, source) in aggs.iter_mut() {
        let value = match source {
            None => None,
            Some((slot, prop)) => Some(path_prop(env.graph, env.caches, ids[*slot], prop)),
        };
        agg.offer_value(value.as_ref())?;
    }
    Ok(())
}

/// The allocate-fill-absorb glue every path shape runs: allocate the
/// slot ids, fill the shape's bound slots, then absorb the binding into
/// the groups — one match over the key plan yields the CachedKey, and
/// the group insert-or-find, offer, and note tail runs exactly once.
fn absorb(
    env: &mut FoldEnv<'_>,
    template: &[AggSeat],
    groups: &mut IndexMap<Vec<GroupKey>, (Vec<Value>, Vec<AggSeat>)>,
    bindings: &[(usize, NodeId)],
) -> Result<()> {
    let mut ids = vec![NodeId::from(0); env.plan.nslots];
    for (slot, id) in bindings {
        ids[*slot] = *id;
    }
    let cached = match env.plan.key_plan {
        KeyPlan::None => CachedKey::from_vals(Vec::new()),
        KeyPlan::One(slot) => {
            let id = ids[slot];
            match env.caches.keys.get(&id) {
                Some(cached) => cached.clone(),
                None => {
                    let cached = path_key(env.graph, env.plan, env.caches, &ids);
                    env.caches.keys.insert(id, cached.clone());
                    cached
                }
            }
        }
        KeyPlan::Mixed => path_key(env.graph, env.plan, env.caches, &ids),
    };
    let entry = groups
        .entry(cached.key)
        .or_insert_with(|| (cached.vals, template.to_vec()));
    offer_aggs(env, &mut entry.1, &ids)?;
    note_path_binding();
    Ok(())
}

fn project_path_row(
    items: &[Projection],
    spec: &[PathItem],
    key_vals: &[Value],
    aggs: &[AggSeat],
) -> Row {
    let mut row = Row::new();
    let mut key_idx = 0;
    let mut agg_idx = 0;
    for (i, item) in spec.iter().enumerate() {
        let name = items[i].output_name(i);
        match item {
            PathItem::Key { .. } => {
                row.insert(name, key_vals[key_idx].clone());
                key_idx += 1;
            }
            PathItem::CountStar | PathItem::Count { .. } | PathItem::CountDistinct { .. } => {
                row.insert(name, aggs[agg_idx].0.finish());
                agg_idx += 1;
            }
        }
    }
    row
}

/// The path fold's aggregate template: one classification pass over the
/// `PathItem` vocabulary pairs each prototype with its value source, so
/// `offer_aggs` walks this structure instead of re-matching the items.
fn fresh_path_aggs(items: &[PathItem]) -> Vec<AggSeat> {
    items
        .iter()
        .filter_map(|item| match item {
            PathItem::CountStar => Some((StreamAgg::count_star(), None)),
            PathItem::Count { slot, prop } => {
                Some((StreamAgg::count(), Some((*slot, prop.clone()))))
            }
            PathItem::CountDistinct { slot, prop } => {
                Some((StreamAgg::count_distinct(), Some((*slot, prop.clone()))))
            }
            PathItem::Key { .. } => None,
        })
        .collect()
}

/// Walk the compiled hop steps, threading the slot→id binding map: each
/// step anchors on the slots earlier steps bound (`expand_hop`'s bound
/// ends), every accepted hop extends the map, the previous hop's rel is
/// `skip_rel` (the DIFFERENT RELATIONSHIPS rule), and the last step's full
/// binding is absorbed.
fn fold_steps(
    env: &mut FoldEnv<'_>,
    patterns: &[PathPattern],
    step_idx: usize,
    skip_rel: Option<RelId>,
    bindings: &mut [Option<NodeId>],
    template: &[AggSeat],
    groups: &mut IndexMap<Vec<GroupKey>, (Vec<Value>, Vec<AggSeat>)>,
) -> Result<()> {
    let Some(step) = env.plan.steps.get(step_idx) else {
        let full: Vec<(usize, NodeId)> = bindings
            .iter()
            .enumerate()
            .filter_map(|(slot, id)| id.map(|id| (slot, id)))
            .collect();
        return absorb(env, template, groups, &full);
    };
    let hops = expand_hop(
        env,
        &HopWalk {
            path: &patterns[step.pattern],
            rel_idx: step.rel,
            start_slot: step.start_slot,
            end_slot: step.end_slot,
            bound_start: bindings[step.start_slot],
            bound_end: bindings[step.end_slot],
            skip_rel,
        },
    )?;
    // Each accepted hop sets the two slots and restores them after the
    // deeper fold: a deeper step's binding must not leak back as an
    // anchor when the walk backtracks to this step's next hop. The
    // save/restore replaces a per-hop `bindings.to_vec()` — the profiled
    // dealloc hot spot on q19 (LABS-337): one Vec allocation per hop for
    // the same isolation the two saved values provide.
    for hop in hops {
        let saved = (bindings[step.start_slot], bindings[step.end_slot]);
        bindings[step.start_slot] = Some(hop.start);
        bindings[step.end_slot] = Some(hop.end);
        // Restore before propagating, unconditionally: the slot
        // isolation is local to the loop, never left to a caller to
        // honor on the error path.
        let deeper = fold_steps(
            env,
            patterns,
            step_idx + 1,
            Some(hop.rel),
            bindings,
            template,
            groups,
        );
        bindings[step.start_slot] = saved.0;
        bindings[step.end_slot] = saved.1;
        deeper?;
    }
    Ok(())
}

/// Stream one fixed hop, a fixed two-hop path, or two comma-joined one-hop
/// patterns into group counts or a distinct set. Wider patterns, variable-length
/// walks, repeated relationship variables, and `sum` stay on match-then-aggregate.
pub(crate) fn try_fold_path_aggs(
    graph: &ExecGraph<'_>,
    rows: &[Row],
    patterns: &[PathPattern],
    where_clause: Option<&Expr>,
    suffix: &[Clause],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Option<FoldedReturn>> {
    let Some(ret) = fold_return(suffix) else {
        return Ok(None);
    };
    let Some(plan) = compile_path(patterns, where_clause, ret.items) else {
        return Ok(None);
    };
    let output = FoldOutput {
        items: ret.items,
        order: ret.order,
        skip: ret.skip,
        limit: ret.limit,
        params,
        cypher_version,
    };
    let template = fresh_path_aggs(&plan.items);
    let mut groups: IndexMap<Vec<GroupKey>, (Vec<Value>, Vec<AggSeat>)> = IndexMap::new();
    let mut caches = FoldCaches {
        props: rustc_hash::FxHashMap::default(),
        keys: rustc_hash::FxHashMap::default(),
    };
    for row in rows {
        // The env is per-row: the row-scoped `preds` map starts empty by
        // construction, while the graph-scoped `caches` maps stay warm
        // across rows.
        let mut env = FoldEnv {
            graph,
            row,
            params,
            cypher_version,
            plan: &plan,
            caches: &mut caches,
            preds: rustc_hash::FxHashMap::default(),
        };
        let mut bindings = vec![None; plan.nslots];
        fold_steps(
            &mut env,
            patterns,
            0,
            None,
            &mut bindings,
            &template,
            &mut groups,
        )?;
    }
    // No accepted binding: `groups` stayed empty — `absorb`
    // inserts-or-finds a group entry on every call, so the table carries
    // the fact. `key_plan` carries the seed-group fact: `None` means
    // every item is an aggregate, so the seed group answers them; any key
    // plan means a key column exists and the empty tail below yields the
    // empty row set — one exit, no parallel early return.
    if groups.is_empty() && matches!(plan.key_plan, KeyPlan::None) {
        groups.insert(Vec::new(), (Vec::new(), template));
    }
    let out = groups
        .into_values()
        .map(|(key_vals, aggs)| project_path_row(output.items, &plan.items, &key_vals, &aggs))
        .collect();
    Ok(Some(finish_folded_rows(graph, &output, out)?))
}
