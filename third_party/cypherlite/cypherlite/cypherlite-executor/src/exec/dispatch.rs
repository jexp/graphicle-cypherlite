//! Clause dispatchers: `run_clauses` (write path) and
//! `execute_part_read_seeded` (read path).

use std::collections::BTreeMap;
// `web_time` on wasm32: `std::time::Instant::now()` traps on
// `wasm32-unknown-unknown`, and PROFILE is a user-reachable path in the
// browser build — the clock must read, not panic (the facade uses the
// same shim).
use web_time::Instant;

use cypherlite_core::{Error, Result, Value};
use cypherlite_ir::{Clause, MatchMode};

use crate::ctx::{ClauseMetricsRecord, ExecCtx, PlanMetricsSite, QueryCtx};
use crate::eval::{eval, into_items, rows_logical_bytes, Row};
use crate::graph::QueryStats;
use crate::overlay::ExecGraph;

use super::call::{
    concluding_part_result, conditional_columns, exec_call_subquery, exec_conditional,
    exec_load_csv, SubquerySpec,
};
use super::catalog::{catalog_result, execute_catalog};
use super::catalog_show::show_indexes_result;
use super::count_stats::count_only_stats_result;
use super::create::{
    create_patterns, patterns_have_graph_sensitive_expr, reject_null_merge_props,
    resolve_create_patterns,
};
use super::ddl::{
    exec_create_constraint, exec_create_index, exec_drop_constraint, exec_drop_index,
    ConstraintSpec,
};
use super::exists_probe::try_fold_boolean_count;
use super::foreach::exec_foreach;
use super::match_clause::{match_clause, match_paths};
use super::procedure_rows::builtin_procedure;
use super::procedures::{exec_call, CallSpec};
use super::project::{
    project_rows, projection_result_columns, sinkable_projection, ProjectSpec, RowSource,
};
use super::pushdown::{
    live_from, match_limit_cap, trim_rows, try_fold_one_hop_aggs, try_fold_path_aggs, FoldedReturn,
    Residual,
};
use super::scope::{refresh_rows, star_scope_names};
use super::search::search_match_clause;
use super::unwind::Source;
use super::write::{apply_resolved_removes, apply_sets, delete_value, resolve_removes};
use super::{rows_as_result, QueryResult};

/// Read/write mode for the unified clause dispatcher. Both clause loops —
/// the write-capable [`run_clauses`] and the read-only
/// [`execute_part_read_seeded`] — are the same loop over the same `Clause`
/// arms; the mode only decides whether write clauses run or are rejected and
/// which stats a `RETURN` reports. Clause-independent concerns (the loop-top
/// cancellation check, per-clause dispatch) live in exactly one place:
/// [`run_clauses_mode`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum ClauseMode {
    Read,
    Write,
}

impl ClauseMode {
    fn is_read(self) -> bool {
        matches!(self, ClauseMode::Read)
    }
}

pub(crate) fn run_clauses(
    graph: &ExecGraph<'_>,
    clauses: &[Clause],
    params: &BTreeMap<String, Value>,
    rows: Vec<Row>,
    qctx: QueryCtx<'_>,
    profile: Option<PlanMetricsSite<'_>>,
) -> Result<(Vec<Row>, Option<QueryResult>)> {
    run_clauses_mode(
        graph,
        ClauseMode::Write,
        clauses,
        params,
        rows,
        qctx,
        profile,
    )
}

/// One nested clause-engine pass (ADR-0395): tick the statement's budget
/// counter, then run the clause engine. The direct nested entries — the
/// EXISTS/COUNT clause-engine paths (`subquery_eval`) and the FOREACH arm
/// — all enter through here; the seeded statement engine
/// (`execute_query_seeded`) ticks its own entry. The nested boundary is
/// the explicit `None` profile site — no PROFILE facts cross it.
pub(crate) fn run_nested_clauses(
    graph: &ExecGraph<'_>,
    clauses: &[Clause],
    params: &BTreeMap<String, Value>,
    rows: Vec<Row>,
    qctx: QueryCtx<'_>,
) -> Result<(Vec<Row>, Option<QueryResult>)> {
    graph.nested_evals().tick()?;
    run_clauses(graph, clauses, params, rows, qctx, None)
}

