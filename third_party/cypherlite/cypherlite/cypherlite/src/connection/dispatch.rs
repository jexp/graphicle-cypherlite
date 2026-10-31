//! Statement admission and dispatch (ADR-0248, ADR-0344/0345 phase 2).
//!
//! The statement's execution facts — admission, the dispatch context
//! (`DispatchCtx`: stamp, start, PROFILE sink), the per-transaction run
//! facts, and the outcome vocabulary — live here, beside the methods
//! that consume them; `Connection`'s lifecycle (transactions, namespace,
//! pull) stays in the parent module.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use web_time::Instant;

use cypherlite_executor::{
    execute_read_at, ExecGraph, ExecRequest, Graph, PlanMetricsSink, QueryResult as ExecResult,
    ReadAdmission, WriteScratch,
};
use cypherlite_front::parse_and_lower_statement_with_preview;

use cypherlite_core::Namespace;
use cypherlite_planner::{is_database_admin_command, is_show_databases_with_next, LogicalPlan};

use super::txn::TxnRun;
use super::Connection;
use crate::commit::commit_write;
use crate::explain;
use crate::gate::ResultStamp;
use crate::{CancelToken, Error, QueryResult, Result, ResultHeader, Value};

/// A statement's staged outcome (ADR-0319: `Err` before dispatch, `Ok` with
/// the dispatch outcome after) together with whether it admitted its read
/// before it ran. `admitted` stays true when an admitted read then fails, so
/// its caller can skip the post-run health check for a read that already
/// pinned the last safe snapshot; a read that finished keeps its rows —
/// nothing after execution (the PROFILE estimates read included) may turn a
/// finished result back into an error.
pub(super) struct QueryOutcome {
    pub(super) result: Result<Executed>,
    pub(super) admitted: bool,
}

impl QueryOutcome {
    pub(super) fn not_admitted(result: Result<Executed>) -> Self {
        Self {
            result,
            admitted: false,
        }
    }
}

/// The outcome of a statement that parsed, validated and planned
/// (ADR-0319): it either produced its result or failed while executing.
#[derive(Debug)]
pub enum Executed {
    /// Execution finished; the buffered result.
    Rows(QueryResult),
    /// Execution of the planned statement failed.
    Failed(ExecutionFailure),
}

/// A statement that planned but failed while executing (ADR-0319): what a
/// surface needs to announce the result before it reports the error.
#[derive(Debug)]
pub struct ExecutionFailure {
    /// The planned result metadata — the same header a `QueryResult`
    /// carries: the output `columns`
    /// (`cypherlite_planner::plan_output_names`), their `column_types`
    /// (`ANY` per column, ADR-0224's no-rows stamp), the plan-derived
    /// `query_type` (ADR-0162), the `namespace` (ADR-0117 §7) and the
    /// `elapsed` (ADR-0221).
    pub header: ResultHeader,
    /// The execution error.
    pub error: Error,
}

impl ExecutionFailure {
    /// Stage a dispatch-phase failure of `logical`. A planned statement's
    /// output names always derive (the UNION column-agreement check ran in
    /// `plan`), so [`plan_output_names_expect`] holds the one expect for
    /// that invariant — the same policy the planner's `query_type` uses
    /// (ADR-0319, ADR-0332).
    ///
    /// [`plan_output_names_expect`]: cypherlite_planner::plan_output_names_expect
    pub(crate) fn staged(
        logical: &LogicalPlan,
        stamp: ResultStamp,
        elapsed: Duration,
        error: Error,
    ) -> Executed {
        let columns = cypherlite_planner::plan_output_names_expect(logical);
        Executed::Failed(Self {
            // The no-rows construction (`&[]`): every column stamps `ANY`
            // (ADR-0224), through the same one construction as every other
            // result.
            header: ResultHeader::new(columns, &[], stamp.qtype, stamp.namespace, elapsed),
            error,
        })
    }
}

