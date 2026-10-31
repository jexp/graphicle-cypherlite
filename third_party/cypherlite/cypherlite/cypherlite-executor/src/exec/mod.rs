//! Query execution entry points, plan folding, and the `exec/` submodule
//! wiring.

use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{CancelToken, Error, QueryClockGuard, Result, Value};
use cypherlite_ir::{Clause, LabelExpr};
use cypherlite_planner::{clause_mutates_store, plan_any_clause, LogicalPlan};

use crate::ctx::{PlanMetricsSink, PlanMetricsSite};

use crate::eval::Row;

// The instrumentation cluster (LABS-448's extraction) — one private
// module, re-exported here so the walkers' `super::` paths keep working:
// the store-read counters, the walk-start slot, and the phase clocks.
mod profiling;
use crate::graph::{Graph, QueryStats, SchemaSnapshot};
use crate::group_key::{row_key, GroupKey};
use crate::overlay::ExecGraph;
use crate::procedure_registry::call_writes_store;
use crate::StatementEffect;
pub(crate) use profiling::{
    arm_phase_profiling, count_hop_expand, count_neighbor_walk, count_node_reads,
    disarm_phase_profiling, phase_seek, phase_times, phase_walk, record_walk_start, store_reads,
    take_walk_start,
};
#[cfg(test)]
pub(crate) use profiling::{take_hop_expands, take_neighbor_walks, take_node_reads};

pub(crate) mod agg_fold;
pub(crate) mod aggregate;
pub(crate) mod bulk;
pub(crate) mod call;
pub(crate) mod candidates;
pub(crate) mod catalog;
pub(crate) mod catalog_schema;
pub(crate) mod catalog_show;
pub(crate) mod constraint_validation;
pub(crate) mod create;
pub(crate) mod ddl;
pub(crate) mod dispatch;
pub(crate) mod exists_probe;
pub(crate) mod expand;
pub(crate) mod far_probe;
pub(crate) mod gpm;
pub(crate) mod match_cap;
pub(crate) mod match_clause;
pub(crate) mod ontology;
pub(crate) mod pivot;
pub(crate) mod procedure_rows;
pub(crate) mod procedures;
pub(crate) mod project;
pub(crate) mod property_defaults;
pub(crate) mod props;
pub(crate) mod pushdown;
pub(crate) mod scope;
pub(crate) mod search;
pub(crate) mod subquery_eval;
pub(crate) mod unwind;
pub(crate) mod vector;
pub(crate) mod write;

pub use procedures::value_matches_proc_type;

/// The read path's one rejection policy: the message a `ClauseMode::Read`
/// clause loop refuses `c` with, or `None` to dispatch it. The dispatcher
/// asks once per clause, before its clause match.
///
/// The table is the planner's published per-clause answer
/// ([`clause_mutates_store`], catalog commands included: the `SHOW` family is
/// a read, a catalog DDL command is a write) plus the two shapes this layer
/// decides: `FOREACH` names itself, and `CALL { … } IN TRANSACTIONS` needs a
/// write transaction. A catalog write reaching a read run is therefore a
/// typed rejection, not a write that only the read-only scratch's panic
/// backstops.
///
/// The name states the policy, not a clause classification: it answers "does
/// the read path reject this clause, and with what message?", which is
/// narrower than the mutation question the composition rules ask
/// ([`mutates_store`]).
pub(crate) fn read_path_rejection(c: &Clause) -> Option<&'static str> {
    match c {
        Clause::Foreach { .. } => Some("internal: FOREACH on read path"),
        Clause::CallSubquery {
            in_transactions: Some(_),
            ..
        } => Some("internal: CALL IN TRANSACTIONS requires a write transaction"),
        other if clause_mutates_store(other) => Some("internal: write clause on read path"),
        _ => None,
    }
}

