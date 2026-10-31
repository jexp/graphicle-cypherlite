//! `LOAD CSV`, `CALL { }` subqueries, `CALL IN TRANSACTIONS`, conditional
//! branches, and part finishing.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::{Clause, Expr, Projection, Query};

use crate::ctx::{ExecCtx, QueryCtx};
use crate::eval::{eval, Row};
use crate::graph::QueryStats;
use crate::overlay::ExecGraph;

mod shared;

use shared::{exec_call_subquery_shared, import_invariant_body, shared_execution_plan};

use super::constraint_validation::validate_constraints;
use super::dispatch::run_nested_clauses;
use super::scope::{nested_seed_row, return_columns, star_scope_names, visible_keys};
use super::QueryResult;
use crate::csv::{field_terminator_char, record_value, split_csv_line, split_csv_records};
use crate::group_key::GroupKey;

pub(crate) fn exec_load_csv(
    ctx: ExecCtx<'_>,
    rows: Vec<Row>,
    with_headers: bool,
    url: &Expr,
    variable: &str,
    field_terminator: Option<&str>,
) -> Result<Vec<Row>> {
    let ExecCtx {
        graph,
        params,
        cypher_version,
        ..
    } = ctx;
    let delim = field_terminator_char(field_terminator)?;
    let mut out = Vec::new();
    for row in rows {
        let src = eval(graph, &row, url, params, cypher_version)?;
        let path = match src {
            Value::String(s) => s,
            Value::Null => continue,
            _ => return Err(Error::ty("LOAD CSV FROM requires a string URL")),
        };
        // The source boundary (`CsvReader::load`) resolves the CIP-202
        // canned fixture URL; everything else goes through the local-file
        // policy (ADR-0081).
        let text: Arc<str> = graph.csv.load(&path)?;
        let mut records = split_csv_records(&text, delim).into_iter();
        // The header record is extracted first, so a leading blank line
        // under WITH HEADERS cannot strip the header and flip map rows to
        // list rows.
        let headers: Option<Vec<String>> = if with_headers {
            records.next().map(|h| split_csv_line(h, delim))
        } else {
            None
        };
        // Blank-line policy for the data records: a blank record (an empty
        // physical line between record breaks) yields no row.
        let records = records.filter(|r| !r.is_empty());
        for record in records {
            // Ragged-row policy (docs/api/cli/spec.md): under WITH HEADERS a
            // record narrower than the header maps its missing names to
            // null — Neo4j's `LOAD CSV WITH HEADERS` semantics, a
            // deliberate behavior change from the earlier empty-string
            // padding, applied at the parse boundary by `record_value`'s
            // named rule (`zip_with_header_nulls`); extra cells are
            // dropped. List rows keep their natural width.
            let val = record_value(headers.as_deref(), split_csv_line(record, delim));
            let mut next = row.clone();
            next.insert(variable.to_string(), val);
            out.push(next);
        }
    }
    Ok(out)
}

/// `CALL { }` clause shape (CIP-141/CIP-32 + IN TRANSACTIONS), bundled to
/// keep the executors under the argument-count lint.
pub(crate) struct SubquerySpec<'a> {
    pub query: &'a Query,
    pub optional: bool,
    pub scope: &'a cypherlite_ir::CallScope,
    pub in_transactions: Option<&'a cypherlite_ir::InTransactions>,
}

pub(crate) fn exec_call_subquery(
    graph: &ExecGraph<'_>,
    rows: Vec<Row>,
    spec: SubquerySpec<'_>,
    params: &BTreeMap<String, Value>,
    qctx: QueryCtx<'_>,
) -> Result<Vec<Row>> {
    if let Some(tx) = spec.in_transactions {
        return exec_call_in_transactions(graph, rows, spec, tx, params, qctx);
    }
    if let Some(plan) = shared_execution_plan(spec.scope, &rows)? {
        if import_invariant_body(spec.query, &plan.check) {
            return exec_call_subquery_shared(graph, rows, spec, plan, params, qctx);
        }
    }
    let mut out = Vec::new();
    for row in rows {
        let produced = call_subquery_row(graph, &row, spec.query, spec.scope, params, qctx)?;
        if produced.is_empty() {
            out.extend(empty_body_row(row, spec.optional, spec.query, spec.scope));
        } else {
            out.extend(produced);
        }
    }
    Ok(out)
}

