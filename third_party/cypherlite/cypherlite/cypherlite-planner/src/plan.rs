//! Plan types and the `plan` entry point: consume a lowered `Query`,
//! validate it, and build the `LogicalPlan` the executor interprets
//! (ADR-0009), with MATCH rewrites applied after bind (ADR-0024).

use cypherlite_core::{Error, Result};
use cypherlite_ir::{Clause, Expr, Query};

use crate::binding::{Binding, Env};
use crate::names::{parts_output_names, stage_output_names};
use crate::rewrite;
use crate::stats::{NoPatternStats, PatternStats};
use crate::validate::{is_schema_clause, validate_part_with_env};

/// Logical plan is a pipeline of clauses (plus UNION).
///
/// Built by `plan`, which consumes the lowered `Query`: the plan owns the
/// clause vectors moved out of it (no deep clone), so the rewrite pass
/// mutates them in place.
#[derive(Debug, Clone)]
pub struct LogicalPlan {
    pub parts: Vec<Vec<Clause>>,
    pub union_all: Vec<bool>,
    pub next: Option<Box<LogicalPlan>>,
    /// CIP-150 `CYPHER <version>` preamble of the source statement, lowered
    /// from `Query::cypher_version` (`NEXT` stages inherit it in the
    /// front-end). Read by the executor for version-dependent behavior.
    pub cypher_version: Option<u32>,
    /// The statement's resolved `USE` target (ADR-0116 §3), lifted into the
    /// plan by [`plan`]: `Some(namespace)` when any `USE` clause is present
    /// (all `USE` clauses in one statement must agree), `None` when the
    /// statement carries none and the session namespace applies. Resolution
    /// failures (unknown name 42N00, composite-only selector 42N72, distinct
    /// targets 42NA5, unresolvable element id 42NA7) are `plan` errors.
    /// Always `None` on nested `NEXT`-stage plans; the top-level plan's
    /// resolution walks the whole statement.
    pub use_target: Option<cypherlite_core::Namespace>,
}

impl LogicalPlan {
    /// Every stage of this plan, top-level first: the plan itself, then each
    /// `NEXT` stage. The one place the stage-level recursion lives — the
    /// stage-level walk ([`plan_output_names`]) rides it; part-level walks
    /// ride [`Self::parts`].
    pub(crate) fn stages(&self) -> impl Iterator<Item = &LogicalPlan> {
        std::iter::successors(Some(self), |stage| stage.next.as_deref())
    }

    /// Every part of every stage, top-level first: the one place the
    /// stage→parts flatten lives. The part-level walks ([`walk_plan`],
    /// [`walk_plan_parts`]) and the namespaces classifiers
    /// ([`crate::namespaces::is_show_databases_with_next`]) ride it.
    pub(crate) fn parts(&self) -> impl Iterator<Item = &[Clause]> {
        self.stages()
            .flat_map(|stage| stage.parts.iter().map(Vec::as_slice))
    }
}

/// The output column names of a planned statement (ADR-0319): the names the
/// final stage's `RETURN`/`YIELD`/`SHOW` exports, through the same
/// output-name analysis semantic validation uses. The engine reports them
/// for a statement whose execution failed, so a surface can still announce
/// its columns. A planned statement already passed the UNION
/// column-agreement check, so the `Err` arm is that check's error; callers
/// that hold a `plan`-produced statement share the one expect policy
/// ([`plan_output_names_expect`]).
pub fn plan_output_names(plan: &LogicalPlan) -> Result<Vec<String>> {
    plan.stages().try_fold(Vec::new(), |names, stage| {
        stage_output_names(stage.parts.iter().map(Vec::as_slice), names)
    })
}

/// [`plan_output_names`] for a caller that holds a planned statement: the
/// `Err` arm is the UNION column-agreement check, which `plan` already ran,
/// so a planned statement's output names always derive. The one expect for
/// that invariant, shared with the facade's `ExecutionFailure::staged`
/// (ADR-0319, ADR-0332).
pub fn plan_output_names_expect(plan: &LogicalPlan) -> Vec<String> {
    plan_output_names(plan).expect("a planned statement's UNION columns agree")
}

/// The canonical way to visit every clause of a [`LogicalPlan`]: each
/// part's clause tree via [`cypherlite_ir::walk_clauses`] — UNION arms are
/// parts, and `CALL { … }` subqueries, `WHEN`/`ELSE` branches, `FOREACH`
/// bodies and expression subqueries are all descended into — recursing
/// into `NEXT` stages. Hand-rolling a `plan.parts` + `plan.next` recursion
/// instead of calling this is how coverage of a nesting position gets
/// dropped.
pub fn walk_plan_clauses<'a>(plan: &'a LogicalPlan, on_clause: &mut impl FnMut(&'a Clause)) {
    walk_plan(plan, on_clause, &mut |_| {});
}