impl Executed {
    /// The finished result, or the execution error: the shape of the
    /// non-staged entry points (`query`, `query_with_params`, …).
    pub fn into_result(self) -> Result<QueryResult> {
        match self {
            Executed::Rows(result) => Ok(result),
            Executed::Failed(failure) => Err(failure.error),
        }
    }
}

/// What [`Connection::admit_statement`] resolves: the parsed preamble
/// flags plus the gated, classified logical plan — one bundle so
/// `run_query` and `explain_plan` share the single admission sequence.
pub(super) struct Admission {
    explain: bool,
    profile: bool,
    /// The plan-derived administration fact, resolved once at admission
    /// (ADR-0116 §2): the EXPLAIN arm, the PROFILE epilogue, the dispatch
    /// target selection and the notification view all read this one value
    /// instead of re-deriving it from the plan.
    admin: bool,
    pub(super) logical: LogicalPlan,
    stamp: ResultStamp,
}

/// The dispatch-only inputs bundled so `dispatch` stays under
/// `clippy::too_many_arguments`: the stamp plus the statement's start
/// instant (ADR-0221), whose elapsed is stamped at result construction.
pub(super) struct DispatchCtx<'a> {
    stamp: ResultStamp,
    started: Instant,
    /// The carried admission fact: whether the statement is a
    /// database-administration command (ADR-0116 §2).
    admin: bool,
    /// ADR-0344 phase 2: the PROFILE metrics sink when the statement was
    /// `PROFILE`; `None` on every other run. Rides the ctx like the
    /// stamp: built and coerced to the trait object once in
    /// `run_query`, read at the dispatch's request sites.
    profile_metrics: Option<&'a dyn PlanMetricsSink>,
}

/// The PROFILE epilogue's inputs beyond the dispatch outcome: the plan and
/// parameters the estimate tree reads, the statement echo, and the
/// admission facts the notification view needs. One bundle so the epilogue
/// stays one call with one argument list.
struct ProfileEpilogue<'a> {
    logical: &'a LogicalPlan,
    params: &'a BTreeMap<String, Value>,
    cypher: &'a str,
    admin: bool,
    namespace: Namespace,
    metrics: &'a crate::profile::ProfileMetrics,
}

/// The admission gates between the EXPLAIN early return and dispatch: the
/// ADR-0308 parameter-completeness check and the SHOW DATABASES + NEXT
/// restriction. One named seam, so `run_query` reads as
/// parse → plan → gates → dispatch.
fn post_explain_gates(logical: &LogicalPlan, params: &BTreeMap<String, Value>) -> Result<()> {
    // ADR-0308: a referenced parameter the caller did not supply fails the
    // statement before anything executes (EXPLAIN, above, plans without
    // values, as in Neo4j).
    let missing: Vec<&str> = cypherlite_planner::plan_parameter_names(logical)
        .into_iter()
        .filter(|name| !params.contains_key(*name))
        .collect();
    if !missing.is_empty() {
        return Err(Error::parameters_missing(missing));
    }
    // SHOW DATABASES is a system-database command (ADR-0116 §2); combining
    // it with a NEXT stage is an execution-phase rule, so it does not apply
    // to EXPLAIN. The §2 classification lives in the planner's namespaces
    // module.
    if is_show_databases_with_next(logical) {
        return Err(Error::execution(
            "SystemDatabaseRequired: SHOW DATABASES cannot be combined with NEXT",
        ));
    }
    Ok(())
}

fn attach_receipt(outcome: &mut QueryOutcome) {
    if let Err(error) = cypherlite_core::tokens::poll() {
        outcome.result = Err(error);
        return;
    }
    if let Ok(Executed::Rows(result)) = outcome.result.as_mut() {
        result.receipt =
            cypherlite_core::tokens::current_receipt(cypherlite_core::TokenOutcome::Success);
    }
}

