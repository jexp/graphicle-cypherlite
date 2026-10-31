//! Node candidate selection for `MATCH`: which index, if any, answers a
//! node pattern's residual predicates, and the label/property scan it
//! falls back to. Relationship expansion lives in `expand`.

use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{NodeId, RelId, Result, Value};
use cypherlite_ir::{BinaryOp, Expr, NodePattern, RelPattern};

use crate::eval::{eval, expr_vars, label_expr_matches, Row};
use crate::overlay::ExecGraph;

use super::create::map_from_expr;
use super::pushdown::Residual;

mod seek;

use seek::id_fn_conversion;
pub(crate) use seek::{leaf_seek, Seek};

/// The relationship twin of [`ids_seek_expr`] (LABS-161): `id(r) = <expr>`
/// (integer) or `elementId(r) = <expr>` (the `5:cypherlite:r<id>` string
/// form, which self-encodes the RelId) on the relationship variable `var`
/// resolves the RelId directly, so the walk can seek the id-keyed
/// relationship storage instead of walking every node's adjacency. Only
/// equality — an `IN` list stays a walk (the residual re-evaluates the
/// predicate either way, so a declined seek never changes the result).
pub(crate) fn rel_seek_expr(
    var: &str,
    expr: &Expr,
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Option<RelId> {
    let Expr::Binary { op, left, right } = expr else {
        return None;
    };
    if !matches!(op, BinaryOp::Eq) {
        return None;
    }
    let to_id = id_fn_conversion(var, left, cypherlite_core::parse_rel_element_id)?;
    let val = eval(graph, row, right, params, cypher_version).ok()?;
    to_id(&val)
}

/// Outcome of the shared LABS-161 seek decode ([`resolve_rel_seek`]),
/// before the caller applies its own anchoring policy.
pub(crate) enum SeekedRel {
    /// No seek applies: the caller enumerates the adjacency.
    Decline,
    /// The resolved rel id cannot match: not visible (`rel_ends` misses),
    /// or rejected by the pattern's static type filter.
    Gone,
    /// The rel id with both raw endpoints; orientation is the caller's.
    Resolved {
        start: NodeId,
        end: NodeId,
        rid: RelId,
    },
}

/// The decode core shared by the walk's first-hop seek (`first_rel_seek`
/// in `expand`) and the one-hop aggregate fold: resolve the row-bound rel
/// or the `elementId(r) = …` / `id(r) = …` equality conjunct to a RelId,
/// check visibility via `rel_ends`, and apply the pattern's static type
/// filter (the adjacency enumeration applies it while scanning, so the
/// seek applies it in place). Declines for a var-length or anonymous rel,
/// a null or non-relationship row binding (the pre-bound reconciliation
/// keeps its existing behavior), or no equality conjunct. The residual
/// re-evaluates the predicate either way, so the decode narrows the
/// search, never the result.
pub(crate) fn resolve_rel_seek(
    graph: &ExecGraph<'_>,
    rp: &RelPattern,
    conjuncts: &[Expr],
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> SeekedRel {
    if rp.var_length {
        return SeekedRel::Decline;
    }
    let Some(v) = rp.variable.as_ref() else {
        return SeekedRel::Decline;
    };
    let rid = match row.get(v) {
        Some(Value::Relationship(r)) => r.id,
        Some(_) => return SeekedRel::Decline,
        None => match conjuncts
            .iter()
            .find_map(|c| rel_seek_expr(v, c, graph, row, params, cypher_version))
        {
            Some(rid) => rid,
            None => return SeekedRel::Decline,
        },
    };
    let Some((start, end)) = graph.rel_ends(rid) else {
        return SeekedRel::Gone;
    };
    if !static_rel_type_ok(graph, rid, &rp.types) {
        return SeekedRel::Gone;
    }
    SeekedRel::Resolved { start, end, rid }
}

/// The pattern's static type filter for one rel, spelled once for both
/// users: the walk's per-hop check in `expand` re-checks already-scanned
/// neighbors with it (one cheap comparison), and the LABS-161 rel seek
/// applies it in place of the adjacency enumeration's filter. An empty
/// type list matches any rel; a rel with no resolvable type or none of
/// the pattern's types does not match.
pub(crate) fn static_rel_type_ok(graph: &ExecGraph<'_>, rid: RelId, types: &[String]) -> bool {
    types.is_empty()
        || graph
            .rel_type(rid)
            .is_some_and(|rel_type| types.iter().any(|t| t == &rel_type))
}

/// The label criterion is static (then `np.labels` is its exact operand
/// list) or dynamic: one spelling, consulted by the seek funnel and the
/// fallback scan alike.
pub(crate) fn label_index_seekable(node: &NodePattern) -> bool {
    node.label_expr
        .as_ref()
        .is_none_or(|expr| expr.is_static_conjunction())
}

/// Outcome of trying to answer one boolean node with indexes.
///
/// `Served` is a candidate list (possibly empty). `Unserved` is a recognized
/// seek whose index declined (`None`), which an `AND` may skip and an `OR`
/// may not. `Decline` abandons the whole union: the leaf is not a seek, or
/// it names a variable other than the node being expanded.
enum IndexArm {
    Served(Vec<NodeId>),
    Unserved,
    Decline,
}

fn or_union_candidates(
    var: &str,
    residual: &Residual<'_>,
    graph: &ExecGraph<'_>,
    labels: &[String],
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Option<Vec<NodeId>> {
    let mut acc: Option<HashSet<NodeId>> = None;
    for c in residual.conjuncts {
        if !matches!(
            c,
            Expr::Binary {
                op: BinaryOp::Or,
                ..
            }
        ) {
            continue;
        }
        let IndexArm::Served(ids) = index_arm(var, c, graph, labels, row, params, cypher_version)
        else {
            continue;
        };
        let next: HashSet<NodeId> = ids.into_iter().collect();
        acc = Some(match acc.take() {
            None => next,
            Some(prev) => prev.intersection(&next).copied().collect(),
        });
    }
    acc.map(|set| {
        let mut ids: Vec<_> = set.into_iter().collect();
        ids.sort_unstable();
        ids
    })
}

fn index_arm(
    var: &str,
    expr: &Expr,
    graph: &ExecGraph<'_>,
    labels: &[String],
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> IndexArm {
    match expr {
        Expr::Binary {
            op: BinaryOp::Or,
            left,
            right,
        } => match (
            index_arm(var, left, graph, labels, row, params, cypher_version),
            index_arm(var, right, graph, labels, row, params, cypher_version),
        ) {
            (IndexArm::Decline, _) | (_, IndexArm::Decline) => IndexArm::Decline,
            (IndexArm::Served(a), IndexArm::Served(b)) => IndexArm::Served(union_ids(a, b)),
            _ => IndexArm::Unserved,
        },
        Expr::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => match (
            index_arm(var, left, graph, labels, row, params, cypher_version),
            index_arm(var, right, graph, labels, row, params, cypher_version),
        ) {
            (IndexArm::Decline, _) | (_, IndexArm::Decline) => IndexArm::Decline,
            (IndexArm::Served(a), IndexArm::Served(b)) => IndexArm::Served(intersect_ids(a, b)),
            (IndexArm::Served(ids), IndexArm::Unserved)
            | (IndexArm::Unserved, IndexArm::Served(ids)) => IndexArm::Served(ids),
            (IndexArm::Unserved, IndexArm::Unserved) => IndexArm::Unserved,
        },
        other => index_leaf(var, other, graph, labels, row, params, cypher_version),
    }
}

fn index_leaf(
    var: &str,
    expr: &Expr,
    graph: &ExecGraph<'_>,
    labels: &[String],
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> IndexArm {
    let mut vars = HashSet::new();
    expr_vars(expr, &mut vars);
    if vars.len() != 1 || !vars.contains(var) {
        return IndexArm::Decline;
    }
    // ENDS WITH has no reverse-key index. `string_seek_expr` maps it to
    // `StringSeek::Contains`: every string that ends with the suffix contains
    // it, and the residual applies the real ENDS WITH. No text index, or an
    // empty suffix, is Unserved so an AND sibling can still seed the arm.
    let Some(seek) = leaf_seek(var, expr, graph, row, params, cypher_version) else {
        return IndexArm::Decline;
    };
    seek.serve(graph, labels)
        .map_or(IndexArm::Unserved, IndexArm::Served)
}

fn retain_equality(
    graph: &ExecGraph<'_>,
    labels: &[String],
    props: &BTreeMap<String, Value>,
    ids: &mut Vec<NodeId>,
) {
    if props.is_empty() {
        return;
    }
    let eq: HashSet<NodeId> = graph
        .lookup_nodes(labels, props)
        .expect("props non-empty: lookup_nodes always serves")
        .into_iter()
        .collect();
    ids.retain(|id| eq.contains(id));
}

fn union_ids(a: Vec<NodeId>, b: Vec<NodeId>) -> Vec<NodeId> {
    let mut set: HashSet<NodeId> = a.into_iter().collect();
    set.extend(b);
    let mut ids: Vec<_> = set.into_iter().collect();
    ids.sort_unstable();
    ids
}

fn intersect_ids(a: Vec<NodeId>, b: Vec<NodeId>) -> Vec<NodeId> {
    let right: HashSet<NodeId> = b.into_iter().collect();
    let mut ids: Vec<_> = a.into_iter().filter(|id| right.contains(id)).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

pub(crate) fn node_candidates(
    graph: &ExecGraph<'_>,
    np: &NodePattern,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    residual: &Residual<'_>,
) -> Result<Vec<NodeId>> {
    // One node-store read per candidate the match examines (LABS-375):
    // counted HERE, the one funnel every node-match path reads (match_node,
    // the cheapest-start expansion, gpm start nodes, the agg folds, the
    // shortest-route and eval paths), so a scan or index seek reports its
    // reads in PROFILE Db Hits wherever it runs.
    let candidates = node_candidates_uncounted(graph, np, row, params, cypher_version, residual)?;
    super::count_node_reads(candidates.len() as u64);
    Ok(candidates)
}

fn node_candidates_uncounted(
    graph: &ExecGraph<'_>,
    np: &NodePattern,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
    residual: &Residual<'_>,
) -> Result<Vec<NodeId>> {
    // Seek strategies need the pattern's property map and a static label
    // conjunction (a dynamic label expression cannot be pre-filtered by an
    // index). Both are computed lazily and exactly once per strategy that
    // needs them.
    let props = || -> Result<BTreeMap<String, Value>> {
        match &np.properties {
            Some(e) => map_from_expr(graph, row, e, params, cypher_version),
            None => Ok(BTreeMap::new()),
        }
    };
    let label_seekable = label_index_seekable(np);

    if let Some(v) = &np.variable {
        match row.get(v) {
            Some(Value::Node(n)) => return Ok(vec![n.id]),
            // null and non-node bindings match nothing (match_node's
            // binding check rejects them all; short-circuit here)
            Some(_) => return Ok(Vec::new()),
            None => {}
        }
        // Every conjunct that is a seek on this variable, cheapest first.
        // The first one an index serves answers the pattern; an unserved
        // seek (no index, empty CONTAINS needle) yields to the next. The
        // residual still applies every predicate.
        let mut seeks: Vec<Seek> = residual
            .conjuncts
            .iter()
            .filter_map(|c| leaf_seek(v, c, graph, row, params, cypher_version))
            .collect();
        seeks.sort_by_key(Seek::rank);
        for seek in &seeks {
            if !seek.servable(label_seekable) {
                continue;
            }
            if let Some(mut ids) = seek.serve(graph, &np.labels) {
                if seek.indexed() {
                    // props non-empty: lookup_nodes always serves (its
                    // only None is the empty-seek case).
                    retain_equality(graph, &np.labels, &props()?, &mut ids);
                }
                return Ok(ids);
            }
        }
        if label_seekable {
            // An OR conjunct is a candidate set only when every arm is served.
            // One declined arm (missing index, empty CONTAINS needle, NOT,
            // XOR, or a leaf that is not exactly this variable) abandons
            // that conjunct. ENDS WITH is served by a text index as a
            // contains-superset. Several served OR conjuncts are AND
            // factors, so their id lists are intersected.
            if let Some(mut ids) =
                or_union_candidates(v, residual, graph, &np.labels, row, params, cypher_version)
            {
                retain_equality(graph, &np.labels, &props()?, &mut ids);
                return Ok(ids);
            }
        }
    }
    if label_seekable {
        if let Some(ids) = graph.lookup_nodes(&np.labels, &props()?) {
            return Ok(ids);
        }
    }
    Ok(graph.all_node_ids())
}

pub(crate) fn node_ok(
    graph: &ExecGraph<'_>,
    id: NodeId,
    np: &NodePattern,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<bool> {
    // One visibility read per path, and label checks without
    // materializing the label list (LABS-347, stellar F-1-17/F-1-36).
    match &np.label_expr {
        // The common case: even a plain `MATCH (n:Person)` carries a
        // synthesized static conjunction. Checking each label by name
        // pays one interner lookup per (candidate, label) — no
        // `Vec<String>` materialization, the q19 hot-path cost.
        // A static conjunction implies a non-empty label list at every
        // construction site (the planner's `add_labels` rebuilds
        // `label_expr` as `And(...)` over exactly `n.labels`; nothing
        // builds `And([])`), so the guard needs no labels cross-check —
        // the same single criterion `label_index_seekable` uses.
        Some(expr) if expr.is_static_conjunction() => {
            // `node_has_label` answers `None` for an invisible node —
            // which fails the check exactly like an absent label.
            if !np
                .labels
                .iter()
                .all(|l| matches!(graph.node_has_label(id, l), Some(true)))
            {
                return Ok(false);
            }
        }
        // Genuinely dynamic label expressions need the labels as
        // strings; `node_labels` returning None is this path's
        // visibility read (invisible nodes have no labels).
        Some(expr) => {
            let Some(labels) = graph.node_labels(id) else {
                return Ok(false);
            };
            if !label_expr_matches(graph, row, params, expr, &labels, false, cypher_version)? {
                return Ok(false);
            }
        }
        // No label expression at all: bare visibility.
        None => {
            if !graph.node_exists(id) {
                return Ok(false);
            }
        }
    }
    if let Some(p) = &np.properties {
        let want = map_from_expr(graph, row, p, params, cypher_version)?;
        for (k, v) in want {
            // The string-keyed read: `node_prop` runs one interner
            // lookup per (candidate, key) — the same count any wrapper
            // of it pays — and the scratch (uncommitted) map it reads is
            // string-keyed by construction.
            match graph.node_prop(id, &k) {
                Some(have) if have.eq_non_null(&v) => {}
                _ => return Ok(false),
            }
        }
    }
    Ok(true)
}
