//! The shared-execution fast path for import-invariant `CALL { … }` bodies
//! (LABS-198): when a body never *computes* from its imported variables
//! (they appear, if at all, only as same-name pass-through projection
//! items), every per-row execution returns the same rows, so one execution
//! on the first row's seed is merged against every outer row instead of
//! re-running the body per row.
//!
//! Row equivalence with `call_subquery_row` holds because the body's rows
//! carry no information from the seed beyond same-name pass-through
//! columns, whose values the merge takes from the outer row (the per-row
//! path's imported-name exclusion rule). The analysis lives in the
//! executor rather than the planner because the check set is not static:
//! for the naked `CALL { }` form it is the outer rows' runtime key set
//! (plus its homogeneity), known only at execution.

use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{Result, Value};
use cypherlite_ir::{expr_vars, Clause, Expr, Projection, Query};

use super::{
    empty_body_row, execute_query_seeded, import_seed, merge_body_export,
    query_with_persistent_imports, SubquerySpec,
};
use crate::ctx::QueryCtx;
use crate::eval::Row;
use crate::exec::scope::nested_seed_row;
use crate::overlay::ExecGraph;

/// What one shared execution of a `CALL { … }` body needs.
pub(super) struct SharedExecution {
    /// Outer names the body must not *compute* from (bare same-name
    /// pass-through projections excepted) for one shared execution to be
    /// row-equivalent; the merge also excludes these columns, mirroring the
    /// per-row path's imported-name exclusion. For the naked form this is
    /// every outer key — rebinding an outer name inside the body is a
    /// compile error (`VariableAlreadyBound`), so a body-exported column
    /// colliding with an outer name is always a same-name pass-through and
    /// keeping the outer row's own value matches the per-row merge.
    pub(super) check: HashSet<String>,
    /// The scope's imported names, threaded through the body's `WITH`s
    /// exactly like the per-row path's `query_with_persistent_imports`
    /// (empty for the naked form, as there). Without the threading a body
    /// that references an import after a `WITH` dropped it would read a
    /// silent null instead of the seeded binding.
    pub(super) imported: HashSet<String>,
    /// The single seed row: the first outer row's bindings.
    pub(super) seed: Row,
}

/// Builds the shared-execution plan for one CALL clause execution, or
/// `None` when the fast path cannot apply: empty input (nothing to share
/// over — the per-row loop is behavior-identical there), `Wildcard` scope
/// (per-row key sets may differ), or a naked body over heterogeneous outer
/// schemas (a pass-through name missing from some outer row would gain a
/// null export per row that the shared merge's exclusion would instead
/// keep absent). The seed comes from the same `import_seed` as the per-row
/// path, so a missing import is its internal error here too.
pub(super) fn shared_execution_plan(
    scope: &cypherlite_ir::CallScope,
    rows: &[Row],
) -> Result<Option<SharedExecution>> {
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    Ok(match scope {
        cypherlite_ir::CallScope::Vars(vars) if !vars.is_empty() => Some(SharedExecution {
            check: vars.iter().cloned().collect(),
            imported: vars.iter().cloned().collect(),
            seed: import_seed(vars, first)?,
        }),
        cypherlite_ir::CallScope::None => {
            if rows
                .iter()
                .any(|row| row.keys().collect::<HashSet<_>>() != first.keys().collect())
            {
                return Ok(None);
            }
            // The naked scope seeds the body from the outer row minus the
            // enclosing clause engine's internal columns, exactly like the
            // per-row path (`call_subquery_row`); the check set follows the
            // stripped seed, since an internal slot is never a binding the
            // body could compute from.
            let seed = nested_seed_row(first);
            let check: HashSet<String> = seed.keys().cloned().collect();
            (!check.is_empty()).then(|| SharedExecution {
                imported: HashSet::new(),
                seed,
                check,
            })
        }
        _ => None,
    })
}

/// An expression is body-safe when it references no checked variable. The
/// two whole-body expression policies (no nondeterministic calls, no
/// scope-bearing variants) run in the same one `walk_query` traversal in
/// [`import_invariant_body`], so the per-position checks here stay
/// variables-only.
fn body_safe_expr(expr: &Expr, check: &HashSet<String>) -> bool {
    let mut vars = HashSet::new();
    expr_vars(expr, &mut vars);
    vars.is_disjoint(check)
}

/// A projection item is import-safe when it is `*`, computes nothing from
/// the checked variables, or passes one through under its own name. A
/// renamed pass-through (`WITH i AS j`) would need per-row value patching
/// the shared path does not do, so it disqualifies the body.
fn body_safe_item(item: &Projection, check: &HashSet<String>) -> bool {
    match &item.expr {
        Expr::Star => true,
        Expr::Variable(name) if check.contains(name) => {
            item.alias.as_deref().is_none_or(|a| a == name)
        }
        expr => body_safe_expr(expr, check),
    }
}

/// A pattern is body-safe when no checked name is *mentioned* anywhere in
/// it (`path_pattern_mentions`, the canonical IR walker): a mention in any
/// expression is a computation from the outer row, and a mention as a
/// pattern variable is a *correlation* (`MATCH (p)` with `p` imported
/// matches that one node), never a fresh binding — rebinding an outer name
/// is a compile error everywhere else. Shape eligibility is the IR's own
/// fact: `needs_gpm` covers path modes, selectors, predicates and
/// quantified/elements-only forms, and `shortestPath` goes through its own
/// engine — those shapes keep the fast path off rather than exercise one
/// more shape.
fn body_safe_pattern(pattern: &cypherlite_ir::PathPattern, check: &HashSet<String>) -> bool {
    if pattern.needs_gpm() || pattern.is_shortest_path_function {
        return false;
    }
    let mut mentioned = HashSet::new();
    cypherlite_ir::path_pattern_mentions(pattern, &mut mentioned);
    mentioned.is_disjoint(check)
}