fn retain_result_rows(outcome: &QueryOutcome) -> Result<()> {
    let Ok(Executed::Rows(result)) = &outcome.result else {
        return Ok(());
    };
    let charge = cypherlite_core::tokens::WorkspaceCharge::retain_with(|| {
        cypherlite_executor::result_rows_bytes(&result.rows)
    })?;
    charge.transfer_to_ledger();
    Ok(())
}

impl Connection {
    /// The one statement-admission sequence (ADR-0259): the health gate,
    /// the failed-transaction gate, parse + front-end lowering, then the
    /// plan gates and classification. `run_query` and `explain_plan`'s
    /// worker body both enter here — one canonical entry, so no statement
    /// path can skip a gate. Errors surface through the caller's usual
    /// epilogue (the transaction fails there).
    pub(super) fn admit_statement(
        &mut self,
        cypher: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<Admission> {
        self.inner.check_healthy()?;
        if self.txn.as_ref().is_some_and(|t| t.failed.get()) {
            return Err(Error::semantic("transaction failed; rollback required"));
        }
        // ADR-0371: the store's CYPHERLITE_PREVIEW-resolved set threads
        // into the parse -- a gated preview construct (CIP-278 COMBINE)
        // fails closed with the authored disabled-feature error here,
        // before anything of it plans or executes. ADR-0378: the set has one
        // owner, the store-level `StoreServices` bundle, so admission reads
        // it from the graph under a read guard rather than keeping a second
        // copy on `Shared`.
        let preview = {
            let _read = self.inner.read_guard()?;
            self.inner.graph.preview()
        };
        let parsed = parse_and_lower_statement_with_preview(cypher, preview)?;
        let explain = parsed.explain;
        let profile = parsed.profile;
        let (logical, stamp) = self.plan_gate_classify(parsed.query, params)?;
        // ADR-0116 §2: resolved once, here, from the finished plan. Every
        // later stage reads the carried fact.
        let admin = is_database_admin_command(&logical);
        Ok(Admission {
            explain,
            profile,
            admin,
            logical,
            stamp,
        })
    }

    /// The query body, run on the connection's worker thread (ADR-0259).
    /// The five-stage narrative: admit → EXPLAIN → gates → dispatch →
    /// PROFILE epilogue. Each stage is one named call, and the admission
    /// facts ride `Admission`/`DispatchCtx` instead of being re-derived.
    pub(super) fn run_query(
        &mut self,
        cypher: &str,
        params: BTreeMap<String, Value>,
        token: &CancelToken,
    ) -> QueryOutcome {
        // ADR-0221: one measurement around the whole statement body —
        // parse, plan, gates, dispatch — stamped onto every result.
        let started = Instant::now();
        let timing = self.inner.timing_on.load(Ordering::Relaxed);
        let t_parse = timing.then(Instant::now);
        let admission = match self.admit_statement(cypher, &params) {
            Ok(admission) => admission,
            Err(e) => return self.fail_not_admitted(e),
        };
        // EXPLAIN plans and compile-time validates the wrapped statement,
        // then renders the plan — execution never touches it (no writes, no
        // procedures, no LOAD CSV). The SHOW DATABASES + NEXT restriction is
        // an execution-phase rule, so it does not apply here.
        if admission.explain {
            return match self.explain_run(cypher, &params, &admission, started) {
                Ok(result) => QueryOutcome::not_admitted(Ok(Executed::Rows(result))),
                Err(e) => self.fail_not_admitted(e),
            };
        }
        let Admission {
            profile,
            admin,
            logical,
            stamp,
            ..
        } = admission;
        // `commit_write` reserves returned rows before it appends to the
        // WAL. Explicit transactions still reserve here: their write has
        // not committed until a later `COMMIT`.
        let auto_commit_write = stamp.write_route && self.txn.is_none();
        // The admission gates between the EXPLAIN early return and dispatch:
        // the ADR-0308 parameter-completeness check and the ADR-0116 §2
        // SHOW DATABASES + NEXT restriction. One named seam.
        if let Err(e) = post_explain_gates(&logical, &params) {
            return self.fail_not_admitted(e);
        }
        let mut observed = None;
        let mut ledger = if self.inner.resolved.budget.enabled() {
            match self.reserve_tokens(cypher, &params) {
                Ok((ledger, key)) => {
                    observed = Some(key);
                    Some(ledger)
                }
                Err(error) => return QueryOutcome::not_admitted(Err(error)),
            }
        } else {
            None
        };
        let guard = ledger
            .as_mut()
            .map(cypherlite_core::tokens::LedgerGuard::install);
        super::admit_hold();
        let parse_plan_ns = crate::commit::elapsed_nanos(t_parse);
        // PROFILE runs the statement through the normal dispatch, then
        // appends the plan and execution statistics to its result. The
        // result keeps the wrapped statement's query type (ADR-0162).
        let metrics = profile.then(crate::profile::ProfileMetrics::new);
        let notification_namespace = stamp.namespace;
        // The one concrete→dyn coercion (ADR-0344 phase 2): every request
        // site carries the trait object; the concrete local stays here for
        // the PROFILE epilogue.
        let profile_metrics: Option<&dyn PlanMetricsSink> =
            metrics.as_ref().map(|m| m as &dyn PlanMetricsSink);
        let ctx = DispatchCtx {
            stamp,
            started,
            admin,
            profile_metrics,
        };
        let mut outcome = self.dispatch(&logical, &params, parse_plan_ns, cypher, token, ctx);
        let epilogue = metrics.as_ref().map(|metrics| ProfileEpilogue {
            logical: &logical,
            params: &params,
            cypher,
            admin,
            namespace: notification_namespace,
            metrics,
        });
        // Read and explicit-transaction results are charged here. An
        // auto-commit write charged its result before the WAL boundary in
        // `commit_write`, so it cannot report a retryable token failure
        // after committing.
        if guard.is_some() && !auto_commit_write {
            if let Err(error) = retain_result_rows(&outcome) {
                outcome.result = Err(error);
            }
        }
        let mut outcome = self.profile_epilogue(outcome, epilogue);
        if guard.is_some() {
            attach_receipt(&mut outcome);
        }
        let keeps_result_reservation = matches!(outcome.result, Ok(Executed::Rows(_)));
        drop(guard);
        if let Some(ledger) = ledger {
            if let Some(key) = observed {
                self.inner.resolved.budget.observe(
                    key,
                    ledger
                        .receipt(cypherlite_core::TokenOutcome::Success)
                        .peak_logical_bytes,
                );
            }
            if keeps_result_reservation {
                let Ok(Executed::Rows(result)) = outcome.result.as_mut() else {
                    unreachable!("the result-reservation flag follows the outcome");
                };
                result._retained_ledger = Some(Box::new(ledger));
            }
        }
        outcome
    }

    fn reserve_tokens(
        &self,
        cypher: &str,
        params: &BTreeMap<String, Value>,
    ) -> Result<(
        cypherlite_core::tokens::TokenLedger,
        crate::token_budget::PeakKey,
    )> {
        let commit_ts = self.inner.commit_ts()?;
        let key = crate::token_budget::PeakKey::new(commit_ts, cypher, params);
        let cost = self.inner.resolved.budget.admission_cost(&key);
        let ledger = self.inner.resolved.budget.reserve(cost)?;
        Ok((ledger, key))
    }

    /// The EXPLAIN stage: plan, compile-time validate and render the wrapped
    /// statement without executing it. The notification facts read the same
    /// view the statement would run against (ADR-0363 decision 5) — an
    /// explicit transaction's scratch writes included.
    fn explain_run(
        &self,
        cypher: &str,
        params: &BTreeMap<String, Value>,
        admission: &Admission,
        started: Instant,
    ) -> Result<QueryResult> {
        // The estimates (ADR-0218) read the store under a read guard, like
        // the plan's own admission above.
        let _read = self.inner.read_guard()?;
        let view = self.notification_view(admission.admin, admission.stamp.namespace)?;
        let stats = crate::stats::GraphPatternStats::new(&self.inner.graph, params);
        Ok(explain::explain_result(
            &admission.logical,
            admission.stamp.namespace,
            admission.stamp.qtype,
            started.elapsed(),
            &stats,
            explain::statement_echo(cypher, "EXPLAIN"),
            &view,
        ))
    }

    /// The PROFILE epilogue: append the profiled plan and its statistics to
    /// a finished result. A failed statement stays failed — PROFILE adds
    /// nothing to a staged execution failure — and a non-PROFILE run is the
    /// outcome unchanged.
    fn profile_epilogue(
        &self,
        outcome: QueryOutcome,
        input: Option<ProfileEpilogue<'_>>,
    ) -> QueryOutcome {
        let QueryOutcome { result, admitted } = outcome;
        let Some(input) = input else {
            return QueryOutcome { result, admitted };
        };
        let result = match result {
            Ok(Executed::Rows(result)) => Ok(Executed::Rows(self.profile_enrich(input, result))),
            failed => failed,
        };
        QueryOutcome { result, admitted }
    }

    /// Enrich one finished PROFILE result with the estimate tree. It reads
    /// the store for the tree's estimates (ADR-0218) under a fresh read
    /// guard — execution has finished, so this guard cannot nest a dispatch
    /// one. A refused guard (the store was poisoned while this statement
    /// ran) or a namespace that no longer resolves must never discard the
    /// finished result: the statement admitted its read (ADR-0319) and its
    /// rows came off its own pinned snapshot. The estimates are a rendering
    /// detail, so the fallback keeps the rows un-enriched — the one such
    /// site.
    fn profile_enrich(&self, input: ProfileEpilogue<'_>, result: QueryResult) -> QueryResult {
        let Some(_read) = self.inner.read_guard().ok() else {
            return result;
        };
        let view = match self.notification_view(input.admin, input.namespace) {
            Ok(view) => view,
            Err(_) => return result,
        };
        let stats = crate::stats::GraphPatternStats::new(&self.inner.graph, input.params);
        let mut profiled = explain::profile_result(
            input.logical,
            input.metrics,
            result,
            &stats,
            explain::statement_echo(input.cypher, "PROFILE"),
            &view,
        );
        if self.inner.resolved.budget.enabled() {
            if let Some(plan) = profiled.plan.as_mut() {
                if let Some(receipt) =
                    cypherlite_core::tokens::current_receipt(cypherlite_core::TokenOutcome::Success)
                {
                    plan.root.token_estimate = Some(crate::TokenEstimate {
                        peak_logical_bytes: receipt.peak_logical_bytes,
                        token_schedule: receipt.token_schedule,
                    });
                }
            }
        }
        profiled
    }

    /// The normal statement dispatch: explicit-transaction overlay,
    /// auto-commit write, or snapshot read — routed by the statement's
    /// target namespace (ADR-0116 §3, §4, §10; ADR-0117 §2): the resolved
    /// `USE` target, or the session namespace when the statement carries no
    /// `USE`. Only `neo4j` and `system` targets (and administration
    /// commands, which always use the `neo4j` catalog) reach here — every
    /// non-administration statement routed to `system_ontologies` was
    /// rejected by the namespace gate above the dispatch split
    /// (ADR-0229 §1). Each arm produces the raw executor result and its
    /// admission flag; the `stamp` metadata derived above the
    /// EXPLAIN/dispatch split is applied once, at the single tail, so no
    /// arm can return an unstamped `QueryResult`. (`stamp` is bundled to
    /// avoid `clippy::too_many_arguments`.)
    fn dispatch(
        &mut self,
        logical: &LogicalPlan,
        params: &BTreeMap<String, Value>,
        parse_plan_ns: u64,
        cypher: &str,
        token: &CancelToken,
        ctx: DispatchCtx,
    ) -> QueryOutcome {
        let namespace = ctx.stamp.namespace;
        // ADR-0116 §2: the carried admission fact — administration commands
        // run from any namespace against the store's catalog (the `neo4j`
        // graph), so they are exempt from the `system` /
        // `system_ontologies` clause rules (enforced in
        // `plan_gate_classify`, above the EXPLAIN/dispatch split).
        let admin = ctx.admin;
        let (result, admitted): (Result<ExecResult>, bool) = if let Some(txn) = self.txn.as_ref() {
            // `begin` pinned the transaction's view, and that pin keeps the
            // GraphCell from being replaced for the transaction's lifetime,
            // so deriving the target here is safe without a fresh guard.
            let target = match self.dispatch_target(admin, namespace) {
                Ok(target) => target,
                Err(e) => return QueryOutcome::not_admitted(Err(e)),
            };
            let shared = Arc::clone(&self.inner);
            // `begin` already pinned the transaction's view. Only admission
            // must be serialized with poison publication, not its execution.
            let schema = match shared.read_guard() {
                Ok(_read) => target.schema_snapshot(),
                Err(e) => return QueryOutcome::not_admitted(Err(e)),
            };
            #[cfg(test)]
            shared.pause_admitted_read();
            (
                self.query_in_txn(
                    TxnRun {
                        txn,
                        target,
                        schema: &schema,
                        profile_metrics: ctx.profile_metrics,
                    },
                    logical,
                    params,
                    cypher,
                    token,
                ),
                true,
            )
        } else if ctx.stamp.write_route {
            // Only reachable when the statement targets the `neo4j` graph:
            // the other namespaces reject writes above, and administration
            // commands target `neo4j`. `write_route` (computed in
            // `plan_gate_classify` under the read guard) is `is_write` plus
            // the ontology write procedures, whose redo lands through the
            // same group-commit pipeline (ADR-0120 §3).
            // ADR-0209 batched-vs-single commit strategy selection lives in
            // commit.rs (`commit_write`).
            (
                commit_write(
                    &self.inner,
                    // The statement's execution facts, the same bundle
                    // `commit_optimistic` executes.
                    &ExecRequest {
                        plan: logical,
                        params,
                        query_text: cypher,
                        principal: self.principal.as_deref(),
                        batch_commit: None,
                        plan_metrics: ctx.profile_metrics,
                    },
                    parse_plan_ns,
                    token,
                ),
                false,
            )
        } else {
            // The target is selected and pinned under the read guard: the
            // `neo4j` arm derefs the GraphCell, whose contract requires the
            // guard (a follow reload may `replace` the graph otherwise,
            // ADR-0137); the pin then keeps the cell alive for `target`'s
            // later use. The (admin, namespace) dispatch decision is made
            // exactly once, here.
            let target: &Graph;
            // The ontology document the statement reads (ADR-0121 §3,
            // ADR-0227): store-level state captured together with the graph
            // pin under the read guard, so an auto-commit statement sees a
            // consistent admission-time document (explicit transactions use
            // the begin-time capture in `query_in_txn`). A shared handle:
            // the capture is an `Arc` pointer clone, not a document copy.
            let ontology_document: Option<Arc<str>>;
            let (pin, schema) = match self.inner.read_guard() {
                Ok(_read) => {
                    // The same target vocabulary every dispatch site
                    // reads (dispatch_target, ADR-0229 §1): the
                    // auto-commit read path routes through it too, its
                    // error mapped to the outcome form.
                    target = match self.dispatch_target(admin, namespace) {
                        Ok(target) => target,
                        Err(e) => return QueryOutcome::not_admitted(Err(e)),
                    };
                    ontology_document = self.inner.ontology.get();
                    match target.pin_now() {
                        Ok(pin) => (pin, target.schema_snapshot()),
                        Err(e) => return QueryOutcome::not_admitted(Err(e)),
                    }
                }
                Err(e) => return QueryOutcome::not_admitted(Err(e)),
            };
            #[cfg(test)]
            self.inner.pause_admitted_read();
            (
                execute_read_at(&ReadAdmission {
                    graph: target,
                    snapshot_ts: pin.ts(),
                    schema: &schema,
                    request: ExecRequest {
                        plan: logical,
                        params,
                        query_text: cypher,
                        principal: self.principal.as_deref(),
                        batch_commit: None,
                        plan_metrics: ctx.profile_metrics,
                    },
                    cancel: Some(token),
                    ontology_document: ontology_document.as_deref(),
                }),
                true,
            )
        };
        // ADR-0319: a failure here is execution-phase — the statement was
        // planned and dispatched — so it is staged as `Executed::Failed`
        // (a planned statement's output names derive, so the staging keeps
        // the result; the one path that cannot derive them reports the
        // statement's error before any result).
        let result = match result {
            Ok(r) => Ok(Executed::Rows(QueryResult::from_exec(
                r,
                ctx.stamp.namespace,
                ctx.stamp.qtype,
                ctx.started.elapsed(),
            ))),
            // `staged` derives the planned output names (the UNION
            // column-agreement check ran in `plan`), so a planned statement
            // always stages its failure with its columns.
            Err(error) => Ok(ExecutionFailure::staged(
                logical,
                ctx.stamp,
                ctx.started.elapsed(),
                error,
            )),
        };
        QueryOutcome { result, admitted }
    }

    /// The view a statement executes — and its notification derivation
    /// reads — against (ADR-0363 decision 5): the explicit transaction's
    /// write overlay when the target namespace is writable and a scratch is
    /// supplied, else the committed graph at its commit timestamp. One
    /// constructor for `query_in_txn` and [`Self::notification_view`], so
    /// the two cannot pick differently.
    pub(super) fn statement_view<'a>(
        target: &'a Graph,
        scratch: Option<&'a RefCell<WriteScratch>>,
    ) -> ExecGraph<'a> {
        match scratch {
            Some(scratch) if target.namespace().is_writable() => ExecGraph::write(target, scratch),
            _ => ExecGraph::read_at(target, target.commit_ts()),
        }
    }

    /// The view a statement's notification derivation reads (ADR-0363
    /// decision 5): the same [`Self::statement_view`] `query_in_txn`
    /// executes against, from the transaction's scratch when one is open.
    fn notification_view<'a>(&'a self, admin: bool, namespace: Namespace) -> Result<ExecGraph<'a>> {
        let target = self.dispatch_target(admin, namespace)?;
        Ok(Self::statement_view(
            target,
            self.txn.as_ref().map(|txn| &txn.scratch),
        ))
    }

    /// The graph a statement in an explicit transaction runs against
    /// (ADR-0116 §3/§10): administration commands always use the store's
    /// `neo4j` graph from any namespace (ADR-0116 §2); a `system` session
    /// reads the store's empty `system` graph. A `system_ontologies`
    /// statement never reaches here — the namespace gate in
    /// `plan_gate_classify` rejected every non-administration statement
    /// routed there (ADR-0229 §1) — so that arm fails closed instead of
    /// guessing a graph. The `neo4j` arm derefs the GraphCell, so call
    /// only with a read guard held or a live pin (ADR-0137). (The
    /// auto-commit read path selects its target inline in `dispatch`.)
    fn dispatch_target(&self, admin: bool, namespace: Namespace) -> Result<&Graph> {
        match (admin, namespace) {
            (true, _) | (false, Namespace::Neo4j) => Ok(&self.inner.graph),
            (false, Namespace::System) => Ok(&self.inner.system),
            (false, Namespace::SystemOntologies) => {
                Err(Error::internal("system_ontologies is not queryable"))
            }
        }
    }
}
