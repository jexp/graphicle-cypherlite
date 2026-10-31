//! The compile-time plan gates (ADR-0116 §3/§4, ADR-0121 §3, ADR-0227 §1,
//! ADR-0229 §1/§2, ADR-0378) and the query-type/write-route
//! classification, extracted from `connection.rs` to keep it under the
//! repository's file-size budget. `Connection::plan_gate_classify` is the
//! one entry point: it plans, enforces the preview gate, validates `CALL`s,
//! enforces the namespace and ontology gates, and classifies the statement
//! — all under one read guard, above the EXPLAIN early-return and the
//! dispatch split, so no path can skip a gate.

use std::collections::BTreeMap;

use cypherlite_core::{ce_messages, Namespace};
use cypherlite_executor::{is_ontology_store_op, is_ontology_write_procedure};
use cypherlite_ir::{Clause, Query};
use cypherlite_planner::{
    is_database_admin_command, is_write, plan_with_stats, query_type,
    unsupported_system_clause_names, LogicalPlan, QueryType,
};

use crate::connection::Connection;
use crate::procedures::{call_is_write, validate_calls};
use crate::{Error, Result, Value};

/// The metadata stamped onto every `QueryResult` at construction: the
/// namespace the statement ran in (ADR-0117 §7) and its plan-derived query
/// type (ADR-0162), plus the auto-commit write-route decision (ADR-0121:
/// statements calling a `writes`-flagged built-in such as the ontology
/// write procedures must reach the write path even when no data clause
/// classifies them as writes). All are resolved in `plan_gate_classify`
/// above the EXPLAIN/dispatch split and carried into `dispatch` as one
/// bundle.
#[derive(Clone, Copy)]
pub(crate) struct ResultStamp {
    pub(crate) namespace: Namespace,
    pub(crate) qtype: QueryType,
    pub(crate) write_route: bool,
}

/// Every procedure call name in a plan (ADR-0121, ADR-0227, ADR-0378),
/// collected in clause-walk order, plus the ontology-procedure subset of
/// that list. Full descent (`walk_plan_clauses`) so a call nested in a
/// `CALL { … }` body, a `WHEN`/`ELSE` branch, a `FOREACH` body, an
/// expression subquery or a `NEXT` stage is gated and routed like a
/// top-level one.
///
/// Every call-name *gate* on this path derives from this one collection:
/// the preview gate reads [`PlanCallNames::all`], the ontology gate reads
/// both `all` (its other-write predicate) and [`PlanCallNames::ontology`],
/// and the write-route predicate reads `ontology`. Compile-time `CALL`
/// validation keeps its own clause walk by design (it validates each call
/// site, not just names); no gate re-walks the plan for names.
struct PlanCallNames<'a> {
    /// Every call name, in walk order.
    all: Vec<&'a str>,
    /// The ontology-procedure subset of `all`, in walk order.
    ontology: Vec<&'a str>,
}

/// Collect a plan's call names for the gates (ADR-0121, ADR-0227,
/// ADR-0378): the one `CALL` walk on the gate path, with the ontology
/// subset derived here too. Which names are ontology procedures, which
/// preview feature gates a name and which of them write stays with the
/// executor's canonical predicates
/// ([`cypherlite_executor::is_ontology_procedure`],
/// [`cypherlite_executor::procedure_preview_feature`],
/// [`cypherlite_executor::is_ontology_write_procedure`]), so the facade and
/// the executor's `CALL` dispatch agree.
fn plan_call_names(logical: &LogicalPlan) -> PlanCallNames<'_> {
    let mut all = Vec::new();
    cypherlite_planner::walk_plan_clauses(logical, &mut |clause| {
        if let Clause::Call { name, .. } = clause {
            all.push(name.as_str());
        }
    });
    let ontology = all
        .iter()
        .copied()
        .filter(|name| cypherlite_executor::is_ontology_procedure(name))
        .collect();
    PlanCallNames { all, ontology }
}