fn run_clauses_mode(
    graph: &ExecGraph<'_>,
    mode: ClauseMode,
    clauses: &[Clause],
    params: &BTreeMap<String, Value>,
    mut rows: Vec<Row>,
    qctx: QueryCtx<'_>,
    profile: Option<PlanMetricsSite<'_>>,
) -> Result<(Vec<Row>, Option<QueryResult>)> {
    let mut last_result: Option<QueryResult> = None;
    // LABS-370 (v1): the count-only short-circuit — a whole-population
    // count answers from the maintained statistics (the label counts, the
    // rel-type counts, the (label, type, end-label) pair counts) instead
    // of scanning its population. Only without the PROFILE site
    // attached: a PROFILE run must observe the plan it describes, and
    // the count operator's self-describing plan node + its 0-db-hit
    // facts are the follow-up phase.
    if profile.is_none() {
        // The incoming seed rows ride along (round 3, F-1-3): the
        // short-circuit answers a whole-population count — one unbound
        // seed row. A stage seeded with the previous stage's rows (a
        // NEXT/WHEN/ELSE body) must answer N × population, so the
        // recognizer declines on anything but that one row.
        if let Some((rows, result)) = count_only_stats_result(graph, clauses, &rows) {
            return Ok((rows, Some(result)));
        }
    }
    let mut clause_i = 0;
    // Set to `SinkProjected` when the clause just run left the next
    // projection's columns already evaluated in its rows (the match sink,
    // which only ever feeds the clause immediately after it) and read by that
    // projection one iteration later. The rows carry no marker of their own,
    // so a user alias can never forge this state; the dispatcher is its one
    // authority.
    let mut rows_source = RowSource::Bindings;
    while clause_i < clauses.len() {
        let i = clause_i;
        let clause = &clauses[i];
        graph.check_cancelled()?;
        let rows_from = std::mem::replace(&mut rows_source, RowSource::Bindings);
        // The bundled PROFILE fact (ADR-0344 phase 2 + metric fields):
        // the site's presence decides the per-clause tick — the sink it
        // carries, its position, the arm's start clock and store-access
        // counter snapshot — through one branch, never an `Instant` or a
        // counter read on the off-path. The site bundles the sink with
        // its position, so both half-states (a tick with no sink, a sink
        // with no position) are unrepresentable.
        let metrics_tick = profile.map(profile_tick);
        // The two MATCH fast paths (the search MATCH and the folded
        // MATCH) consume more than this clause; `skip` carries how many
        // extra clauses they swallowed past `i` (0 for the ordinary
        // one-clause advance). Recording and advancing happen once, at
        // the shared tail.
        let mut skip = 0;
        // `RETURN` moves its cells into the query result before this tail
        // runs. Record that clause while the values are still in the rows.
        let mut profile_recorded = false;
        // The read path's one admission rule, asked once per clause before
        // any clause arm runs: the policy names the message, the dispatcher
        // only reports it. The clause match below therefore holds clause arms
        // and nothing else.
        if mode.is_read() {
            if let Some(reason) = super::read_path_rejection(clause) {
                return Err(Error::semantic(reason));
            }
        }
        match clause {
            Clause::Catalog(command) => {
                rows = execute_catalog(graph, command, rows, params, qctx)?;
                // Only row-producing catalog commands (the SHOW family and
                // TERMINATE TRANSACTIONS) project their rows — `catalog_result`
                // returns `None` for schema/admin DDL, leaving `last_result`
                // unset so the part finishes with zero rows and zero columns
                // (Neo4j parity, CIP-161).
                if i + 1 == clauses.len() {
                    last_result = catalog_result(&rows, command);
                }
            }
            Clause::Create { patterns } => {
                // MATCH (and any earlier clause) drops its row charge on
                // return, but this clause keeps that vector until it
                // finishes. Bill it for the whole write, and again as
                // created entities land in the rows.
                let mut driving = cypherlite_core::tokens::WorkspaceCharge::retain_with(|| {
                    crate::workspace::row_buffer_bytes(&rows)
                })?;
                if patterns_have_graph_sensitive_expr(patterns) {
                    let prepared = rows
                        .iter()
                        .map(|row| {
                            resolve_create_patterns(
                                graph,
                                row,
                                patterns,
                                params,
                                qctx.cypher_version,
                            )
                        })
                        .collect::<Result<Vec<_>>>()?;
                    for n in 0..rows.len() {
                        create_patterns(
                            graph,
                            &mut rows[n],
                            &prepared[n],
                            params,
                            qctx.cypher_version,
                        )?;
                        if n % 64 == 63 {
                            driving.resize_with(|| crate::workspace::row_buffer_bytes(&rows))?;
                        }
                    }
                } else {
                    for n in 0..rows.len() {
                        create_patterns(
                            graph,
                            &mut rows[n],
                            patterns,
                            params,
                            qctx.cypher_version,
                        )?;
                        if n % 64 == 63 {
                            driving.resize_with(|| crate::workspace::row_buffer_bytes(&rows))?;
                        }
                    }
                }
                driving.resize_with(|| crate::workspace::row_buffer_bytes(&rows))?;
                refresh_rows(graph, &mut rows);
                driving.resize_with(|| crate::workspace::row_buffer_bytes(&rows))?;
            }
            Clause::CreateIndex {
                name,
                if_not_exists,
                label,
                property,
            } => {
                exec_create_index(graph, name.as_deref(), *if_not_exists, label, property)?;
            }
            Clause::DropIndex { if_exists, target } => {
                exec_drop_index(graph, *if_exists, target)?;
            }
            Clause::ShowIndexes => {
                last_result = Some(show_indexes_result(graph));
            }
            Clause::Finish => {
                last_result = Some(QueryResult::intermediate(
                    Vec::new(),
                    Vec::new(),
                    QueryStats::default(),
                ));
            }
            Clause::LoadCsv {
                with_headers,
                url,
                variable,
                field_terminator,
            } => {
                rows = exec_load_csv(
                    ExecCtx::new(graph, params, qctx.cypher_version),
                    rows,
                    *with_headers,
                    url,
                    variable,
                    field_terminator.as_deref(),
                )?;
            }
            Clause::CallSubquery {
                query,
                optional,
                scope,
                in_transactions,
            } => {
                rows = exec_call_subquery(
                    graph,
                    rows,
                    SubquerySpec {
                        query,
                        optional: *optional,
                        scope,
                        in_transactions: in_transactions.as_ref(),
                    },
                    params,
                    qctx,
                )?;
            }
            Clause::Conditional {
                branches,
                else_query,
            } => {
                rows =
                    exec_conditional(graph, rows, branches, else_query.as_deref(), params, qctx)?;
                let mut result = rows_as_result(graph, &rows);
                if result.columns.is_empty() {
                    result.columns = conditional_columns(branches, else_query.as_deref());
                }
                last_result = Some(result);
            }
            Clause::Foreach {
                variable,
                list,
                clauses,
            } => {
                exec_foreach(graph, &rows, variable, list, clauses, params, qctx)?;
            }
            Clause::Use { .. } => {}
            Clause::CreateConstraint {
                name,
                if_not_exists,
                relationship,
                label,
                properties,
                kind,
                property_type,
            } => {
                exec_create_constraint(
                    graph,
                    &ConstraintSpec {
                        name: name.as_deref(),
                        if_not_exists: *if_not_exists,
                        relationship: *relationship,
                        label,
                        properties,
                        kind,
                        property_type: property_type.as_ref(),
                    },
                )?;
            }
            Clause::DropConstraint { name, if_exists } => {
                exec_drop_constraint(graph, name, *if_exists)?;
            }
            Clause::Match {
                optional,
                match_mode,
                patterns,
                where_clause,
                search,
            } => {
                if let Some(search) = search {
                    rows = search_match_clause(
                        ExecCtx::new(graph, params, qctx.cypher_version),
                        rows,
                        *optional,
                        *match_mode,
                        patterns,
                        where_clause.as_ref(),
                        search,
                    )?;
                    if let Some(live) = live_from(&clauses[i + 1..]) {
                        trim_rows(&mut rows, &live);
                    }
                } else {
                    let folded = if !*optional {
                        try_match_folds(
                            graph,
                            &rows,
                            patterns,
                            where_clause.as_ref(),
                            &clauses[i..],
                            params,
                            qctx.cypher_version,
                        )?
                    } else {
                        None
                    };
                    match folded {
                        Some(folded) => {
                            rows = folded.rows;
                            last_result = Some(folded.result);
                            skip = folded.consumed_after;
                        }
                        None => {
                            let mut cap = if *optional || where_clause.is_some() {
                                None
                            } else {
                                match_limit_cap(
                                    &clauses[i + 1..],
                                    patterns,
                                    graph,
                                    params,
                                    qctx.cypher_version,
                                    rows.len(),
                                )?
                            };
                            let projection = clauses.get(i + 1).and_then(sinkable_projection);
                            rows = match_clause(
                                ExecCtx::new(graph, params, qctx.cypher_version),
                                rows,
                                *optional,
                                *match_mode,
                                patterns,
                                where_clause.as_ref(),
                                cap.as_mut(),
                                projection.as_ref(),
                            )?;
                            if projection.is_some() {
                                rows_source = RowSource::SinkProjected;
                            } else if let Some(live) = live_from(&clauses[i + 1..]) {
                                // A sink projection already dropped the
                                // bindings the next clause does not name.
                                // Trimming here would keep the pre-projection
                                // variables and discard the output columns.
                                trim_rows(&mut rows, &live);
                            }
                        }
                    }
                }
            }
            Clause::Merge {
                pattern,
                on_create,
                on_match,
            } => {
                let mut next = Vec::new();
                for row in rows {
                    reject_null_merge_props(graph, &row, pattern, params, qctx.cypher_version)?;
                    let none = Residual::empty();
                    let matched = match_paths(
                        ExecCtx::new(graph, params, qctx.cypher_version),
                        std::slice::from_ref(pattern),
                        MatchMode::DifferentRelationships,
                        row.clone(),
                        &none,
                        None,
                        None,
                    )?;
                    if matched.is_empty() {
                        let mut created = row;
                        create_patterns(
                            graph,
                            &mut created,
                            std::slice::from_ref(pattern),
                            params,
                            qctx.cypher_version,
                        )?;
                        apply_sets(graph, &mut created, on_create, params, qctx.cypher_version)?;
                        next.push(created);
                    } else {
                        for mut m in matched {
                            apply_sets(graph, &mut m, on_match, params, qctx.cypher_version)?;
                            next.push(m);
                        }
                    }
                }
                rows = next;
                refresh_rows(graph, &mut rows);
            }
            Clause::Delete {
                detach,
                expressions,
            } => {
                let resolved = rows
                    .iter()
                    .map(|row| {
                        expressions
                            .iter()
                            .map(|expr| eval(graph, row, expr, params, qctx.cypher_version))
                            .collect::<Result<Vec<_>>>()
                    })
                    .collect::<Result<Vec<_>>>()?;
                for values in resolved {
                    for value in values {
                        delete_value(graph, &value, *detach)?;
                    }
                }
                refresh_rows(graph, &mut rows);
            }
            Clause::Set { items } => {
                for row in rows.iter_mut() {
                    apply_sets(graph, row, items, params, qctx.cypher_version)?;
                }
                refresh_rows(graph, &mut rows);
            }
            Clause::Remove { items } => {
                let resolved = rows
                    .iter()
                    .map(|row| resolve_removes(graph, row, items, params, qctx.cypher_version))
                    .collect::<Result<Vec<_>>>()?;
                for items in resolved {
                    apply_resolved_removes(graph, items)?;
                }
                refresh_rows(graph, &mut rows);
            }
            Clause::Call {
                name,
                args,
                explicit,
                yield_items,
                optional,
                where_clause,
            } => {
                let was_empty = rows.is_empty();
                rows = exec_call(
                    ExecCtx::new(graph, params, qctx.cypher_version).with_principal(qctx.principal),
                    rows,
                    CallSpec {
                        name,
                        args,
                        explicit: *explicit,
                        yield_items,
                        optional: *optional,
                        where_clause: where_clause.as_ref(),
                    },
                )?;
                // A void optional call over a zero-row driving table yields
                // one unit row (CIP-159 [12]); Neo4j reads every variable
                // in scope as null on it, so bind the scope names
                // explicitly (ADR-0335) instead of leaving a following
                // projection to read an unbound reference.
                if was_empty && rows.len() == 1 && rows[0].is_empty() {
                    for name in star_scope_names(&clauses[..i]) {
                        rows[0].insert(name, Value::Null);
                    }
                }
                if i + 1 == clauses.len()
                    && graph
                        .procedure(name)
                        .or_else(|| builtin_procedure(graph, name, qctx.principal))
                        .is_some_and(|procedure| procedure.yield_names.is_empty())
                {
                    last_result = Some(QueryResult::intermediate(
                        Vec::new(),
                        Vec::new(),
                        QueryStats::default(),
                    ));
                }
            }
            Clause::Unwind { expression, alias } => {
                let mut next = Vec::new();
                // The suffix does not depend on the row, so the strip decision
                // is planned once and applied to each template.
                let strip = Source::plan(expression, &clauses[i + 1..]);
                for row in rows {
                    let v = eval(graph, &row, expression, params, qctx.cypher_version)?;
                    match v {
                        Value::List(xs) => {
                            let mut template = row;
                            if let Some(source) = &strip {
                                source.strip(&mut template);
                            }
                            for x in into_items(xs) {
                                graph.cancel_tick()?;
                                let mut r = template.clone();
                                r.insert(alias.clone(), x);
                                next.push(r);
                            }
                        }
                        Value::Null => {}
                        other => {
                            let mut r = row;
                            r.insert(alias.clone(), other);
                            next.push(r);
                        }
                    }
                }
                rows = next;
            }
            Clause::With {
                distinct,
                group_by,
                items,
                order,
                skip,
                limit,
                where_clause,
                ..
            } => {
                rows = project_rows(
                    ExecCtx::new(graph, params, qctx.cypher_version),
                    rows,
                    &ProjectSpec {
                        items,
                        distinct: *distinct,
                        group_by: group_by.as_deref(),
                        order,
                        skip,
                        limit,
                        where_clause: where_clause.as_ref(),
                    },
                    false,
                    rows_from,
                )?;
            }
            Clause::Return {
                distinct,
                group_by,
                items,
                order,
                skip,
                limit,
            } => {
                rows = project_rows(
                    ExecCtx::new(graph, params, qctx.cypher_version),
                    rows,
                    &ProjectSpec {
                        items,
                        distinct: *distinct,
                        group_by: group_by.as_deref(),
                        order,
                        skip,
                        limit,
                        where_clause: None,
                    },
                    true,
                    rows_from,
                )?;
                if let Some(tick) = &metrics_tick {
                    tick.record(i, &rows);
                    profile_recorded = true;
                }
                let columns = projection_result_columns(items, &rows, clauses);
                // Projection rows are rectangular by contract (ADR-0339):
                // every result column is bound, so a miss is an engine
                // contract break. Move the cells out — the PROFILE tick
                // above already read them — instead of cloning.
                let data: Vec<Vec<Value>> = rows
                    .iter_mut()
                    .map(|r| {
                        columns
                            .iter()
                            .map(|c| {
                                r.take_value(c)
                                    .expect("projection rows carry every result column")
                            })
                            .collect()
                    })
                    .collect();
                // The view's own stats: a read view carries no scratch, so
                // its stats are already the default, and the read path's
                // wrapper (`execute_read_at`) resets them on the way out
                // regardless — one policy, not a per-arm mode branch.
                last_result = Some(QueryResult::intermediate(columns, data, graph.stats()));
            }
        }
        // ADR-0344 phase 2 + metric fields: report one clause operator's
        // metrics — rows produced, store accesses, row-materialization
        // bytes, elapsed — to the PROFILE sink its tick carries, at the
        // shared clause-exit tail of the dispatch loop. The tick is
        // `None` exactly when no sink is attached (the off-path branch),
        // so the elapsed read, the counter read, and the row-size walk
        // happen only on the PROFILE path.
        if let Some(tick) = metrics_tick.filter(|_| !profile_recorded) {
            tick.record(i, &rows);
        }
        clause_i = i + 1 + skip;
    }
    Ok((rows, last_result))
}