/// The seed row of one outer row for a `CALL (vars) { … }` body — the
/// imported bindings. An import the clause's planner scope promises must
/// survive every row-shape pass (the `UNWIND` source strip, the liveness
/// trim, the `OPTIONAL CALL` null-pad); a miss is therefore an internal
/// error, reported rather than turned into a panic.
fn import_seed(vars: &[String], outer: &Row) -> Result<Row> {
    vars.iter()
        .map(|name| {
            let value = outer.get(name).cloned().ok_or_else(|| {
                Error::internal(format!(
                    "planner-scope invariant: imported variable `{name}` must be in the outer row"
                ))
            })?;
            Ok((name.clone(), value))
        })
        .collect()
}

/// The one row-merge policy of both subquery paths: the body's export
/// overlays the outer row except for the `excluded` column names, which
/// always keep the outer row's own value. Callers pass the scope's
/// imported names (per-row path) or the shared plan's check set (the same
/// names for `CALL (vars)`; every outer key for the naked form, where a
/// colliding export is provably a same-name pass-through).
fn merge_body_export(
    outer: &Row,
    columns: &[String],
    values: &[Value],
    excluded: &HashSet<String>,
) -> Row {
    let mut merged = outer.clone();
    for (col, value) in project_row(columns, values).filter(|(col, _)| !excluded.contains(col)) {
        merged.insert(col, value);
    }
    merged
}

/// The one empty-body policy: `OPTIONAL CALL` null-pads the outer row,
/// plain `CALL` drops it.
fn empty_body_row(
    row: Row,
    optional: bool,
    query: &Query,
    scope: &cypherlite_ir::CallScope,
) -> Option<Row> {
    optional.then(|| null_pad_call_row(row, query, scope))
}

/// The names a `CALL { … }` scope imports from the outer row — the export
/// set's derivation (`call_export_columns`). A `(vars)` list names its
/// bindings; the naked scope imports nothing; the `(*)` scope imports the
/// outer row's visible names ([`visible_keys`] = the row minus the
/// enclosing clause engine's internal columns, the same set the stripped
/// seed binds). `None` (no row to derive from, e.g. a columns-only
/// question over an empty table) imports nothing — the star expansion then
/// falls back to the declared scope.
fn imported_names(scope: &cypherlite_ir::CallScope, outer: Option<&Row>) -> HashSet<String> {
    match scope {
        cypherlite_ir::CallScope::Vars(vars) => vars.iter().cloned().collect(),
        cypherlite_ir::CallScope::None => HashSet::new(),
        cypherlite_ir::CallScope::Wildcard => outer
            .map(|row| visible_keys(row).into_iter().collect())
            .unwrap_or_default(),
    }
}

