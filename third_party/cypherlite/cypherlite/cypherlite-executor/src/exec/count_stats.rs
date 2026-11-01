//! LABS-370 (v1): the count-only statistics short-circuit — the shape
//! recognizer that answers a whole-population count from the maintained
//! statistics instead of scanning its population. Declared beside
//! `count_stats_tests`, its contract tests (ADR-0387).

use std::collections::HashSet;

use cypherlite_core::Value;
use cypherlite_ir::{
    classify_aggregate, AggregateKind, Clause, Direction, Expr, MatchMode, Projection,
};

use crate::eval::Row;
use crate::graph::{CountProbe, QueryStats};
use crate::overlay::ExecGraph;

use super::{label_expr_ok, QueryResult};

/// The pattern node the single-label statistics can answer: exactly one
/// label, no properties, no label expression the counts cannot mirror, no
/// inline WHERE (a statically mirroring label expression — the raw
/// front-lowered shape — is accepted, round 4, F-4-1, through the
/// canonical static-vs-dynamic discrimination shared with the bulk
/// recognizer; a
/// multi-label node needs label-set intersections the per-label counts
/// cannot answer; a property, dynamic label or predicate needs the scan's
/// filters — round 3, F-1-3: every predicate-bearing field on the
/// pattern types is enumerated, none silently accepted).
fn single_plain_label(np: &cypherlite_ir::NodePattern) -> Option<&str> {
    (np.labels.len() == 1
        && np.properties.is_none()
        && label_expr_ok(&np.label_expr, &np.labels)
        && np.where_clause.is_none())
    .then(|| np.labels[0].as_str())
}

/// LABS-370 (v1): the count-only shape recognizer (LABS-425, the round-6
/// decomposition of the round-1 F-1-4 finding — the recognizer, the
/// gates, and the answer are three concerns in three places, not one
/// 160-line function). Deliberately conservative — every shape it does
/// not recognize returns `None` and the caller falls back to the clause
/// loop's scan:
/// exactly `[Match, Return]`; the MATCH is non-optional, the default
/// match mode, one pattern, no WHERE, no search; the pattern carries no
/// path-level predicate either (no path WHERE, mode or selector — round
/// 3, F-1-3: every predicate-bearing field on the IR's pattern types is
/// enumerated, none silently accepted); the pattern's nodes carry
/// exactly one label each and no properties, dynamic label expressions
/// or inline WHERE (multi-label nodes would need label-set intersections
/// the single-label counts cannot answer); its rel — when present — is
/// single-hop, single-type, property-free, predicate-free and directed
/// (`Both` counts both orientations, which the one-directional pair
/// counts cannot answer); the pattern's node variables do not repeat
/// (round 3, F-1-4: a same-variable self-loop shape would answer from
/// the pair count, which includes distinct-endpoint rels the scan
/// excludes); the RETURN projects exactly one `count(*)` or `count(var)`
/// with no DISTINCT, grouping, ORDER BY, SKIP or LIMIT. `count(var)`
/// equals `count(*)` when the pattern binds the variable on every row
/// it emits — the binding check is recognition, so it lives here.
fn recognize_count_shape(clauses: &[Clause]) -> Option<(CountProbe<'_>, &Projection)> {
    if clauses.len() != 2 {
        return None;
    }
    let (
        Clause::Match {
            optional: false,
            match_mode: MatchMode::DifferentRelationships,
            patterns,
            where_clause: None,
            search: None,
        },
        Clause::Return {
            distinct: false,
            group_by: None,
            items,
            order,
            skip: None,
            limit: None,
        },
    ) = (&clauses[0], &clauses[1])
    else {
        return None;
    };
    if !order.is_empty() || items.len() != 1 {
        return None;
    }
    let item = &items[0];
    let counted_var: Option<&str> = match &item.expr {
        Expr::CountStar => None,
        // The aggregate vocabulary's own classifier (ADR-0217): match
        // the kind, not a re-derived name literal.
        Expr::Function {
            name,
            distinct: false,
            args,
        } if classify_aggregate(name) == Some(AggregateKind::Count) && args.len() == 1 => {
            match &args[0] {
                Expr::Variable(v) => Some(v),
                _ => return None,
            }
        }
        _ => return None,
    };
    let [pattern] = &patterns[..] else {
        return None;
    };
    // Round 3, F-1-3: the path-level predicate fields — a path WHERE, a
    // mode (WALK/TRAIL/ACYCLIC/SIMPLE) or a selector (ANY/SHORTEST/...)
    // changes which rows the scan emits; the counts cannot express them.
    if pattern.where_clause.is_some() || pattern.mode.is_some() || pattern.selector.is_some() {
        return None;
    }
    // Round 3, F-1-4: repeated node variables (a same-variable self-loop
    // shape, `(a:A)-[:R]->(a:A)`) would answer from the (A,R,A) pair
    // count, which includes distinct-endpoint rels the scan's
    // same-variable shape excludes — decline when the variables repeat.
    let mut node_vars: HashSet<&str> = HashSet::new();
    if pattern
        .nodes
        .iter()
        .filter_map(|n| n.variable.as_deref())
        .any(|v| !node_vars.insert(v))
    {
        return None;
    }
    let plain = single_plain_label;
    let probe = if pattern.rels.is_empty() {
        // The single-node shape: the whole label population.
        let [node] = &pattern.nodes[..] else {
            return None;
        };
        // The exact per-label live count — the label index's rows are a
        // plan-time upper bound (deleted ids stay until checkpoint GC),
        // this map follows every mutation seam exactly.
        plain(node).map(CountProbe::Label)
    } else if pattern.rels.len() == 1 && pattern.nodes.len() == 2 {
        let rp = &pattern.rels[0];
        let (Some(a), Some(b)) = (plain(&pattern.nodes[0]), plain(&pattern.nodes[1])) else {
            return None;
        };
        let ([rel_type], Direction::Outgoing | Direction::Incoming) = (&rp.types[..], rp.direction)
        else {
            return None;
        };
        // Round 3, F-1-3: the rel-side predicate fields — a dynamic or
        // non-mirroring type expression and an inline WHERE change the
        // scan's row set (a statically mirroring type expression is the
        // raw front-lowered shape and is accepted — round 4, F-4-2).
        if rp.properties.is_some()
            || !label_expr_ok(&rp.type_expr, &rp.types)
            || rp.where_clause.is_some()
            || rp.var_length
            || rp.quantified
        {
            return None;
        }
        // `In` is the same single rel seen from the other end: the pair
        // the stats hold is (start, type, end) in the rel's own
        // direction, whichever way the pattern spells it.
        let (s, e) = match rp.direction {
            Direction::Outgoing => (a, b),
            _ => (b, a),
        };
        Some(CountProbe::Pair(s, rel_type, e))
    } else {
        None
    };
    let probe = probe?;
    // `count(var)` is answerable only when the pattern binds the
    // variable on every row it emits — recognition, not answer: an
    // unbound variable's count rides the scan's expression semantics.
    if let Some(v) = counted_var {
        let bound = pattern
            .nodes
            .iter()
            .filter_map(|n| n.variable.as_deref())
            .chain(pattern.rels.iter().filter_map(|r| r.variable.as_deref()))
            .any(|name| name == v);
        if !bound {
            return None;
        }
    }
    Some((probe, item))
}