/// Whether one clause mutates the store — the per-clause form of
/// [`plan_mutates_store`], shared with the composition rules that ask the
/// mutation question rather than the read-admission one. The two answers
/// this layer decides below the planner table live here:
///
/// - `LOAD CSV` is `Write` in the clause table (write-*routed* by the
///   conservative access fold, ADR-0332) but only reads a file;
/// - a `CALL` is the procedure registry's writes flag
///   ([`call_writes_store`]) — a registered procedure is a static row table
///   and cannot mutate the store.
pub(crate) fn mutates_store(clause: &Clause) -> bool {
    match clause {
        Clause::LoadCsv { .. } => false,
        Clause::Call { name, .. } => call_writes_store(name),
        other => clause_mutates_store(other),
    }
}

/// Whether `plan` holds a mutation operator — the statement's ADR-0402
/// Decision 2 write-attempt fact, recorded once at statement entry (before
/// any rows flow) through the canonical plan walk
/// (`cypherlite_planner::plan_any_clause` over `walk_plan_clauses`): every
/// part, every `NEXT` stage, and every nested body the walk descends into
/// (`CALL { … }`, `WHEN`/`ELSE`, `FOREACH` bodies, expression subqueries). A
/// write inside a body that never dispatches — an empty driving row set, an
/// untaken branch — is therefore still an attempt, which is the same
/// short-circuit the empty top-level pipeline is made of. Both statement
/// boundaries that name an effect record it here — the write path and the
/// read admission — so the fact has one source on every route.
///
/// The planner's [`clause_mutates_store`] answers each clause; the two shapes
/// whose answer lives at this layer are decided by [`mutates_store`], which
/// this walk applies clause by clause.
pub(crate) fn plan_mutates_store(plan: &LogicalPlan) -> bool {
    plan_any_clause(plan, &mut mutates_store)
}

use constraint_validation::validate_constraints;
use dispatch::{execute_part_read_seeded, run_clauses};
use scope::visible_keys;
pub(crate) use subquery_eval::{eval_collect_query, eval_count_query, eval_exists_query};

use crate::ctx::QueryCtx;

/// The lowering duplicates a static label/type list into a `LabelExpr`
/// (`labels`/`types` carry the same names). A label expression is
/// batch-safe exactly when it is a tree of static `Label` leaves over
/// precisely those names — no wildcards, dynamics, or negation.
/// Hoisted from `exec/bulk.rs` (round 4, F-4-1): the static-mirror
/// discrimination the bulk-write recognizer and the count-only shape
/// recognizer share — one canonical helper per policy.
pub(crate) fn static_label_tree(expr: &LabelExpr, names: &[String]) -> bool {
    fn leaves<'a>(expr: &'a LabelExpr, out: &mut Vec<&'a str>) -> bool {
        match expr {
            LabelExpr::Label(name) => {
                out.push(name);
                true
            }
            LabelExpr::And(parts) | LabelExpr::Or(parts) => {
                parts.iter().all(|part| leaves(part, out))
            }
            _ => false,
        }
    }
    let mut found = Vec::new();
    leaves(expr, &mut found)
        && found.len() == names.len()
        && found.iter().all(|name| names.iter().any(|n| n == name))
}

/// Whether an optional label expression is absent or static-matches
/// `names` — the one gate every label-expression-accepting recognizer
/// consults: gate on static-vs-dynamic, never on presence (round 4,
/// F-4-1/F-4-2: the raw front-lowered shape carries the static mirror,
/// and a presence gate would decline real pipeline shapes).
pub(crate) fn label_expr_ok(expr: &Option<LabelExpr>, names: &[String]) -> bool {
    expr.as_ref().is_none_or(|e| static_label_tree(e, names))
}

pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub stats: QueryStats,
    /// The engine's truthful write effect for this statement (ADR-0402
    /// Decision 2), named once at the statement boundary from two execution
    /// facts: whether the statement's plan holds a mutation operator (the one
    /// plan walk, [`plan_mutates_store`]) and whether any change reached the
    /// write redo since the statement's entry reset. A read therefore carries
    /// [`StatementEffect::None`]. Results built *inside* a statement (a
    /// clause's rows, a nested body's output) go through the crate-private
    /// `QueryResult::intermediate` and never spell this field: the boundary is
    /// the one place that names it. See [`StatementEffect`] for why neither
    /// the plan's query type nor the mutation counters can answer this.
    pub effect: StatementEffect,
}