/// [`walk_plan_clauses`]' full-descent walk, seeing every expression of
/// the plan as well as every clause. Rides [`LogicalPlan::parts`], the one
/// stage→parts flatten.
fn walk_plan<'a>(
    plan: &'a LogicalPlan,
    on_clause: &mut impl FnMut(&'a Clause),
    on_expr: &mut impl FnMut(&'a Expr),
) {
    for part in plan.parts() {
        cypherlite_ir::walk_clauses(part, on_clause, on_expr);
    }
}

/// The parameter names `plan` references (`$name`), each once, in the
/// order of first appearance in the full-descent walk ([`walk_plan_clauses`]'
/// coverage: UNION arms, `NEXT` stages, subqueries, branches, `FOREACH`
/// bodies and pattern payloads). The facade checks them against the
/// supplied parameters before dispatch (ADR-0308).
pub fn plan_parameter_names(plan: &LogicalPlan) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::new();
    walk_plan(plan, &mut |_| {}, &mut |expr| {
        if let Expr::Parameter(name) = expr {
            if !names.contains(&name.as_str()) {
                names.push(name);
            }
        }
    });
    names
}

/// The canonical way to visit a [`LogicalPlan`] PART BY PART: each part's
/// TOP-LEVEL clause slice (UNION arms are parts), recursing into `NEXT`
/// stages — with no descent into `CALL { … }` bodies, `WHEN`/`ELSE`
/// branches, `FOREACH` bodies or expression subqueries.
///
/// This is deliberately NOT [`walk_plan_clauses`]'s coverage: analyses that
/// reason per part over top-level clauses only (e.g. the `system`-namespace
/// 42N17 offender collection, where a read MATCH nested in an expression
/// subquery attached to a standalone CALL stays allowed) must use this walk
/// so the part-vs-`NEXT` recursion lives in exactly one place. If your
/// analysis needs the nested positions, use [`walk_plan_clauses`] instead.
pub fn walk_plan_parts<'a>(plan: &'a LogicalPlan, on_part: &mut impl FnMut(&'a [Clause])) {
    for part in plan.parts() {
        on_part(part);
    }
}

/// The canonical way to visit a not-yet-planned [`Query`] PART BY PART —
/// the `ir::Query` analog of [`walk_plan_parts`]: each part's TOP-LEVEL
/// clause slice (UNION arms are parts), recursing into `NEXT` stages —
/// with no descent into `CALL { … }` bodies, `WHEN`/`ELSE` branches,
/// `FOREACH` bodies or expression subqueries.
///
/// Analyses that reason per part over the pre-plan IR (the access fold in
/// [`crate::access`], the validate.rs write / IN TRANSACTIONS sweeps) must
/// use this walk so the part-vs-`NEXT` recursion lives in exactly one
/// place. Queries nested inside a clause (`WHEN`/`ELSE` branch bodies,
/// `CALL { … }`, `FOREACH`) are the CALLER's recursion: the walk yields
/// the clause slices, and the clause handler decides which nested
/// [`Query`]s to descend into (as [`crate::access`]'s `WHEN`/`ELSE`
/// handling does).
pub fn walk_query_parts<'a>(query: &'a Query, on_part: &mut impl FnMut(&'a [Clause])) {
    for part in &query.parts {
        on_part(&part.clauses);
    }
    if let Some(next) = query.next.as_deref() {
        walk_query_parts(next, on_part);
    }
}

/// Whether any clause of `plan` (via [`walk_plan_clauses`]) satisfies
/// `pred` — the one walk-and-accumulate boolean over plan clauses, so
/// classifiers stay one-line predicates instead of hand-rolling
/// `let mut found = false; … found |= …; found`.
pub fn plan_any_clause(plan: &LogicalPlan, pred: &mut impl FnMut(&Clause) -> bool) -> bool {
    let mut found = false;
    walk_plan_clauses(plan, &mut |clause| found |= pred(clause));
    found
}

/// The not-yet-planned [`Query`] analog of [`plan_any_clause`], riding
/// [`walk_query_parts`] (top-level clauses per part, `NEXT` stages; the
/// caller's predicate decides which nested queries to descend into).
pub fn query_any_clause(query: &Query, pred: &mut impl FnMut(&Clause) -> bool) -> bool {
    let mut found = false;
    walk_query_parts(query, &mut |part| found |= part.iter().any(&mut *pred));
    found
}

/// ADR-0210: whether one clause is a `CALL { … } IN TRANSACTIONS` —
/// the single spelling both in-transactions classifiers
/// ([`plan_contains_in_transactions`], `validate::query_contains_in_transactions`)
/// ride.
pub(crate) fn clause_is_in_transactions(clause: &Clause) -> bool {
    matches!(
        clause,
        Clause::CallSubquery {
            in_transactions: Some(_),
            ..
        }
    )
}

