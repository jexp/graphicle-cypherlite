//! Depth-first existence over planner-ordered MATCH patterns.
//!
//! [`match_paths`](super::match_clause::match_paths) finishes one pattern's
//! `Vec` before starting the next, so a row cap on the last pattern cannot
//! stop the first. This probe offers each complete binding of pattern N to
//! pattern N+1 immediately and returns at the first binding that satisfies
//! the predicate.

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{RelId, Result, Value};
use cypherlite_ir::{BinaryOp, Clause, Expr, MatchMode, PathPattern, Projection};

use crate::ctx::ExecCtx;
use crate::eval::{eval, Row};
use crate::overlay::ExecGraph;

use super::expand::{match_resolved_path, ProbeSink};
use super::pushdown::{flatten_and_owned, match_scope, FoldedReturn, Residual};

/// `Ok(None)` when the patterns are not a classic walk (GPM, shortest path).
/// The caller keeps the full-match path.
pub(crate) fn patterns_exist(
    ctx: ExecCtx<'_>,
    patterns: &[PathPattern],
    wheres: &[Expr],
    row: Row,
    pred: &dyn Fn(&Row) -> Result<bool>,
) -> Result<Option<bool>> {
    if patterns.is_empty()
        || patterns
            .iter()
            .any(|path| path.needs_gpm() || path.is_shortest_path_function)
    {
        return Ok(None);
    }
    let conjuncts: Vec<Expr> = wheres.iter().flat_map(flatten_and_owned).collect();
    let scope = match_scope(&row, patterns);
    let residual = Residual {
        conjuncts: &conjuncts,
        scope: &scope,
    };
    Ok(Some(seek(
        ctx,
        patterns,
        0,
        row,
        HashSet::new(),
        &residual,
        pred,
    )?))
}

fn seek(
    ctx: ExecCtx<'_>,
    patterns: &[PathPattern],
    idx: usize,
    row: Row,
    used: HashSet<RelId>,
    residual: &Residual<'_>,
    pred: &dyn Fn(&Row) -> Result<bool>,
) -> Result<bool> {
    if idx >= patterns.len() {
        return pred(&row);
    }
    let halt = Cell::new(false);
    let graph = ctx.graph;
    let params = ctx.params;
    let cypher_version = ctx.cypher_version;
    let accept = |row2: Row, used2: HashSet<RelId>| -> Result<bool> {
        seek(
            ExecCtx::new(graph, params, cypher_version),
            patterns,
            idx + 1,
            row2,
            used2,
            residual,
            pred,
        )
    };
    let mut sink = ProbeSink {
        accept: &accept,
        halt: &halt,
    };
    // LABS-161: the first-hop rel seek narrows the probe's walk the same
    // way it narrows match_one_path's; a Gone seek probes false.
    match_resolved_path(
        graph,
        &patterns[idx],
        params,
        cypher_version,
        residual,
        row,
        used,
        &mut sink,
    )?;
    Ok(halt.get())
}

struct PlainMatch<'a> {
    patterns: &'a [PathPattern],
    where_clause: Option<&'a Expr>,
}

/// A non-optional, non-search `MATCH` in `DIFFERENT RELATIONSHIPS` mode.
fn plain_match(clause: &Clause) -> Option<PlainMatch<'_>> {
    let Clause::Match {
        optional,
        match_mode,
        patterns,
        where_clause,
        search,
    } = clause
    else {
        return None;
    };
    if *optional || search.is_some() || *match_mode != MatchMode::DifferentRelationships {
        return None;
    }
    Some(PlainMatch {
        patterns,
        where_clause: where_clause.as_ref(),
    })
}

struct BoolCount<'a> {
    patterns: Vec<PathPattern>,
    wheres: Vec<Expr>,
    /// MATCH clauses after the one currently being dispatched.
    extra_matches: usize,
    output: String,
    counted: &'a Expr,
}