impl Connection {
    /// Plan, preview gate, compile-time CALL validation, the namespace gate
    /// and the query-type classification, under one read guard: all of them
    /// read `graph.procedures` through the `GraphCell` deref, whose contract
    /// requires a read guard or a live pin (a follow reload may `replace`
    /// the graph otherwise, ADR-0137). The guard is dropped before
    /// dispatch, so admission semantics are unchanged. The preview gate
    /// precedes compile-time CALL validation and the namespace gates on
    /// every path (ADR-0378); CALL validation in turn precedes the
    /// namespace gates, so a doubly-invalid statement reports the procedure
    /// error. All run exactly once here, above the EXPLAIN early-return and
    /// the normal dispatch, so no future early-return path can skip them.
    pub(crate) fn plan_gate_classify(
        &self,
        query: Query,
        params: &BTreeMap<String, Value>,
    ) -> Result<(LogicalPlan, ResultStamp)> {
        let _read = self.inner.read_guard()?;
        let mut is_write_proc = |name: &str| call_is_write(&self.inner.graph, name);
        // ADR-0218: the planner stays pure; cardinalities come from this
        // store, read under the plan read guard. A `USE` retarget still
        // plans against the home store (ADR-0218 Consequences).
        // The one canonical construction (ADR-0218) every plan read site
        // builds — see GraphPatternStats::new.
        let stats = crate::stats::GraphPatternStats::new(&self.inner.graph, params);
        let logical = plan_with_stats(query, &mut is_write_proc, &stats)?;
        // The one call-name collection every gate below reads: each derives
        // its view from it instead of re-walking the plan for `CALL`s.
        let call_names = plan_call_names(&logical);
        // ADR-0378: the preview gate runs BEFORE compile-time CALL
        // validation and every later gate (see `preview_gate_error`), so a
        // disabled `db.ontology.*` call reports the authored preview error
        // even when it is also malformed or routed to `system_ontologies`.
        if let Some(e) = self.preview_gate_error(&call_names) {
            return Err(e);
        }
        validate_calls(&self.inner.graph, &logical, params)?;
        // ADR-0116 §3: `USE` routes the statement; the planner resolved
        // the target eagerly, so the resolved target picks the graph and
        // the namespace rules it runs under.
        // ADR-0116 §4: the gates are plan-based (CE raises 42N17/42N18 at
        // semantic-check time), so they apply to EXPLAIN too.
        let namespace = logical.use_target.unwrap_or(self.namespace);
        if let Some(e) = self.namespace_gate_error(&logical, namespace) {
            return Err(e);
        }
        // ADR-0121/ADR-0227: the ontology-procedure gates run after the §4
        // namespace gates — after ADR-0229 §1 every procedure routed to
        // `system_ontologies` fails 42N17 there, before the ontology
        // target text, which now fires only on `system`.
        if let Some(e) = self.ontology_gate_error(&logical, namespace, &call_names) {
            return Err(e);
        }
        // ADR-0162: the query type is plan-based and total, classified
        // under the same read guard, so EXPLAIN/PROFILE report the
        // wrapped statement's type without executing it.
        let qtype = query_type(&logical, &mut is_write_proc);
        // Auto-commit routing: the write path is also required by a
        // statement whose only write is an ontology write procedure
        // (set/drop/apply push redo; `is_write` alone classifies a
        // bare `CALL` as a read because its procedure predicate is
        // write-blind). Deliberately NOT `plan_calls_write_procedure` with
        // the full predicate: routing user-registered or other
        // writes-flagged procedures to the write path would change their
        // admission semantics for nothing — they push no redo. The names
        // are the ontology subset of the collected call names, filtered by
        // the executor's registry predicate, not a literal list.
        let write_route = is_write(&logical)
            || call_names
                .ontology
                .iter()
                .any(|name| is_ontology_write_procedure(name));
        Ok((
            logical,
            ResultStamp {
                namespace,
                qtype,
                write_route,
            },
        ))
    }