/// The PROFILE path's per-clause snapshot: the site's sink and its
/// `(stage, part)` position, the arm's start `Instant`, and the
/// store-access counter total at that moment. Owning the site makes the
/// half-states — a tick with no sink to report to, a sink with no
/// position to key it — unrepresentable. `db_hits` is the delta the
/// clause's arm moved the counter, so the metric rides the executor's
/// existing graph-access counters with no second counting mechanism.
struct ProfileTick<'a> {
    /// The statement's PROFILE site, held whole — the sink↔position
    /// pairing stays the site type's invariant, never re-spelled as
    /// parallel flat fields here.
    site: PlanMetricsSite<'a>,
    t0: Instant,
    /// The read-kind snapshot at the arm's start (LABS-369): the tick's
    /// `reads` is the delta against it, three thread-local reads per
    /// clause, never a per-row cost — and the one snapshot, since the
    /// store-access total is the kinds' sum.
    reads: crate::ctx::StoreReads,
    /// The phase-time snapshot at the arm's start (LABS-448): the tick's
    /// `phases` is the delta against it — and arming the phase stamps is
    /// this tick's first act, disarming its last, so the stamps never run
    /// outside a profiled statement's arm.
    phases: crate::ctx::PhaseTimes,
}

impl Drop for ProfileTick<'_> {
    /// Disarm the phase stamps (LABS-448): the arm is over, so the next
    /// clause's stamps arm with its own tick — and a non-PROFILEd
    /// statement following never pays an `Instant`.
    fn drop(&mut self) {
        super::disarm_phase_profiling();
    }
}