/// One or more plain `MATCH` clauses followed by an ungrouped
/// `RETURN count(DISTINCT expr) > 0` or `>= 1`.
fn boolean_count_span(clauses: &[Clause]) -> Option<BoolCount<'_>> {
    let mut patterns = Vec::new();
    let mut wheres = Vec::new();
    let mut matches = 0usize;
    for clause in clauses {
        match clause {
            Clause::Match { .. } => {
                let plain = plain_match(clause)?;
                if plain.patterns.is_empty() {
                    return None;
                }
                patterns.extend(plain.patterns.iter().cloned());
                if let Some(where_clause) = plain.where_clause {
                    wheres.push(where_clause.clone());
                }
                matches += 1;
            }
            Clause::Return {
                distinct,
                group_by,
                items,
                order,
                skip,
                limit,
            } => {
                if matches == 0
                    || *distinct
                    || group_by.is_some()
                    || !order.is_empty()
                    || skip.is_some()
                    || limit.is_some()
                    || items.len() != 1
                {
                    return None;
                }
                let Projection { expr, .. } = &items[0];
                let counted = counted_existence(expr)?;
                return Some(BoolCount {
                    patterns,
                    wheres,
                    extra_matches: matches - 1,
                    output: items[0].output_name(0),
                    counted,
                });
            }
            _ => return None,
        }
    }
    None
}

/// `count(DISTINCT expr) > 0` or `count(DISTINCT expr) >= 1`.
fn counted_existence(expr: &Expr) -> Option<&Expr> {
    let Expr::Binary { op, left, right } = expr else {
        return None;
    };
    let threshold_ok = match op {
        BinaryOp::Gt => matches!(right.as_ref(), Expr::Int(0)),
        BinaryOp::Gte => matches!(right.as_ref(), Expr::Int(1)),
        _ => false,
    };
    if !threshold_ok {
        return None;
    }
    let Expr::Function {
        name,
        distinct,
        args,
    } = left.as_ref()
    else {
        return None;
    };
    if !*distinct || args.len() != 1 || !name.eq_ignore_ascii_case("count") {
        return None;
    }
    Some(&args[0])
}

/// `Ok(None)` keeps the full-match path. The returned fold already includes
/// the consumed `RETURN`.
pub(crate) fn try_fold_boolean_count(
    graph: &ExecGraph<'_>,
    rows: &[Row],
    clauses: &[Clause],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Option<FoldedReturn>> {
    let Some(span) = boolean_count_span(clauses) else {
        return Ok(None);
    };
    // WHERE conjuncts are the residual on the walk. This predicate only
    // decides whether the counted expression is non-null.
    let pred = |row: &Row| -> Result<bool> {
        let value = eval(graph, row, span.counted, params, cypher_version)?;
        Ok(!value.is_null())
    };
    let mut hit = false;
    for row in rows {
        graph.cancel_tick()?;
        match patterns_exist(
            ExecCtx::new(graph, params, cypher_version),
            &span.patterns,
            &span.wheres,
            row.clone(),
            &pred,
        )? {
            None => return Ok(None),
            Some(true) => {
                hit = true;
                break;
            }
            Some(false) => {}
        }
    }
    let mut out = Row::new();
    out.insert(span.output.clone(), Value::Bool(hit));
    Ok(Some(FoldedReturn {
        result: super::QueryResult::intermediate(
            vec![span.output],
            vec![vec![Value::Bool(hit)]],
            graph.stats(),
        ),
        rows: vec![out],
        // Remaining MATCH clauses plus the RETURN this fold consumed.
        consumed_after: span.extra_matches + 1,
    }))
}

/// Multi-pattern `EXISTS { MATCH ... }` with no `WHERE`. A single pattern
/// stays on the LABS-193 probe. `Ok(None)` keeps the subquery engine.
pub(crate) fn probe_multi_pattern_exists(
    graph: &ExecGraph<'_>,
    row: &Row,
    clauses: &[Clause],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Option<bool>> {
    if clauses.is_empty() {
        return Ok(None);
    }
    let mut patterns = Vec::new();
    for clause in clauses {
        let Some(plain) = plain_match(clause) else {
            return Ok(None);
        };
        if plain.where_clause.is_some() {
            return Ok(None);
        }
        patterns.extend(plain.patterns.iter().cloned());
    }
    if patterns.len() < 2 {
        return Ok(None);
    }
    patterns_exist(
        ExecCtx::new(graph, params, cypher_version),
        &patterns,
        &[],
        row.clone(),
        &|_| Ok(true),
    )
}