/// LABS-370 (v1): recognize the count-only shape and answer it from the
/// statistics. The exactness and seed gates and the answer live here; the
/// shape decline-ladder lives in [`recognize_count_shape`] (LABS-425,
/// the round-6 decomposition of the round-1 F-1-4 finding). The boundary
/// never fails — `Option` is the whole contract: `Some` = answered,
/// `None` = decline to the clause loop's scan. The seed rows must be
/// exactly one row with no bindings (round 3, F-1-3:
/// `run_clauses_mode` runs per part with the previous stage's rows — a
/// NEXT/WHEN/ELSE body seeded with N rows must answer N × population, 0
/// when N = 0, which the single-population read cannot express).
pub(super) fn count_only_stats_result(
    graph: &ExecGraph<'_>,
    clauses: &[Clause],
    seed_rows: &[Row],
) -> Option<(Vec<Row>, QueryResult)> {
    // The exactness precondition: the base stats are only exact for a view
    // of the latest published state with no overlay — anything else
    // (in-tx scratch, pinned snapshot, in-flight apply) declines to the
    // scan, which reads its own writes (the overlay's predicate, ADR-0387
    // follow-up). A check that passes here can go stale (a writer opens
    // after it) — the windowed read below re-checks under the stats locks
    // and closes that gap; a check that fails is stable (a finished
    // writer advances `commit_ts`, so the pinned-equality never turns
    // true again for this view).
    if !graph.sees_exact_base_stats() {
        return None;
    }
    // Round 3, F-1-3: the short-circuit answers a whole-population count —
    // one unbound seed row, the statement's opening stage. Anything else
    // (zero rows, N > 1 rows, or a bound row) is a seeded stage whose
    // answer scales with the seeds — decline to the scan.
    if seed_rows.len() != 1 || !seed_rows[0].is_empty() {
        return None;
    }
    let (probe, item) = recognize_count_shape(clauses)?;
    // Round 3, F-1-5: the consistency window — the exactness check and
    // the count read under BOTH count-map locks (the pre-check above
    // only filters; a writer that opens between it and here is closed by
    // the under-lock re-check). The None≡0-by-exactness boundary lives
    // inside the stats family (a drained entry leaves the map).
    let count = graph
        .base()
        .windowed_stats_count(|| graph.sees_exact_base_stats(), probe)?;
    let column = item.output_name(0);
    // Round 3, F-1-18: the value is constructed once, shared by the row
    // and the result.
    let value = Value::Int(count as i64);
    let row = Row::from([(column.clone(), value.clone())]);
    Some((
        vec![row],
        QueryResult::intermediate(vec![column], vec![vec![value]], QueryStats::default()),
    ))
}