/// ADR-0210: whether the plan carries `CALL { … } IN TRANSACTIONS`
/// anywhere (top level or nested in a `CALL { }` body — the nested form
/// is rejected by [`crate::validate`], so in practice this is the
/// top-level batched subquery). The facade's auto-commit write path uses
/// this to attach the per-batch commit sink.
pub fn plan_contains_in_transactions(plan: &LogicalPlan) -> bool {
    plan_any_clause(plan, &mut clause_is_in_transactions)
}

/// Bind AST to a logical plan (ADR-0009), then rewrite MATCH (ADR-0024).
///
/// Takes ownership of the query: the clauses are moved into the plan, not
/// cloned.
/// `is_write_procedure` classifies `CALL` targets for the write rules
/// (IN TRANSACTIONS composition, writes in subquery expressions): the
/// procedure registry lives in the executor, so the facade injects the
/// canonical predicate rather than the planner hardcoding names.
pub fn plan(
    query: Query,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
) -> Result<LogicalPlan> {
    plan_with_stats(query, is_write_procedure, &NoPatternStats)
}

/// [`plan`] with cardinality hints for MATCH ordering (ADR-0218).
///
/// `stats` is consulted while rewriting each MATCH. Missing counts fall
/// back to the fixed endpoint ranks. `NoPatternStats` is what [`plan`]
/// passes, so callers that have no graph keep the rank-only order.
pub fn plan_with_stats(
    query: Query,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    stats: &dyn PatternStats,
) -> Result<LogicalPlan> {
    let mut plan = plan_with_env(query, Env::new(), is_write_procedure, stats)?;
    // ADR-0116 §3/§10: lift the statement's `USE` target into the plan.
    // Resolution walks the whole statement (UNION arms, CALL subqueries,
    // WHEN branches, expression subqueries, NEXT stages), so it runs once,
    // on the top-level plan.
    plan.use_target = crate::use_target::resolve_use_target(&plan)?;
    Ok(plan)
}

pub(crate) fn plan_with_env(
    query: Query,
    seed_env: Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    stats: &dyn PatternStats,
) -> Result<LogicalPlan> {
    if query.parts.is_empty() {
        return Err(Error::semantic("empty query"));
    }
    if !query.union_all.is_empty() {
        let first = query.union_all[0];
        if query.union_all.iter().any(|a| *a != first) {
            return Err(Error::syntax(
                "InvalidClauseComposition: mixing UNION and UNION ALL",
            ));
        }
        if query
            .parts
            .iter()
            .any(|p| p.clauses.iter().any(is_schema_clause))
        {
            return Err(Error::syntax(
                "InvalidClauseComposition: CREATE/DROP/SHOW INDEX cannot be used with UNION",
            ));
        }
    }
    for part in &query.parts {
        validate_part_with_env(part, seed_env.clone(), is_write_procedure)?;
        if query.next.is_some() {
            if let Some(Clause::Return { items, .. }) = part.clauses.last() {
                if items.iter().any(|item| {
                    item.alias.is_none() && !matches!(item.expr, Expr::Variable(_) | Expr::Star)
                }) {
                    return Err(Error::syntax(
                        "NoExpressionAlias: expression in RETURN followed by NEXT must use AS",
                    ));
                }
            }
        }
        if matches!(
            part.clauses.last(),
            Some(
                Clause::Match { .. }
                    | Clause::With { .. }
                    | Clause::Unwind { .. }
                    | Clause::Use { .. }
            )
        ) {
            return Err(Error::syntax(
                "InvalidClauseComposition: query must conclude with RETURN, FINISH, an update, or a unit subquery",
            ));
        }
    }
    // This stage's output columns — the UNION column-agreement check — are
    // computed for every stage, not only when a `NEXT` stage needs the seed
    // environment, so every `plan`-produced `LogicalPlan` satisfies the
    // invariant `plan_output_names_expect` asserts (ADR-0332). Computed
    // from the unrewritten parts, before moving the clauses into the plan.
    let output_names = parts_output_names(&query.parts, Vec::new())?;
    let next_env: Env = if query.next.is_some() {
        output_names
            .into_iter()
            .map(|name| (name, Binding::Value))
            .collect()
    } else {
        Env::new()
    };
    let Query {
        parts,
        union_all,
        next,
        cypher_version,
    } = query;
    let mut parts: Vec<Vec<Clause>> = parts.into_iter().map(|part| part.clauses).collect();
    for part in &mut parts {
        rewrite::rewrite_part(part, stats);
    }
    let next = match next {
        Some(next) => Some(Box::new(plan_with_env(
            *next,
            next_env,
            is_write_procedure,
            stats,
        )?)),
        None => None,
    };
    Ok(LogicalPlan {
        parts,
        union_all,
        next,
        cypher_version,
        use_target: None,
    })
}
