//! The seek vocabulary and its recognizers: which single-index lookup
//! (if any) answers a node pattern's residual predicate — TEXT contains /
//! prefix, RANGE bounds, SPATIAL, and the `id()`/`elementId()` direct
//! seeks — the family `candidates`' funnel (`leaf_seek`) and the
//! relationship twin consult.

use std::collections::BTreeMap;

use cypherlite_core::{NodeId, Value};
use cypherlite_ir::{BinaryOp, Expr};

use crate::eval::{eval, orient, Row};
use crate::graph::{RangeSeek, SpatialSeek};
use crate::overlay::ExecGraph;

#[derive(Clone)]
pub(crate) enum StringSeek {
    Contains(String),
    Prefix(String),
}

/// One index-answerable predicate a conjunct places on the node being
/// matched. Collected from every conjunct, ranked, and the first one an
/// index serves wins; the residual re-checks the predicate either way.
#[derive(Clone)]
pub(crate) enum Seek {
    /// `id(n) = …` / `id(n) IN …` / `n IN [nodes…]` (LABS-250): ids
    /// answer directly from storage or the list, so this needs no index
    /// and no static label conjunction.
    Ids(Vec<NodeId>),
    /// `CONTAINS` / `ENDS WITH` (TEXT index) and `STARTS WITH` (RANGE index).
    Text(String, StringSeek),
    /// `=`, `IN`, and comparisons (RANGE index).
    Range(String, RangeSeek),
    /// `point.distance` / `point.withinBBox` (POINT index).
    Spatial(String, SpatialSeek),
}

impl Seek {
    /// Preference when several conjuncts seek: a direct id lookup, then
    /// text, range, and spatial. Stable, so conjunct order breaks ties.
    pub(super) fn rank(&self) -> u8 {
        match self {
            Seek::Ids(_) => 0,
            Seek::Text(..) => 1,
            Seek::Range(..) => 2,
            Seek::Spatial(..) => 3,
        }
    }

    /// Whether this seek is usable at all under a `label_seekable` label
    /// criterion: an index-backed seek is only servable under a static
    /// label conjunction; a non-indexed seek (the scan fallback) is usable
    /// either way. The whole policy lives here — the funnels consult it,
    /// none re-assembles the compound guard.
    pub(crate) fn servable(&self, label_seekable: bool) -> bool {
        !self.indexed() || label_seekable
    }

    /// Served by a declared index over the pattern labels. `Ids` is
    /// answered by storage and is valid under any label expression.
    pub(crate) fn indexed(&self) -> bool {
        !matches!(self, Seek::Ids(_))
    }

    /// Candidate ids, or `None` when no index serves this seek (the TEXT
    /// index also declines an empty `CONTAINS` needle).
    pub(crate) fn serve(&self, graph: &ExecGraph<'_>, labels: &[String]) -> Option<Vec<NodeId>> {
        match self {
            Seek::Ids(ids) => Some(
                ids.iter()
                    .copied()
                    .filter(|&id| graph.node_exists(id))
                    .collect(),
            ),
            Seek::Text(key, StringSeek::Contains(n)) => {
                graph.lookup_string_contains(labels, key, n)
            }
            Seek::Text(key, StringSeek::Prefix(n)) => graph.lookup_string_prefix(labels, key, n),
            Seek::Range(key, seek) => graph.lookup_range(labels, key, seek),
            Seek::Spatial(key, seek) => graph.lookup_spatial(labels, key, seek),
        }
    }

    /// `Some(n)` is an upper bound on the ids this seek would materialize.
    /// `None` when no index serves it (text and spatial seeks included).
    pub(crate) fn cardinality(&self, graph: &ExecGraph<'_>, labels: &[String]) -> Option<usize> {
        match self {
            Seek::Ids(ids) => Some(ids.len()),
            Seek::Range(key, seek) => graph.lookup_range_cardinality(labels, key, seek),
            Seek::Text(..) | Seek::Spatial(..) => None,
        }
    }
}