pub(crate) fn call_subquery_row(
    graph: &ExecGraph<'_>,
    outer: &Row,
    query: &Query,
    scope: &cypherlite_ir::CallScope,
    params: &BTreeMap<String, Value>,
    qctx: QueryCtx<'_>,
) -> Result<Vec<Row>> {
    // One match over the CallScope vocabulary derives both the seed row and
    // the imported-name set: the seed carries the imported bindings. The
    // naked and `(*)` scopes hand the body the outer row, minus the enclosing
    // clause engine's internal columns (`nested_seed_row`) — a parked
    // `__ord*`/`__agg_*` slot is the enclosing clause's state, never a
    // binding the body may read as its own. For `(*)` the imported set is
    // derived from that same stripped seed: threading a name through the
    // body's `WITH`s that the seed no longer binds would make the body
    // evaluate an absent variable. A `(vars)` import list names its bindings
    // explicitly, so it needs no strip.
    let (seed, imported) = match scope {
        cypherlite_ir::CallScope::None => (nested_seed_row(outer), HashSet::new()),
        cypherlite_ir::CallScope::Wildcard => {
            let seed = nested_seed_row(outer);
            let imported = seed.keys().cloned().collect();
            (seed, imported)
        }
        cypherlite_ir::CallScope::Vars(vars) => {
            (import_seed(vars, outer)?, vars.iter().cloned().collect())
        }
    };
    // Called unconditionally: with the None scope's empty `imported` set
    // `query_with_persistent_imports` pushes nothing and returns the clone
    // a plain `query.clone()` would.
    let scoped_query = query_with_persistent_imports(query, &imported);
    let result = execute_query_seeded(graph, &scoped_query, params, vec![seed], qctx)?;
    // A part merges its rows back only when its last clause produces a
    // statement result: RETURN, a conditional, or a row-producing catalog
    // command (the canonical `cypherlite-front` classification — a
    // schema/admin DDL command's empty result must not suppress the
    // pass-through).
    if query.parts.iter().all(|part| match part.clauses.last() {
        Some(Clause::Return { .. } | Clause::Conditional { .. }) => false,
        Some(Clause::Catalog(command)) => {
            !cypherlite_front::show_columns::catalog_produces_rows(command)
        }
        _ => true,
    }) {
        return Ok(vec![outer.clone()]);
    }
    if result.rows.is_empty()
        && query
            .parts
            .iter()
            .all(|part| matches!(part.clauses.last(), Some(Clause::Finish)))
    {
        return Ok(vec![outer.clone()]);
    }
    let mut produced = Vec::new();
    for values in result.rows {
        // Exclusion is by column name: imported bindings pass through from
        // the outer row instead of being overwritten by the body's export.
        produced.push(merge_body_export(
            outer,
            &result.columns,
            &values,
            &imported,
        ));
    }
    Ok(produced)
}

/// Zip a `QueryResult`'s columns with one row's values into `(name, value)`
/// pairs — the one projection both row-merge paths use. The engine keeps
/// exactly one value per column; a mismatch is a bug, so the widths are
/// asserted rather than silently truncated (in release `zip` would truncate
/// without the check).
fn project_row<'a>(
    columns: &'a [String],
    values: &'a [Value],
) -> impl Iterator<Item = (String, Value)> + 'a {
    assert_eq!(
        columns.len(),
        values.len(),
        "QueryResult invariant: every row has one value per column"
    );
    columns.iter().cloned().zip(values.iter().cloned())
}

/// Push a `WITH`/`RETURN` projection for every imported binding the item
/// list does not already carry — the persistent-import policy shared by the
/// mid-part `WITH` and the concluding `RETURN` of
/// [`query_with_persistent_imports`].
fn ensure_imported(items: &mut Vec<Projection>, imported: &HashSet<String>) {
    for name in imported {
        if !items
            .iter()
            .any(|item| matches!(&item.expr, Expr::Variable(variable) if variable == name))
        {
            items.push(Projection {
                expr: Expr::Variable(name.clone()),
                alias: None,
                source_text: None,
            });
        }
    }
}

pub(crate) fn query_with_persistent_imports(query: &Query, imported: &HashSet<String>) -> Query {
    let mut query = query.clone();
    for part in &mut query.parts {
        for clause in &mut part.clauses {
            if let Clause::With { items, .. } = clause {
                ensure_imported(items, imported);
            }
        }
        if query.next.is_some() {
            if let Some(Clause::Return { items, .. }) = part.clauses.last_mut() {
                ensure_imported(items, imported);
            }
        }
    }
    if let Some(next) = &mut query.next {
        **next = query_with_persistent_imports(next, imported);
    }
    query
}

