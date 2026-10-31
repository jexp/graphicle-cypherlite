//! The EXISTS/COUNT/COLLECT expression-subquery evaluators (the eval
//! layer's `crate::exec::eval_*_query` entry points, re-exported there):
//! the nested statement context, the LABS-193 existence probes, and the
//! three subquery engines.

use std::collections::BTreeMap;

use cypherlite_core::{Error, Result, Value};
use cypherlite_error::{ErrorKind, COUNT_UNION_MIX_MESSAGE, COUNT_UNION_OMITTED_MESSAGE};
use cypherlite_ir::{Clause, MatchMode, Query};

use super::call::{execute_query_seeded, query_with_persistent_imports};
use super::dispatch::run_nested_clauses;
use super::exists_probe;
use super::mutates_store;
use super::scope::nested_seed_row;
use super::QueryResult;
use crate::ctx::QueryCtx;
use crate::eval::Row;
use crate::overlay::ExecGraph;

/// The one fold over a nested pass's `(rows, result)` pair: the rows the
/// pass produced — the concluding result's rows when the body ended in a
/// RETURN, the driving rows otherwise. Both expression-subquery consumers
/// of [`run_nested_clauses`] (the EXISTS existence test and the
/// COUNT-omitted-RETURN count) ask exactly this, so the fold is spelled
/// once here instead of per site.
fn nested_row_count(rows: &[Row], last: &Option<QueryResult>) -> usize {
    last.as_ref()
        .map(|result| result.rows.len())
        .unwrap_or(rows.len())
}

/// The one nested-body preparation the three expression-subquery engines
/// share: strip the enclosing clause engine's internal columns off the
/// seed row, import the remaining names, rewrite the body with persistent
/// imports, and build the nested statement context — a fresh context
/// per ADR-0344 phase 2 (no statement text: a `SHOW TRANSACTIONS` buried
/// in an expression subquery reports an empty `currentQuery`; the
/// version inherited from the enclosing statement per CIP-150 unless
/// the body carries its own preamble; the PROFILE facts never cross the
/// boundary, the clause engine running at the explicit `None` site).
/// The budget never rode the context (ADR-0395): every nested entry
/// ticks the view-owned counter at the engine entries, so a fresh
/// context cannot reset it.
///
/// The seed row is returned because the body must run on the stripped
/// row, not the caller's: a `__`-prefixed column left on it would be
/// imported as a persistent binding and read by the body's projection as
/// its own state (`nested_seed_row`).
fn nested_body(
    query: &Query,
    outer: &Row,
    cypher_version: Option<u32>,
) -> (Query, QueryCtx<'static>, Row) {
    let seed = nested_seed_row(outer);
    let imported = seed.keys().cloned().collect();
    let query = query_with_persistent_imports(query, &imported);
    let qctx = QueryCtx::new(query.cypher_version.or(cypher_version), "");
    (query, qctx, seed)
}