fn profile_tick(site: PlanMetricsSite<'_>) -> ProfileTick<'_> {
    // The clause's clearing boundary (LABS-369): a non-PROFILE statement
    // records walk starts without ever taking one, so the slot must be
    // emptied here, before the clause's arm runs — otherwise the next
    // PROFILE statement's first walking clause would drop its own fresh
    // start and report the earlier statement's walk.
    let _ = super::take_walk_start();
    // LABS-448: arm the phase stamps for this arm — the only site that
    // does, so the stamps' `Instant` reads happen exclusively inside a
    // profiled clause arm.
    super::arm_phase_profiling();
    ProfileTick {
        site,
        t0: Instant::now(),
        reads: super::store_reads(),
        phases: super::phase_times(),
    }
}

impl ProfileTick<'_> {
    /// Report one clause operator's metrics — rows produced, store
    /// accesses, row-materialization bytes, elapsed — to the sink the
    /// tick carries, at the position the statement-level walker keyed.
    fn record(&self, clause: usize, rows: &[Row]) {
        let reads = super::store_reads().since(self.reads);
        self.site.sink.record_clause(ClauseMetricsRecord {
            stage: self.site.stage,
            part: self.site.part,
            clause,
            rows: rows.len() as u64,
            memory_bytes: rows_logical_bytes(rows),
            time_ns: self.t0.elapsed().as_nanos(),
            reads,
            phases: super::phase_times().since(self.phases),
            start: super::take_walk_start(),
        });
    }
}