pub(crate) fn execute_query_seeded(
    graph: &ExecGraph<'_>,
    query: &Query,
    params: &BTreeMap<String, Value>,
    seeds: Vec<Row>,
    qctx: QueryCtx<'_>,
) -> Result<QueryResult> {
    let mut combined: Option<QueryResult> = None;
    // `seen` is the UNION joiner state `union_merge` owns.
    let mut seen: Option<HashSet<Vec<GroupKey>>> = None;
    let mut seeds = seeds;
    let part_count = query.parts.len();
    for (part_index, part) in query.parts.iter().enumerate() {
        // The last part takes the seeds; earlier UNION parts clone them.
        let part_seeds = if part_index + 1 == part_count {
            std::mem::take(&mut seeds)
        } else {
            seeds.clone()
        };
        // ADR-0395: one nested clause-engine part pass — every UNION part
        // of this body crosses the one nested boundary
        // (`run_nested_clauses`: the budget tick, then the clause engine
        // at the explicit `None` profile site), so a wide body costs its
        // part count on the statement's budget instead of hiding its
        // width inside one invocation tick. The top-level statement
        // driver (exec/mod.rs) never enters here.
        let (rows, last) = run_nested_clauses(graph, &part.clauses, params, part_seeds, qctx)?;
        let result = match last {
            Some(result) => result,
            None => super::rows_as_result(graph, &rows),
        };
        if let Some(prev) = &combined {
            if prev.columns != result.columns {
                return Err(Error::semantic("UNION requires same columns"));
            }
        }
        // The subquery fold's first part has no joiner of its own; every
        // later part joins through its boundary's flag.
        let all = if part_index == 0 {
            true
        } else {
            query.union_all.get(part_index - 1).copied().unwrap_or(true)
        };
        super::union_merge(&mut combined, result, all, &mut seen);
    }
    let result = combined.unwrap_or(QueryResult::intermediate(
        Vec::new(),
        Vec::new(),
        graph.stats(),
    ));
    if let Some(next) = query.next.as_deref() {
        // The one NEXT-stage seeding policy: a `Query`'s parts are the
        // same trailing-clause shape a `LogicalPlan`'s are.
        execute_query_seeded(
            graph,
            next,
            params,
            super::next_seeds(&result, query.parts.iter().map(|p| p.clauses.as_slice())),
            qctx,
        )
    } else {
        Ok(result)
    }
}

/// The names a `CALL { … }` body exports into the enclosing row's scope: the
/// shared [`return_columns`] set of its terminal `RETURN` — a `*` item
/// expanded from the body's declared scope via `star_scope_names`, its
/// sibling items appended after it — minus the `(vars)` imports. Both
/// runtime consumers take it as given: the `OPTIONAL CALL` null-pad
/// ([`null_pad_call_row`]) and the concluding CALL's result columns
/// ([`concluding_call_columns`]).
///
/// This is the runtime export set, not the front's admission set: the front's
/// `call_subquery_exports` threads the outer scope through the body's `WITH`s
/// and every introducing clause arm, so it is only as equal to this as
/// `star_scope_names`'s doc says the declared-scope walk is.
pub(crate) fn call_export_columns(
    query: &Query,
    scope: &cypherlite_ir::CallScope,
    outer: Option<&Row>,
) -> Vec<String> {
    if let Some(next) = query.next.as_deref() {
        return call_export_columns(next, scope, outer);
    }
    // The columns-only path's imported set (`imported_names` — the per-row
    // merge derives its own set from the seed it just built instead):
    // `None` imports nothing, `(*)` imports every visible outer name,
    // `(vars)` names its bindings. An exported name that is an import is a
    // pass-through, never a new column.
    let imported = imported_names(scope, outer);
    let Some(part) = query.parts.first() else {
        return Vec::new();
    };
    let columns = match part.clauses.last() {
        Some(Clause::Return { items, .. }) => {
            return_columns(items, || star_scope_names(&part.clauses))
        }
        _ => Vec::new(),
    };
    columns
        .into_iter()
        .filter(|name| !imported.contains(name.as_str()))
        .collect()
}

pub(crate) fn null_pad_call_row(
    mut row: Row,
    query: &Query,
    scope: &cypherlite_ir::CallScope,
) -> Row {
    for column in call_export_columns(query, scope, Some(&row)) {
        row.insert(column, Value::Null);
    }
    row
}