impl QueryResult {
    /// A result a statement boundary finished, its effect named by the
    /// execution that produced it — the construction for a crate outside this
    /// one (the ADR-0209 batched bulk write is its own statement boundary and
    /// runs no clause loop). The effect always comes from
    /// [`StatementEffect`]'s own constructors, so a caller never spells a
    /// zero state at the construction site.
    pub fn finished(
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
        stats: QueryStats,
        effect: StatementEffect,
    ) -> Self {
        Self {
            columns,
            rows,
            stats,
            effect,
        }
    }

    /// A result built *inside* a statement — a clause's rows, a nested
    /// body's output, a finishing projection — before the boundary names its
    /// effect.
    ///
    /// The effect starts as the unstamped read state; the statement boundary
    /// ([`execute`] on the write path, `execute_read_at` on the read path) is
    /// the one place that names it from the execution's own facts. Routing
    /// every intermediate construction through this constructor is what keeps
    /// a placeholder out of the construction sites: no clause can hand a
    /// surface an effect that claims a write applied nothing.
    pub(crate) fn intermediate(
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
        stats: QueryStats,
    ) -> Self {
        Self::finished(columns, rows, stats, StatementEffect::None)
    }
}

/// The plan, parameters, source text, and authenticated principal shared by
/// every execution entry point. `query_text` is the source text of the
/// statement (exposed as `SHOW TRANSACTIONS`' `currentQuery` column); the
/// statement's CIP-150 version preamble rides on the plan. `principal` is the
/// connection's authenticated principal (ADR-0273), read by
/// `dbms.showCurrentUser`; `None` reports the documented placeholder.
/// `Copy`: every field is a reference (or an optional reference) — the
/// commit path reuses the caller's request with its own batch-commit
/// sink attached, without rebuilding the bundle field by field.
#[derive(Clone, Copy)]
pub struct ExecRequest<'a> {
    pub plan: &'a LogicalPlan,
    pub params: &'a BTreeMap<String, Value>,
    pub query_text: &'a str,
    pub principal: Option<&'a str>,
    /// ADR-0210: per-batch durable-commit sink for `CALL { … } IN
    /// TRANSACTIONS`; `None` keeps the accumulate-to-statement-end
    /// behavior.
    pub batch_commit: Option<&'a dyn crate::ctx::BatchCommit>,
    /// ADR-0344 phase 2: the PROFILE metrics sink for the statement's own
    /// clauses; `None` on every non-PROFILE run. The dispatch loop
    /// records one entry per clause operator; nested execution strips it.
    pub plan_metrics: Option<&'a dyn crate::ctx::PlanMetricsSink>,
}

impl<'a> ExecRequest<'a> {
    /// The None-default request: no principal, no batch-commit sink, no
    /// PROFILE sink — the statement's own execution with none of the
    /// optional carriers. The one construction site for the defaults, so
    /// a new optional field lands here, not at every caller.
    pub fn new(
        plan: &'a LogicalPlan,
        params: &'a BTreeMap<String, Value>,
        query_text: &'a str,
    ) -> Self {
        Self {
            plan,
            params,
            query_text,
            principal: None,
            batch_commit: None,
            plan_metrics: None,
        }
    }
}

/// A read admitted at a caller-pinned timestamp: the `graph`, the pinned
/// `snapshot_ts`, and the `schema` captured together with that pin under the
/// publication read guard, plus the `request` to run and its optional
/// cooperative cancellation token (ADR-0111; `None` is never cancelled).
pub struct ReadAdmission<'a> {
    pub graph: &'a Graph,
    pub snapshot_ts: u64,
    pub schema: &'a SchemaSnapshot,
    pub request: ExecRequest<'a>,
    pub cancel: Option<&'a CancelToken>,
    /// The store's ontology document (verbatim) captured at admission
    /// (ADR-0121 §3, ADR-0227): attached to the read view like `cancel`
    /// and read by `db.ontology.get` / `db.ontology.apply`; `None` when
    /// the store holds no ontology.
    pub ontology_document: Option<&'a str>,
}