fn try_match_folds(
    graph: &ExecGraph<'_>,
    rows: &[Row],
    patterns: &[cypherlite_ir::PathPattern],
    where_clause: Option<&cypherlite_ir::Expr>,
    clauses: &[Clause],
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Option<FoldedReturn>> {
    if let Some(folded) = try_fold_boolean_count(graph, rows, clauses, params, cypher_version)? {
        return Ok(Some(folded));
    }
    // `skip_rel` keeps one relationship out of the next hop, which is the
    // DIFFERENT RELATIONSHIPS rule. A single hop has nothing to skip, so
    // REPEATABLE ELEMENTS still uses the folds. Wider patterns decline and the
    // ordinary matcher applies the mode.
    let single_hop = patterns.len() == 1 && patterns[0].rels.len() == 1;
    if !single_hop
        && matches!(
            clauses.first(),
            Some(Clause::Match {
                match_mode: MatchMode::RepeatableElements,
                ..
            })
        )
    {
        return Ok(None);
    }
    // The path fold first: it compiles the full hop plan once and serves the
    // widest coverage. The one-hop fold follows and serves exactly the
    // queries the path fold declined — no coverage gate between them, so
    // no query compiles the plan twice.
    if let Some(folded) = try_fold_path_aggs(
        graph,
        rows,
        patterns,
        where_clause,
        &clauses[1..],
        params,
        cypher_version,
    )? {
        return Ok(Some(folded));
    }
    try_fold_one_hop_aggs(
        graph,
        rows,
        patterns,
        where_clause,
        &clauses[1..],
        params,
        cypher_version,
    )
}

pub(crate) fn execute_part_read_seeded(
    graph: &ExecGraph<'_>,
    clauses: &[Clause],
    params: &BTreeMap<String, Value>,
    rows: Vec<Row>,
    qctx: QueryCtx<'_>,
    profile: Option<PlanMetricsSite<'_>>,
) -> Result<QueryResult> {
    let (mut rows, last_result) = run_clauses_mode(
        graph,
        ClauseMode::Read,
        clauses,
        params,
        rows,
        qctx,
        profile,
    )?;
    // The read path reports no write stats; its wrapper resets stats on
    // the way out regardless.
    Ok(concluding_part_result(
        clauses,
        &mut rows,
        last_result,
        QueryStats::default(),
    ))
}