pub(crate) fn transaction_status(
    started: bool,
    committed: bool,
    id: Option<String>,
    error: Option<String>,
) -> Value {
    Value::Map(
        BTreeMap::from([
            ("started".into(), Value::Bool(started)),
            ("committed".into(), Value::Bool(committed)),
            (
                "transactionId".into(),
                id.map(|s| Value::String(s.into())).unwrap_or(Value::Null),
            ),
            (
                "errorMessage".into(),
                error
                    .map(|s| Value::String(s.into()))
                    .unwrap_or(Value::Null),
            ),
        ])
        .into(),
    )
}

pub(crate) fn exec_conditional(
    graph: &ExecGraph<'_>,
    rows: Vec<Row>,
    branches: &[(Expr, Query)],
    else_query: Option<&Query>,
    params: &BTreeMap<String, Value>,
    qctx: QueryCtx<'_>,
) -> Result<Vec<Row>> {
    let mut out = Vec::new();
    let mut remaining = rows;
    for (predicate, query) in branches {
        let mut selected = Vec::new();
        let mut unmatched = Vec::new();
        for row in remaining {
            if eval(graph, &row, predicate, params, qctx.cypher_version)?.truthy() == Some(true) {
                selected.push(row);
            } else {
                unmatched.push(row);
            }
        }
        remaining = unmatched;
        if !selected.is_empty() {
            append_query_rows(
                &mut out,
                execute_query_seeded(graph, query, params, selected, qctx)?,
            );
        }
    }
    if !remaining.is_empty() {
        if let Some(query) = else_query {
            append_query_rows(
                &mut out,
                execute_query_seeded(graph, query, params, remaining, qctx)?,
            );
        }
    }
    Ok(out)
}

pub(crate) fn append_query_rows(out: &mut Vec<Row>, result: QueryResult) {
    for values in result.rows {
        out.push(project_row(&result.columns, &values).collect());
    }
}

/// The result columns a row-less `WHEN`/`ELSE` clause declares: its first
/// branch's (or `ELSE` body's) terminal projection, read through the shared
/// [`return_columns`] set — a `RETURN *` branch expands from that body's
/// declared scope, not from nothing.
pub(crate) fn conditional_columns(
    branches: &[(Expr, Query)],
    else_query: Option<&Query>,
) -> Vec<String> {
    let query = branches.first().map(|(_, query)| query).or(else_query);
    query
        .and_then(|query| query.parts.first())
        .and_then(|part| match part.clauses.last()? {
            Clause::Return { items, .. } => {
                Some(return_columns(items, || star_scope_names(&part.clauses)))
            }
            Clause::Conditional {
                branches,
                else_query,
            } => Some(conditional_columns(branches, else_query.as_deref())),
            _ => None,
        })
        .unwrap_or_default()
}