/// Existence fast path for [`eval_exists_query`]. A single plain MATCH of
/// one pattern stays on the LABS-193 endpoint probe. Several plain MATCH
/// patterns with no WHERE use the depth-first probe and stop at the first
/// hit. Anything else — WHERE, OPTIONAL, search, UNION, NEXT — returns
/// `Ok(None)` and runs the engine.
fn exists_pattern_probe(
    graph: &ExecGraph<'_>,
    row: &Row,
    query: &Query,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Option<bool>> {
    if query.parts.len() != 1 || !query.union_all.is_empty() || query.next.is_some() {
        return Ok(None);
    }
    let part = &query.parts[0];
    // GPM eligibility is decided where it is owned: needs_gpm in
    // resolved_endpoints_exist (the path-level gates), the walk mode here
    // (the only eligibility input this layer adds).
    if let [Clause::Match {
        optional,
        match_mode,
        patterns,
        where_clause,
        search,
    }] = part.clauses.as_slice()
    {
        if !*optional
            && where_clause.is_none()
            && search.is_none()
            && patterns.len() == 1
            && *match_mode != MatchMode::RepeatableElements
        {
            return crate::eval::resolved_endpoints_exist(
                graph,
                row,
                &patterns[0],
                params,
                cypher_version,
            );
        }
    }
    exists_probe::probe_multi_pattern_exists(graph, row, &part.clauses, params, cypher_version)
}

pub(crate) fn eval_exists_query(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    query: &Query,
    cypher_version: Option<u32>,
) -> Result<Value> {
    let (query, qctx, seed) = nested_body(query, row, cypher_version);
    // LABS-193: a pattern-only body between row-bound nodes answers
    // existence without running the subquery engine — and never ticks
    // the budget (ADR-0395).
    // The body gate asks the mutation question the composition rule is
    // about — a catalog DDL command and a writes-flagged procedure count
    // here even though the read dispatcher admits their clause shapes.
    // One static admission check for the whole body, before the probe and
    // the per-part loop: the rule is a composition fact of the statement,
    // not a per-part decision the loop's short-circuits can skip.
    if query
        .parts
        .iter()
        .any(|part| part.clauses.iter().any(mutates_store))
    {
        return Err(Error::syntax(
            "InvalidClauseComposition: writes are not allowed in EXISTS { }",
        ));
    }
    if let Some(hit) = exists_pattern_probe(graph, &seed, &query, params, cypher_version)? {
        return Ok(Value::Bool(hit));
    }
    for part in &query.parts {
        // The subquery's ctx (subquery_qctx) is the canonical nested
        // context: nested clauses are not the statement's operators.
        // ADR-0395: the clause-engine pass — one per part per outer row.
        let (rows, last) =
            run_nested_clauses(graph, &part.clauses, params, vec![seed.clone()], qctx)?;
        if nested_row_count(&rows, &last) > 0 {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

pub(crate) fn eval_count_query(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    query: &Query,
    cypher_version: Option<u32>,
) -> Result<Value> {
    let (query, qctx, seed) = nested_body(query, row, cypher_version);
    // The parts' explicit-vs-omitted RETURN classification as one count:
    // some-but-not-all is the mix error, zero is the omitted-RETURN path,
    // and every other count takes the seeded statement engine below. Both
    // compositions are rejected before this per-row scan ever runs for
    // parsed text — the parser's COUNT-site check
    // (`check_subquery_union_consistency`) first, then the front's
    // validate battery (`validate_count_subquery_union_shapes`, the
    // statically decidable home of the rules, raising these identical
    // messages); these scans are defense-in-depth for callers that bypass
    // validation (a hand-built query, a future front gap).
    let explicit = query
        .parts
        .iter()
        .filter(|part| matches!(part.clauses.last(), Some(Clause::Return { .. })))
        .count();
    if explicit > 0 && explicit < query.parts.len() {
        return Err(Error::syntax(format!(
            "{}: {COUNT_UNION_MIX_MESSAGE}",
            ErrorKind::DifferentColumnsInUnion.as_str()
        )));
    }
    if explicit == 0 {
        if query.union_all.iter().any(|all| !*all) {
            return Err(Error::syntax(format!(
                "{}: {COUNT_UNION_OMITTED_MESSAGE}",
                ErrorKind::DifferentColumnsInUnion.as_str()
            )));
        }
        let mut count = 0i64;
        for part in &query.parts {
            // ADR-0395: the omitted-RETURN path runs the clause engine
            // directly — one pass per part per outer row (the RETURN path
            // below ticks inside `execute_query_seeded`).
            let (rows, last) =
                run_nested_clauses(graph, &part.clauses, params, vec![seed.clone()], qctx)?;
            count += nested_row_count(&rows, &last) as i64;
        }
        return Ok(Value::Int(count));
    }
    let result = execute_query_seeded(graph, &query, params, vec![seed], qctx)?;
    Ok(Value::Int(result.rows.len() as i64))
}

pub(crate) fn eval_collect_query(
    graph: &ExecGraph<'_>,
    row: &Row,
    params: &BTreeMap<String, Value>,
    query: &Query,
    cypher_version: Option<u32>,
) -> Result<Value> {
    let (query, qctx, seed) = nested_body(query, row, cypher_version);
    let result = execute_query_seeded(graph, &query, params, vec![seed], qctx)?;
    if result.columns.len() != 1 {
        return Err(Error::syntax(
            "CollectExpressionMustReturnSingleColumn: COLLECT { } requires one column",
        ));
    }
    Ok(Value::List(
        result
            .rows
            .into_iter()
            // The single-column guard above makes every row exactly one
            // value (the result columns' invariant) — a miss is internal
            // corruption, not a null to smuggle into the list.
            .map(|row| {
                row.into_iter()
                    .next()
                    .expect("QueryResult rows carry one value per column")
            })
            .collect::<Vec<_>>()
            .into(),
    ))
}