/// Shared projection-shape check for `WITH`/`RETURN`: every item is
/// import-safe and no ordering, positional window, or explicit grouping key
/// computes from the checked variables. (SKIP/LIMIT themselves are allowed:
/// an import-invariant body produces the same row stream per outer row, so
/// the same window is selected.)
fn body_safe_projection(
    items: &[Projection],
    order: &[cypherlite_ir::OrderItem],
    skip: &Option<Expr>,
    limit: &Option<Expr>,
    group_by: &Option<Vec<Expr>>,
    check: &HashSet<String>,
) -> bool {
    items.iter().all(|item| body_safe_item(item, check))
        && order.iter().all(|o| body_safe_expr(&o.expr, check))
        && skip.as_ref().is_none_or(|e| body_safe_expr(e, check))
        && limit.as_ref().is_none_or(|e| body_safe_expr(e, check))
        && group_by
            .as_ref()
            .is_none_or(|keys| keys.iter().all(|k| body_safe_expr(k, check)))
}

/// Per-clause shape eligibility: read-only, position-insensitive clauses
/// whose expressions compute nothing from the checked variables.
fn body_safe_clause(clause: &Clause, check: &HashSet<String>) -> bool {
    match clause {
        Clause::Match {
            patterns,
            where_clause,
            search,
            ..
        } => {
            search.is_none()
                && where_clause
                    .as_ref()
                    .is_none_or(|w| body_safe_expr(w, check))
                && patterns.iter().all(|p| body_safe_pattern(p, check))
        }
        Clause::Unwind { expression, .. } => body_safe_expr(expression, check),
        Clause::With {
            items,
            order,
            skip,
            limit,
            where_clause,
            group_by,
            ..
        } => {
            body_safe_projection(items, order, skip, limit, group_by, check)
                && where_clause
                    .as_ref()
                    .is_none_or(|w| body_safe_expr(w, check))
        }
        Clause::Return {
            items,
            order,
            skip,
            limit,
            group_by,
            ..
        } => body_safe_projection(items, order, skip, limit, group_by, check),
        _ => false,
    }
}

/// Whether a `CALL { … }` body computes nothing from the checked variables
/// (they appear, if at all, only as same-name pass-through projection
/// items), making its result rows identical for every outer row — the
/// precondition for sharing one execution across all of them. Restricted
/// to a single UNION-free part ending in `RETURN`; anything else keeps
/// per-row execution.
///
/// One exhaustive `walk_query` traversal applies all three policies (its
/// matches fail compilation when the IR grows a variant): per-clause shape
/// eligibility (`on_clause`), and two whole-body expression policies
/// (`on_expr`): no nondeterministic call anywhere — one canonical
/// classification, `eval::is_nondeterministic`, beside the dispatch table;
/// a shared execution samples clock/entropy once instead of per outer row
/// — and no scope-bearing expression variant (`EXISTS`/`COUNT`/`COLLECT`
/// subqueries, pattern predicates/comprehensions, shortestPath — the one
/// vocabulary is `Expr::is_scope_bearing` beside `Expr::children`), whose
/// interiors execute per row with their own scope semantics that sharing
/// does not analyze. Nested bodies cannot produce false verdicts from the
/// folded `on_clause`: a nested query exists only under a clause or
/// expression this same walk already refuses.
pub(super) fn import_invariant_body(query: &Query, check: &HashSet<String>) -> bool {
    if query.next.is_some() || query.parts.len() != 1 {
        return false;
    }
    if !matches!(query.parts[0].clauses.last(), Some(Clause::Return { .. })) {
        return false;
    }
    let mut clauses_clean = true;
    let mut exprs_clean = true;
    cypherlite_ir::walk_query(
        query,
        &mut |clause| clauses_clean &= body_safe_clause(clause, check),
        &mut |expr| {
            exprs_clean &= !matches!(expr, Expr::Function { name, .. } if crate::eval::is_nondeterministic(name));
            exprs_clean &= !expr.is_scope_bearing();
        },
    );
    clauses_clean && exprs_clean
}

/// The shared-execution fast path itself: thread the scope's persistent
/// imports exactly like the per-row path, execute once on the plan's seed,
/// then merge the single result against every outer row.
pub(super) fn exec_call_subquery_shared(
    graph: &ExecGraph<'_>,
    rows: Vec<Row>,
    spec: SubquerySpec<'_>,
    plan: SharedExecution,
    params: &BTreeMap<String, Value>,
    qctx: QueryCtx<'_>,
) -> Result<Vec<Row>> {
    let scoped_query = query_with_persistent_imports(spec.query, &plan.imported);
    let result = execute_query_seeded(graph, &scoped_query, params, vec![plan.seed], qctx)?;
    if result.rows.is_empty() {
        return Ok(rows
            .into_iter()
            .filter_map(|row| empty_body_row(row, spec.optional, spec.query, spec.scope))
            .collect());
    }
    let mut out = Vec::new();
    for row in &rows {
        for values in &result.rows {
            out.push(merge_body_export(row, &result.columns, values, &plan.check));
        }
    }
    Ok(out)
}