pub(crate) fn exec_call_in_transactions(
    graph: &ExecGraph<'_>,
    rows: Vec<Row>,
    spec: SubquerySpec<'_>,
    tx: &cypherlite_ir::InTransactions,
    params: &BTreeMap<String, Value>,
    qctx: QueryCtx<'_>,
) -> Result<Vec<Row>> {
    let SubquerySpec {
        query,
        optional,
        scope,
        in_transactions: _,
    } = spec;
    // No input rows: no batch runs, and the batch-size/concurrency
    // expressions have no row to evaluate against — the call is a no-op
    // (a fabricated empty row would silently read null; ADR-0335).
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let first = &rows[0];
    let batch_size = match tx.batch_size.as_ref() {
        Some(expr) => eval(graph, first, expr, params, qctx.cypher_version)?
            .as_int()
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::ty("batch size must be a positive integer"))?
            as usize,
        None => 1000,
    };
    if let cypherlite_ir::Concurrency::Value(expr) = &tx.concurrency {
        let value = eval(graph, first, expr, params, qctx.cypher_version)?;
        if value.as_int().is_none() {
            return Err(Error::ty("concurrency must be an integer"));
        }
    }
    let mut out = Vec::new();
    let mut batches = rows.chunks(batch_size).enumerate();
    for (batch_index, batch) in batches.by_ref() {
        let snapshot = graph.scratch_snapshot();
        let tx_id = format!("cypherlite-inner-{}", batch_index + 1);
        let mut batch_out = Vec::new();
        let attempt = (|| -> Result<()> {
            for row in batch {
                let mut produced = call_subquery_row(graph, row, query, scope, params, qctx)?;
                if produced.is_empty() {
                    produced.extend(empty_body_row(row.clone(), optional, query, scope));
                }
                if let Some(name) = &tx.report_status {
                    for produced_row in &mut produced {
                        produced_row.insert(
                            name.clone(),
                            transaction_status(true, true, Some(tx_id.clone()), None),
                        );
                    }
                }
                batch_out.extend(produced);
            }
            // CIP-262: each `CALL { ... } IN TRANSACTIONS` batch is its
            // own transaction boundary, so the property default values
            // land here -- after the batch's writes, before the batch's
            // constraint validation.
            super::property_defaults::apply_property_defaults(graph)?;
            validate_constraints(graph)?;
            // ADR-0210: with a facade-injected sink (auto-commit write
            // path), each batch commits durably before the next starts
            // and the scratch rebase keeps live state bounded by one
            // batch. The commit is folded into the attempt so a failed
            // batch commit (a conflict race, a capacity rejection, a WAL
            // error) takes the same per-batch failure path as body and
            // constraint errors: `ON ERROR CONTINUE`/`BREAK`/`RETRY` apply
            // and `REPORT STATUS` rows are produced. The redo is drained
            // only on commit success, so the `Err` arm's
            // `restore_scratch` rolls a failed batch back cleanly.
            // Without a sink (explicit transactions, nested execution)
            // the scratch accumulates to statement end, as before.
            if let Some(sink) = qctx.batch_commit {
                graph.commit_batch(sink)?;
            }
            Ok(())
        })();
        match attempt {
            Ok(()) => out.extend(batch_out),
            Err(error) => {
                graph.restore_scratch(snapshot);
                let mode = match &tx.on_error {
                    cypherlite_ir::OnError::Fail => cypherlite_ir::RetryFallback::Fail,
                    cypherlite_ir::OnError::Continue => cypherlite_ir::RetryFallback::Continue,
                    cypherlite_ir::OnError::Break => cypherlite_ir::RetryFallback::Break,
                    cypherlite_ir::OnError::Retry { fallback, .. } => *fallback,
                };
                if mode == cypherlite_ir::RetryFallback::Fail {
                    return Err(error);
                }
                // Pad the failed batch with the error status, and on BREAK
                // every remaining row (no transaction starts) — one body.
                let mut pad_row = |row: &Row, status: Value| {
                    let mut row = null_pad_call_row(row.clone(), query, scope);
                    if let Some(name) = &tx.report_status {
                        row.insert(name.clone(), status);
                    }
                    out.push(row);
                };
                for row in batch {
                    pad_row(
                        row,
                        transaction_status(
                            true,
                            false,
                            Some(tx_id.clone()),
                            Some(transaction_error_message(&error)),
                        ),
                    );
                }
                if mode == cypherlite_ir::RetryFallback::Break {
                    for (_, batch) in batches.by_ref() {
                        for row in batch {
                            pad_row(row, transaction_status(false, false, None, None));
                        }
                    }
                    break;
                }
            }
        }
    }
    Ok(out)
}