    /// The preview gate (ADR-0371, ADR-0378), enforced at compile time under
    /// the same read guard as the other gates. `calls` is [`plan_call_names`]'
    /// one collection: [`PlanCallNames::all`], in walk order, so the first
    /// call whose feature is disabled decides (later gated calls never
    /// outrank it) and no gate re-walks the plan for `CALL` clauses.
    /// `plan_gate_classify` calls this before `validate_calls` and the
    /// namespace/ontology gates, the ADR-0378 position: a disabled
    /// `db.ontology.*` call reports the authored preview error even when it
    /// is also malformed or routed to `system_ontologies`.
    fn preview_gate_error(&self, calls: &PlanCallNames<'_>) -> Option<Error> {
        let preview = self.inner.graph.preview();
        calls.all.iter().find_map(|name| {
            cypherlite_executor::procedure_preview_feature(name)
                .and_then(|feature| preview.require(feature).err())
        })
    }

    /// The ontology-procedure gates (ADR-0121 §3, ADR-0227 §1), enforced
    /// at compile time — the layer that sees the whole statement — under
    /// the same read guard as the other gates. `calls` is [`plan_call_names`]'
    /// one collection: [`PlanCallNames::ontology`] is its ontology subset,
    /// in walk order, and the other-write predicate below runs over
    /// [`PlanCallNames::all`], so neither check re-walks the plan.
    fn ontology_gate_error(
        &self,
        logical: &LogicalPlan,
        namespace: Namespace,
        calls: &PlanCallNames<'_>,
    ) -> Option<Error> {
        if calls.ontology.is_empty() {
            return None;
        }
        // ADR-0227 §1: each procedure acts on the session/USE-target
        // database, which must be `neo4j`. Routed to `system_ontologies`
        // every procedure already failed 42N17 in `namespace_gate_error`
        // (ADR-0229 §1); this arm is what every procedure hits on `system`
        // once the 42N17 standalone-call shape passes.
        if namespace != Namespace::Neo4j {
            return Some(ce_messages::ontology_neo4j_only());
        }
        let set_or_drop = calls
            .ontology
            .iter()
            .copied()
            .find(|name| is_ontology_store_op(name))?;
        // ADR-0121 §3: set/drop are store-level commits of their own —
        // implicit (auto-commit) transactions only.
        if self.txn.is_some() {
            return Some(ce_messages::ontology_implicit_transaction_only(set_or_drop));
        }
        // ADR-0121 §3: a statement calling set/drop may not write anything
        // else (Neo4j's 42N03 text). "Anything else" is a data/schema write
        // (`is_write`'s procedure predicate is write-blind, so the set/drop
        // call itself is not counted) or another write procedure — the
        // predicate below excludes the two ontology store ops themselves
        // but counts the in-overlay `apply` (it writes `neo4j`). It runs
        // per name over the collected list rather than a precomputed bool
        // list: `call_is_write` reads the store's live procedure registry.
        let other_write_procedure =
            |name: &str| !is_ontology_store_op(name) && call_is_write(&self.inner.graph, name);
        if is_write(logical) || calls.all.iter().copied().any(other_write_procedure) {
            return Some(ce_messages::cross_database_write());
        }
        None
    }

    /// ADR-0116 §4 namespace gates: on a `system` session only SHOW
    /// commands, supported administration commands and standalone procedure
    /// calls run (42N17); on a `system_ontologies` session every statement
    /// fails — reads included — with ADR-0229 §2's 42N17 text (the ADR-0116
    /// §4 42N18 write arm is superseded). Both checks are plan-based — CE
    /// raises them at semantic-check time — so `plan_gate_classify`
    /// enforces this gate once, above the EXPLAIN early-return and the
    /// dispatch split; database-administration commands are exempt
    /// (ADR-0116 §2, and the only thing that still runs while routed to
    /// `system_ontologies`, ADR-0229 §1). The exemption is computed here
    /// from the plan (`is_database_admin_command(logical)`, a pure
    /// function of the `LogicalPlan` the gate already takes).
    fn namespace_gate_error(&self, logical: &LogicalPlan, namespace: Namespace) -> Option<Error> {
        if is_database_admin_command(logical) {
            return None;
        }
        match namespace {
            Namespace::System => unsupported_system_clause_names(logical)
                .map(|clauses| ce_messages::unsupported_system_clauses(&clauses)),
            Namespace::SystemOntologies => Some(ce_messages::system_ontologies_not_queryable()),
            Namespace::Neo4j => None,
        }
    }
}