/// The seek one conjunct expresses on `var`, if any. The four recognizers
/// are disjoint: an ids seek has `id()`/`elementId()` or the bare
/// variable itself (LABS-250's `n IN [nodes…]`) on the left, a text or
/// range seek a property, a spatial seek a point function.
pub(crate) fn leaf_seek(
    var: &str,
    expr: &Expr,
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Option<Seek> {
    if let Some(ids) = ids_seek_expr(var, expr, graph, row, params, cypher_version) {
        return Some(Seek::Ids(ids));
    }
    if let Some((key, seek)) = string_seek_expr(var, expr, graph, row, params, cypher_version) {
        return Some(Seek::Text(key, seek));
    }
    if let Some((key, seek)) = range_seek_expr(var, expr, graph, row, params, cypher_version) {
        return Some(Seek::Range(key, seek));
    }
    point_seek_expr(var, expr, graph, row, params, cypher_version)
        .map(|(key, seek)| Seek::Spatial(key, seek))
}

fn string_seek_expr(
    var: &str,
    expr: &Expr,
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Option<(String, StringSeek)> {
    let Expr::Binary { op, left, right } = expr else {
        return None;
    };
    // Classify once, binding the constructor before any paid work: the
    // `_` arm is the decline gate for every other op.
    let variant: fn(String) -> StringSeek = match op {
        BinaryOp::Contains | BinaryOp::EndsWith => StringSeek::Contains,
        BinaryOp::StartsWith => StringSeek::Prefix,
        _ => return None,
    };
    // The var's property on the left — the one canonical recognizer.
    let key = prop_key(left, var)?;
    let val = eval(graph, row, right, params, cypher_version).ok()?;
    let s = val.as_string()?.to_string();
    // Servability has one owner: the gram index itself declines an empty
    // needle (lookup_string_contains returns None), and node_candidates
    // falls through to its shared scan tail — `CONTAINS ''` stays correct.
    Some((key, variant(s)))
}

fn range_seek_expr(
    var: &str,
    expr: &Expr,
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Option<(String, RangeSeek)> {
    let Expr::Binary { op, left, right } = expr else {
        return None;
    };
    let (key, bound_expr, op) = orient(|e| prop_key(e, var), left, right, *op)?;
    let bound = eval(graph, row, bound_expr, params, cypher_version).ok()?;
    // A null bound is never servable. A bound that references the pattern
    // variable itself (`c.key = c.cluster`) or a variable bound later in
    // the pattern evaluates against a row where that variable is unbound —
    // null — and a served seek silently renders the whole pattern empty
    // instead of declining (the LABS-328 regression: the entity-resolution
    // report cell's `WHERE c.key = c.cluster` returned 0 rows once the
    // constraint's backing index materialized). Null never satisfies an
    // equality or a comparison, so declining to the scan tail plus the
    // residual re-applying the predicate per candidate row is the same
    // answer, correctly reached.
    if bound.is_null() {
        return None;
    }
    match op {
        BinaryOp::Eq => Some((key, RangeSeek::Eq(bound))),
        BinaryOp::In => {
            let Value::List(xs) = bound else {
                return None;
            };
            Some((key, RangeSeek::In(xs)))
        }
        BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Lte | BinaryOp::Gte => {
            Some((key, RangeSeek::Cmp { op, bound }))
        }
        _ => None,
    }
}

/// The var's own property key — `key_of` in owned form.
fn prop_key(expr: &Expr, var: &str) -> Option<String> {
    crate::eval::key_of(expr, var).map(str::to_string)
}

fn point_seek_expr(
    var: &str,
    expr: &Expr,
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Option<(String, SpatialSeek)> {
    match expr {
        Expr::Function { name, args, .. }
            if name.eq_ignore_ascii_case("point.withindistance") && args.len() == 3 =>
        {
            let (key, origin) =
                point_prop_and_origin(var, &args[0], &args[1], graph, row, params, cypher_version)?;
            let max = eval(graph, row, &args[2], params, cypher_version)
                .ok()?
                .as_number()?;
            Some((
                key,
                SpatialSeek::Distance {
                    origin,
                    max,
                    inclusive: true,
                },
            ))
        }
        Expr::Function { name, args, .. }
            if name.eq_ignore_ascii_case("point.withinbbox") && args.len() == 3 =>
        {
            let key = prop_key(&args[0], var)?;
            let lo = eval(graph, row, &args[1], params, cypher_version).ok()?;
            let hi = eval(graph, row, &args[2], params, cypher_version).ok()?;
            Some((key, SpatialSeek::BBox { lo, hi }))
        }
        Expr::Binary { op, left, right } => {
            let (func, bound_expr, inclusive) = distance_cmp(left, right, *op)?;
            let Expr::Function { name, args, .. } = func else {
                return None;
            };
            if !name.eq_ignore_ascii_case("point.distance")
                && !name.eq_ignore_ascii_case("distance")
            {
                return None;
            }
            if args.len() != 2 {
                return None;
            }
            let (key, origin) =
                point_prop_and_origin(var, &args[0], &args[1], graph, row, params, cypher_version)?;
            let max = eval(graph, row, bound_expr, params, cypher_version)
                .ok()?
                .as_number()?;
            Some((
                key,
                SpatialSeek::Distance {
                    origin,
                    max,
                    inclusive,
                },
            ))
        }
        _ => None,
    }
}

fn distance_cmp<'a>(
    left: &'a Expr,
    right: &'a Expr,
    op: BinaryOp,
) -> Option<(&'a Expr, &'a Expr, bool)> {
    match op {
        BinaryOp::Lt => Some((left, right, false)),
        BinaryOp::Lte => Some((left, right, true)),
        BinaryOp::Gt => Some((right, left, false)),
        BinaryOp::Gte => Some((right, left, true)),
        _ => None,
    }
}

fn point_prop_and_origin(
    var: &str,
    a: &Expr,
    b: &Expr,
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Option<(String, Value)> {
    // The distance functions are operand-symmetric — no op to mirror, so
    // only the side-selection core applies: the side carrying the var's
    // property decides, the other side is the origin.
    let (key, origin_expr) = crate::eval::var_key_side(var, a, b)?;
    let origin = eval(graph, row, origin_expr, params, cypher_version).ok()?;
    Some((key.to_string(), origin))
}

/// An id conversion selected by a seek recognizer's left shape: a
/// value to a node or rel id (`NodeId`/`RelId` are both `i64`).
type IdConversion = Box<dyn Fn(&Value) -> Option<i64>>;

/// The left-shape dispatch shared by the node and rel id seeks: a
/// single-argument `id(var)` or `elementId(var)` call selects the id
/// conversion — integer for `id()`, the engine's self-encoding element
/// id string for `elementId()` (`parse_element_id` picks the node or
/// rel namespace). Any other left shape declines.
pub(super) fn id_fn_conversion(
    var: &str,
    left: &Expr,
    parse_element_id: fn(&str) -> Option<i64>,
) -> Option<IdConversion> {
    let Expr::Function { name, args, .. } = left else {
        return None;
    };
    if args.len() != 1 {
        return None;
    }
    let Expr::Variable(v) = &args[0] else {
        return None;
    };
    if v != var {
        return None;
    }
    // The conversion is chosen once from the function name.
    if name.eq_ignore_ascii_case("id") {
        Some(Box::new(Value::as_int))
    } else if name.eq_ignore_ascii_case("elementid") {
        Some(Box::new(move |v: &Value| {
            v.as_string().and_then(parse_element_id)
        }))
    } else {
        None
    }
}

/// The ids recognizer: one seek whose id conversion is selected by the
/// left shape — `id(n) =/IN …` (integer), `elementId(n) =/IN …` (the
/// `4:cypherlite:n<id>` string form), or the bare `n IN [nodes…]`
/// (LABS-250, the node-value conversion). The shapes are disjoint, so
/// at most one conversion applies per conjunct.
fn ids_seek_expr(
    var: &str,
    expr: &Expr,
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Option<Vec<NodeId>> {
    let Expr::Binary { op, left, right } = expr else {
        return None;
    };
    // One classification of `op`: the Eq arm serves the id-function
    // shapes, the In arm those plus the bare node variable (Eq on the
    // bare variable is not this recognizer's shape); every other op
    // declines before paying an eval.
    let to_id: IdConversion = match (op, left.as_ref()) {
        (BinaryOp::Eq | BinaryOp::In, Expr::Function { .. }) => {
            id_fn_conversion(var, left, cypherlite_core::parse_node_element_id)?
        }
        // The bare node variable: IN over node values.
        (BinaryOp::In, Expr::Variable(v)) if v == var => Box::new(|elem: &Value| match elem {
            Value::Node(n) => Some(n.id),
            _ => None,
        }),
        _ => return None,
    };
    let val = eval(graph, row, right, params, cypher_version).ok()?;
    match op {
        BinaryOp::Eq => to_id(&val).map(|id| vec![id]),
        BinaryOp::In => in_list_ids(&val, to_id),
        _ => None,
    }
}

/// The `IN`-seek tail shared by every left shape (id()/elementId() and the
/// bare node variable, LABS-250): a list value mapped through the id
/// conversion, deduped — a duplicate list element is a semantic regression
/// (a duplicate row), not a harmless extra. A non-list declines the seek;
/// the residual re-evaluates the predicate anyway — null matches nothing,
/// any other non-list errors with InvalidArgumentType.
fn in_list_ids(val: &Value, to_id: impl Fn(&Value) -> Option<NodeId>) -> Option<Vec<NodeId>> {
    let Value::List(xs) = val else {
        return None;
    };
    let mut ids: Vec<NodeId> = xs.iter().filter_map(to_id).collect();
    ids.sort_unstable();
    ids.dedup();
    Some(ids)
}