/// Render a failed inner-transaction batch's error for the `REPORT STATUS`
/// row. The typed `Error` classifies it: the uniqueness violation is its
/// own variant (matched first, never sniffed from rendered text) and keeps
/// the ADR-0172-pinned `A value already exists …` wrapper; the other typed
/// detail variants hand over the legacy bare inner text — token included,
/// single-sourced in cypherlite-core — exactly as the stringly
/// `Type`/`Semantic` arms hand over their payload directly instead of
/// stripping a Display prefix.
pub(crate) fn transaction_error_message(error: &Error) -> String {
    match error {
        // First: the typed-detail hand-over below must not intercept this
        // variant — the wrapper is pinned byte-identical (ADR-0172).
        Error::UniquenessViolation { .. } => {
            format!("A value already exists and violates a uniqueness constraint: {error}")
        }
        Error::Type(message) | Error::Semantic(message) => message.clone(),
        // ADR-0307: division/modulo by zero hand over the payload their
        // legacy `Type` arm did (`/ by zero`); rounding keeps the bare
        // `ArithmeticError: …` text its legacy `Execution` Display had.
        Error::Arithmetic(failure) => failure.detail().to_string(),
        other => other
            .typed_detail_text()
            .unwrap_or_else(|| other.to_string()),
    }
}

/// Result columns for a part that concludes with a CALL instead of a
/// RETURN (ADR-0210): a concluding `CALL proc()` projects the rows'
/// visible keys (historical behavior); a concluding `CALL { … }`
/// subquery projects exactly the body's exported columns — its terminal
/// RETURN's output names. A unit/write-only body exports nothing and
/// the result is empty. Returns `None` when the part does not conclude
/// with a CALL (or a RETURN already produced the result). A `REPORT
/// STATUS AS <var>` column never appears here: the planner rejects
/// REPORT STATUS without a following RETURN, so the status column only
/// surfaces through that trailing RETURN.
pub(crate) fn concluding_call_columns(clauses: &[Clause], rows: &[Row]) -> Option<Vec<String>> {
    if clauses.iter().any(|c| matches!(c, Clause::Return { .. })) {
        return None;
    }
    if let Some(Clause::CallSubquery { query, scope, .. }) = clauses.last() {
        let columns = call_export_columns(query, scope, rows.first());
        // A unit/write-only body exports nothing: the statement result is
        // empty (no columns, no rows), matching the pre-ADR-0210 behavior
        // for concluding subqueries — pass-through outer rows must not
        // leak as column-less result rows. (A concluding `REPORT STATUS`
        // column never reaches here: the planner rejects REPORT STATUS
        // without a following RETURN.)
        return (!columns.is_empty()).then_some(columns);
    }
    if clauses.iter().any(|c| matches!(c, Clause::Call { .. })) {
        // Historical order: the row's own visible key order, unsorted.
        return Some(rows.first().map(visible_keys).unwrap_or_default());
    }
    None
}

/// Materialize `rows` onto `columns`, moving each cell and padding misses
/// with null. The source slots are left null; callers that still need the
/// values must clone before this.
pub(crate) fn materialize_rows(rows: &mut [Row], columns: &[String]) -> Vec<Vec<Value>> {
    rows.iter_mut()
        .map(|r| {
            columns
                .iter()
                .map(|c| r.take_value(c).unwrap_or(Value::Null))
                .collect()
        })
        .collect()
}

/// The one concluding-CALL finisher (ADR-0210): when no clause produced a
/// result, project the concluding CALL's columns over the surviving rows;
/// otherwise fall back to an empty result. `stats` is the caller's stats
/// source — `graph.stats()` on the write path (`execute_plan_stage_seeded`),
/// `QueryStats::default()` on the read path (`execute_part_read_seeded`),
/// whose wrapper resets stats on the way out regardless.
pub(crate) fn concluding_part_result(
    clauses: &[Clause],
    rows: &mut [Row],
    last_result: Option<QueryResult>,
    stats: QueryStats,
) -> QueryResult {
    last_result
        .or_else(|| {
            concluding_call_columns(clauses, rows).map(|columns| {
                let data = materialize_rows(rows, &columns);
                QueryResult::intermediate(columns, data, stats.clone())
            })
        })
        .unwrap_or(QueryResult::intermediate(Vec::new(), Vec::new(), stats))
}

#[cfg(test)]
mod tests;