/// Execute a write-capable plan, then validate the constraints against what
/// it wrote (ADR-0172). Each call starts a fresh CSV memo even when `graph`
/// is reused across statements.
pub fn execute(graph: &ExecGraph<'_>, request: &ExecRequest<'_>) -> Result<QueryResult> {
    let (result, attempted) = run_statement(graph, request)?;
    // CIP-262: the statement is this engine's transaction, so the graph
    // type's property default values land at its boundary -- after the
    // writes (the statement's own RETURN already saw null, CE1), before
    // the post-statement constraint validation sees the defaulted
    // properties like any other write. They are writes, so they land
    // before the effect's applied fact is read (below).
    property_defaults::apply_property_defaults(graph)?;
    validate_constraints(graph)?;
    finish_statement(result, graph, attempted)
}

/// [`execute`] without the constraint validation: the statement's writes
/// stay in the redo slice the next `validate_constraints` checks. The
/// executor's own tests drive this entry (`exec::test_support`); production
/// always takes the validated one.
///
/// It is the same statement boundary, so it names the effect from its own two
/// facts rather than handing back a placeholder: a caller that stops here
/// still cannot read a read state off a statement that wrote.
#[cfg(test)]
pub(crate) fn execute_unvalidated(
    graph: &ExecGraph<'_>,
    request: &ExecRequest<'_>,
) -> Result<QueryResult> {
    let (result, attempted) = run_statement(graph, request)?;
    finish_statement(result, graph, attempted)
}

/// The private un-finished runner every write statement entry shares: the
/// per-statement resets, the entry checks, the entry facts, and the plan's
/// execution. It never names the effect and never polls the token ledger —
/// [`finish_statement`] does both, exactly once, so `execute` and
/// `execute_unvalidated` cannot stamp a statement twice.
///
/// Returns the un-finished result and the statement's write-attempt fact
/// ([`plan_mutates_store`]), recorded here before any rows flow and handed to
/// [`finish_statement`] unchanged.
fn run_statement(graph: &ExecGraph<'_>, request: &ExecRequest<'_>) -> Result<(QueryResult, bool)> {
    let attempted = plan_mutates_store(request.plan);
    // One statement entry: the CSV memo, the statement-spanning scratch state
    // (ADR-0309 stats + the ADR-0402 applied count) and the ADR-0395
    // nested-evaluation budget restart together.
    graph.begin_statement();
    graph.check_cancelled()?;
    let _clock = QueryClockGuard::new();
    let qctx = QueryCtx::new(request.plan.cypher_version, request.query_text)
        .with_principal(request.principal)
        .with_batch_commit(request.batch_commit);
    let result = execute_plan_stage_seeded(
        graph,
        request.plan,
        request.params,
        vec![Row::new()],
        qctx,
        request.plan_metrics,
    )?;
    Ok((result, attempted))
}

/// Execute a read-only plan at the caller-pinned timestamp in `admission`
/// without repinning. The caller must keep its `Graph::pin_now()` guard alive
/// through this call so version GC cannot discard rows visible at
/// `snapshot_ts`, and must capture `schema` alongside that pin while
/// serializing with graph publication.
///
/// There is deliberately no graph-only wrapper that acquires both itself: the
/// publication read guard lives on the embedded database's `Shared` state, not
/// on a bare `Graph`, so such a wrapper could not capture the pin and schema
/// atomically. Admission must happen under that guard at the caller.
pub fn execute_read_at(admission: &ReadAdmission<'_>) -> Result<QueryResult> {
    let view = ExecGraph::read_at(admission.graph, admission.snapshot_ts)
        .with_schema(admission.schema)
        .with_cancel(admission.cancel)
        .with_ontology_document(admission.ontology_document);
    view.check_cancelled()?;
    let _clock = QueryClockGuard::new();
    // A read admission records its write-attempt fact from the same canonical
    // plan walk as every write entry — never a hardcoded "a read cannot
    // write": a mutation operator reaching this route is still an attempt (the
    // read-only scratch refuses to apply it). A read view applies nothing, so
    // its applied count is the zero the admission starts with.
    let attempted = plan_mutates_store(admission.request.plan);
    // ADR-0395: the freshly constructed view carries a zeroed counter —
    // the read admission's statement gets its own budget.
    let qctx = QueryCtx::new(
        admission.request.plan.cypher_version,
        admission.request.query_text,
    )
    .with_principal(admission.request.principal);
    let mut result = execute_plan_read_seeded(
        &view,
        admission.request.plan,
        admission.request.params,
        vec![Row::new()],
        qctx,
        admission.request.plan_metrics,
    )?;
    result.stats = QueryStats::default();
    finish_statement(result, &view, attempted)
}

/// Name the statement's effect and finish the result — the one assignment to
/// [`QueryResult::effect`], and the one token-ledger poll. Every entry into a
/// statement (the write path, the unvalidated path, a read admission) runs
/// the un-finished runner and ends here exactly once, so no entry can return
/// a fabricated effect for a statement that wrote, and none stamps twice.
///
/// The two facts are taken where they are known: `attempted` from the plan
/// walk at statement entry, `applied` from the statement's own change count —
/// zeroed by the same entry reset as the stats and grown by every redo push,
/// so no baseline has to travel beside it.
fn finish_statement(
    mut result: QueryResult,
    graph: &ExecGraph<'_>,
    attempted: bool,
) -> Result<QueryResult> {
    result.effect = StatementEffect::from_facts(attempted, graph.applied_ops() > 0);
    finish_tokens(result)
}

fn finish_tokens(result: QueryResult) -> Result<QueryResult> {
    cypherlite_core::tokens::poll()?;
    Ok(result)
}

pub(crate) fn result_seed_rows(result: &QueryResult) -> Vec<Row> {
    result
        .rows
        .iter()
        .map(|values| {
            result
                .columns
                .iter()
                .enumerate()
                .map(|(i, name)| (name.clone(), values.get(i).cloned().unwrap_or(Value::Null)))
                .collect()
        })
        .collect()
}

/// Result of the rows' visible keys (sorted). This clones: the clause-exit
/// PROFILE tick still reads these rows. `call::materialize_rows` is the
/// move path, used once a tick has already recorded the clause.
pub(crate) fn rows_as_result(graph: &ExecGraph<'_>, rows: &[Row]) -> QueryResult {
    let mut columns = rows.first().map(visible_keys).unwrap_or_default();
    columns.sort();
    let data = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| row.get(column).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    QueryResult::intermediate(columns, data, graph.stats())
}

/// One stage-walk driver for both dispatch modes (ADR-0344 phase 2
/// review): fold the stage's parts through the runner, then recurse into
/// the NEXT stage with the shared seeding policy (ADR-0287) and the
/// stage count — the scaffolding exists once; the write and read
/// entries supply only their per-part runner. The stage travels with the
/// walk (the tree's stage numbering: the root is stage 1, every NEXT
/// link one later), so the PROFILE metrics key's stage fact stays the
/// tree's own.
/// A stage's per-part runner: the stage's number, the part's index, its
/// clauses, and its seed rows into that part's result — the write and
/// read walkers supply their own mode.
type PartRunner<'a> = dyn Fn(usize, usize, &[Clause], Vec<Row>) -> Result<QueryResult> + 'a;

fn walk_plan_stages(
    plan: &LogicalPlan,
    stage: usize,
    seeds: Vec<Row>,
    run_part: &PartRunner,
) -> Result<QueryResult> {
    // A stage with one part consumes the seeds. Later parts of a UNION
    // each need the same bag, so those plans still clone it.
    let result = if plan.parts.len() == 1 {
        run_part(stage, 0, plan.parts[0].as_slice(), seeds)?
    } else {
        fold_parts(plan, |part, part_clauses| {
            run_part(stage, part, part_clauses, seeds.clone())
        })?
    };
    match &plan.next {
        Some(next) => walk_plan_stages(
            next,
            stage + 1,
            next_seeds(&result, plan.parts.iter().map(|p| p.as_slice())),
            run_part,
        ),
        None => Ok(result),
    }
}

fn execute_plan_stage_seeded(
    graph: &ExecGraph<'_>,
    plan: &LogicalPlan,
    params: &BTreeMap<String, Value>,
    seeds: Vec<Row>,
    qctx: QueryCtx<'_>,
    sink: Option<&dyn PlanMetricsSink>,
) -> Result<QueryResult> {
    walk_plan_stages(plan, 1, seeds, &|stage, part, part_clauses, part_seeds| {
        let (mut rows, last) = run_clauses(
            graph,
            part_clauses,
            params,
            part_seeds,
            qctx,
            // The statement-level walker: the PROFILE site — the sink the
            // admission attached and this part's (stage, part) key, one
            // fact — rides to the clause loop.
            sink.map(|sink| PlanMetricsSite::at(sink, stage, part)),
        )?;
        Ok(call::concluding_part_result(
            part_clauses,
            &mut rows,
            last,
            graph.stats(),
        ))
    })
}

/// The NEXT-stage seeding policy (one spelling, every NEXT-chain walker
/// reads it — both pass their parts iterator, plan-level or subquery):
/// the stage's result rows, or — for a stage whose unit output is a
/// single row (ADR-0287) — one empty seed so the next stage still runs.
pub(crate) fn next_seeds<'p>(
    result: &QueryResult,
    parts: impl Iterator<Item = &'p [Clause]>,
) -> Vec<Row> {
    let mut seeds = result_seed_rows(result);
    if seeds.is_empty() && parts_have_unit_output(parts) {
        seeds.push(Row::new());
    }
    seeds
}

pub(crate) fn execute_plan_read_seeded(
    graph: &ExecGraph<'_>,
    plan: &LogicalPlan,
    params: &BTreeMap<String, Value>,
    seeds: Vec<Row>,
    qctx: QueryCtx<'_>,
    sink: Option<&dyn PlanMetricsSink>,
) -> Result<QueryResult> {
    walk_plan_stages(plan, 1, seeds, &|stage, part, part_clauses, part_seeds| {
        execute_part_read_seeded(
            graph,
            part_clauses,
            params,
            part_seeds,
            qctx,
            // The statement-level walker, read mode: the PROFILE site —
            // sink and this part's (stage, part) key, one fact — rides to
            // the clause loop here too.
            sink.map(|sink| PlanMetricsSite::at(sink, stage, part)),
        )
    })
}

/// Whether a finished stage seeds the next `NEXT` stage with one unit row
/// when its own result is empty (ADR-0287). Data-write parts
/// (`CREATE`/`MERGE`/`DELETE`/`SET`/`REMOVE`/`FOREACH`/`CALL { … }`) and
/// `FINISH` count, and so do schema/admin DDL parts — the IR-native
/// index/constraint variants outright, and `Clause::Catalog` exactly when
/// the canonical `cypherlite-front` predicate says the command is NOT
/// row-producing. Row-producing catalog commands (the SHOW family and
/// `TERMINATE TRANSACTIONS`) keep their row semantics: they seed the next
/// stage from their projected rows, never with a unit row.
fn parts_have_unit_output<'p>(mut parts: impl Iterator<Item = &'p [Clause]>) -> bool {
    parts.all(|part| match part.last() {
        Some(
            Clause::Finish
            | Clause::Create { .. }
            | Clause::Merge { .. }
            | Clause::Delete { .. }
            | Clause::Set { .. }
            | Clause::Remove { .. }
            | Clause::Foreach { .. }
            | Clause::CallSubquery { .. }
            | Clause::CreateIndex { .. }
            | Clause::DropIndex { .. }
            | Clause::CreateConstraint { .. }
            | Clause::DropConstraint { .. },
        ) => true,
        Some(Clause::Catalog(command)) => {
            !cypherlite_front::show_columns::catalog_produces_rows(command)
        }
        _ => false,
    })
}

pub(crate) fn fold_parts(
    plan: &LogicalPlan,
    mut run: impl FnMut(usize, &[Clause]) -> Result<QueryResult>,
) -> Result<QueryResult> {
    let mut acc: Option<QueryResult> = None;
    // `seen` is the UNION joiner state [`union_merge`] owns.
    let mut seen: Option<HashSet<Vec<GroupKey>>> = None;
    for (i, part) in plan.parts.iter().enumerate() {
        let r = run(i, part)?;
        // The joiner attaching this part: part 0 looks ahead to the first
        // joiner, every later part looks back at its own. A lone part (no
        // joiners at all) is never deduplicated.
        let all = plan
            .union_all
            .get(if i == 0 { 0 } else { i - 1 })
            .copied()
            .unwrap_or(true);
        if let Some(prev) = &acc {
            if prev.columns != r.columns {
                return Err(Error::semantic("UNION requires same columns"));
            }
        }
        union_merge(&mut acc, r, all, &mut seen);
    }
    Ok(acc.unwrap())
}

/// The one UNION joiner spelling (ADR-0344 review): both part folds —
/// the plan-level [`fold_parts`] and the subquery walker
/// (`call::execute_query_seeded`) — merge through it, so the dedup
/// policy cannot drift between them. `all` (UNION ALL) appends
/// untouched; a UNION joiner drops every row whose canonical key
/// (GroupKey: 1 = 1.0, null = null, every NaN distinct) an earlier part
/// already contributed — the accumulated rows deduplicated in place,
/// the part's rows retain-before-append. `seen` holds the key of every
/// accumulated row from the first UNION joiner on; `None` before it
/// (UNION ALL appends do no hashing at all). Joiners are uniform across
/// boundaries (mixing UNION and UNION ALL is a parse error), so the
/// in-place accumulated dedup is a no-op in every reachable plan: once
/// a UNION joiner ran, every appended row was retained against `seen`.
pub(crate) fn union_merge(
    acc: &mut Option<QueryResult>,
    mut part: QueryResult,
    all: bool,
    seen: &mut Option<HashSet<Vec<GroupKey>>>,
) {
    let Some(prev) = acc else {
        // The first part: deduplicated in place when its (lookahead)
        // joiner is UNION — the plan-level fold's part 0; the subquery
        // fold passes `true`, its first part has no joiner.
        if !all {
            let seen = seen.get_or_insert_with(HashSet::new);
            part.rows.retain(|row| seen.insert(row_key(row)));
        }
        *acc = Some(part);
        return;
    };
    if all {
        // UNION ALL appends untouched; once dedup has started, its keys
        // still join `seen` so a later UNION joiner observes them.
        if let Some(seen) = seen {
            seen.extend(part.rows.iter().map(|row| row_key(row)));
        }
    } else {
        // A UNION joiner dedups against a fresh `seen`: the
        // accumulated pass dedups in place and seeds the keys in one
        // walk, the part's pass retain-before-append. The accumulated
        // rows are already unique (every earlier joiner retained them),
        // so the in-place pass is their key-collection walk.
        let mut fresh: HashSet<Vec<GroupKey>> = HashSet::new();
        prev.rows.retain(|row| fresh.insert(row_key(row)));
        part.rows.retain(|row| fresh.insert(row_key(row)));
        *seen = Some(fresh);
    }
    prev.rows.extend(part.rows);
    prev.stats.merge(&part.stats);
}

#[cfg(test)]
mod aggregate_tests;
mod count_stats;
#[cfg(test)]
mod count_stats_tests;
#[cfg(test)]
mod direct_fold_tests;
#[cfg(test)]
mod effect_tests;
#[cfg(test)]
mod exists_probe_tests;
#[cfg(test)]
mod far_node_tests;
mod foreach;
#[cfg(test)]
mod foreach_tests;
pub mod graph_stats;
#[cfg(test)]
mod id_seek_tests;
#[cfg(test)]
mod integer_arg_tests;
#[cfg(test)]
mod nested_eval_tests;
#[cfg(test)]
mod path_fold_tests;
#[cfg(test)]
mod pivot_tests;
#[cfg(test)]
mod project_tests;
#[cfg(test)]
mod pushdown_tests;
#[cfg(test)]
mod rel_seek_tests;
#[cfg(test)]
mod shortest_expression_tests;
#[cfg(test)]
mod shortest_path_tests;
#[cfg(test)]
mod subquery_eval_tests;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod text_seek_tests;
