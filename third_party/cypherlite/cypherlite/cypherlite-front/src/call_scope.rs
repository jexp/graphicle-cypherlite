//! CIP-141 `CALL (...) { ... }` variable scope clause + CIP-218 `RETURN *`
//! exclusion, shared between `validate.rs` (compile-time Rule 1/4/5/8/9/10
//! checks) and `engine.rs` (execution: scoped row seeding, Rule 7 "always
//! referenceable" reinjection, Rule 10 star-exclusion filtering).
//!
//! Core model: a scope-clause `CALL (...) { ... }` body is walked clause by
//! clause, threading two sets alongside the ordinary projected columns:
//!
//! - `imported`: the fixed set of variable names the scope clause pulled in
//!   (from `CallScope::Vars`/`Wildcard`), constant for the whole body.
//! - `excluded`: the subset of `imported` that CIP-218 Rule 10 still hides
//!   from a `RETURN *`/`WITH *` star expansion -- initially all of
//!   `imported`, and removed from (i.e. "promoted" to star-visible) only
//!   when a `WITH` clause consists *solely* of bare pass-through references
//!   to already-excluded imported names (mirroring the CIP-218 doc's own
//!   `WITH a` example). A name bundled into a larger WITH item list
//!   alongside anything else (a computed expression, a fresh binding, or a
//!   `*`) is not promoted -- matching the observed (if not fully
//!   doc-justified) Neo4j behavior in the nested-scope-subquery case, where
//!   an inner CALL's imported variable resurfaces via a `WITH x, y, z`
//!   pass-through mixed with newly-computed names without tripping Rule 9.
//!
//! `imported` is also *always* layered back on top of the ordinary
//! projected scope after every clause (Rule 7: "globally referenced ...
//! cannot be delisted by a later WITH"), independent of `excluded`.

use std::collections::HashSet;

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{
    CallScope, CallSubqueryClause, Clause, Expr, PatternPath, ReturnItem, Statement,
};

use crate::ast::collect::{
    check_expr_vars_defined, clause_introduced_names, effective_name, pattern_vars,
    projected_names, search_score_var, union_chain,
};
use crate::validate::{compute_subquery_output_vars, duplicate_outer_variable};

/// Resolves a `CALL (...)` clause's scope against `outer_scope` (the set of
/// variables bound in the enclosing query immediately before this CALL),
/// enforcing Rule 1 (every explicit reference must already be in scope).
/// Returns `None` for a naked `CALL { ... }` (no new restrictions -- the
/// legacy importing-WITH behavior is unaffected by this module).
fn resolve_import_set(
    scope: &CallScope,
    outer_scope: &HashSet<String>,
) -> Result<Option<HashSet<String>>, EngineError> {
    match scope {
        CallScope::None => Ok(None),
        CallScope::Wildcard => Ok(Some(outer_scope.clone())),
        CallScope::Vars(names) => {
            let mut imported = HashSet::new();
            for n in names {
                if !outer_scope.contains(n) {
                    return Err(EngineError::semantic(
                        ErrorKind::UndefinedVariable,
                        format!("variable '{n}' not defined"),
                    ));
                }
                imported.insert(n.clone());
            }
            Ok(Some(imported))
        }
    }
}

/// Inserts a freshly-bound variable name (MATCH/MERGE pattern var, UNWIND
/// alias, CALL procedure yield) into `local_scope`, raising Rule 8's
/// shadowing error (when `strict`) if it collides with an imported name --
/// these binding forms can never be a legitimate pass-through the way a
/// `WITH` item can.
fn check_and_insert(
    name: &str,
    imported: &HashSet<String>,
    local_scope: &mut HashSet<String>,
    strict: bool,
) -> Result<(), EngineError> {
    if strict && imported.contains(name) {
        return Err(shadow_error(name));
    }
    local_scope.insert(name.to_string());
    Ok(())
}

fn shadow_error(name: &str) -> EngineError {
    EngineError::semantic(
        ErrorKind::VariableShadowing,
        format!(
            "the variable `{name}` is shadowing a variable with the same name from the outer scope and needs to be renamed"
        ),
    )
}

/// Like `check_and_insert`, but for a MATCH/MERGE/CREATE pattern-element
/// variable specifically: reusing a variable that's already bound (whether
/// imported or bound earlier in this same body) in a pattern position is a
/// *reference* -- constraining the pattern to that already-known node/
/// relationship, or attaching a new relationship to it -- never a fresh
/// declaration, so it's never Rule 8 shadowing (the same "pattern reuse
/// isn't shadowing" carve-out the planner applies to EXISTS/COUNT/COLLECT
/// bodies on the lowered IR). Only a name genuinely new to this pattern
/// goes through the real shadow check.
fn check_and_insert_pattern_var(
    name: &str,
    imported: &HashSet<String>,
    local_scope: &mut HashSet<String>,
    strict: bool,
) -> Result<(), EngineError> {
    if local_scope.contains(name) {
        return Ok(());
    }
    check_and_insert(name, imported, local_scope, strict)
}

/// The MATCH/CREATE/MERGE arms' one pattern-binding loop: every variable of
/// every path — the path variable first, then its elements' — is checked
/// against the imported set and inserted into the scope. MERGE passes its
/// single pattern as a one-element slice, so the three arms state the same
/// policy once.
fn bind_pattern_vars_checked(
    paths: &[PatternPath],
    imported: &HashSet<String>,
    local_scope: &mut HashSet<String>,
    strict: bool,
) -> Result<(), EngineError> {
    for name in pattern_vars(paths) {
        check_and_insert_pattern_var(name, imported, local_scope, strict)?;
    }
    Ok(())
}

/// Walks one branch (`clauses`; callers handle `.union` chains themselves)
/// of a scope-clause CALL subquery body. Invokes `sink` for every
/// `Clause::Return`/`Clause::Finish` encountered with `(clause_index,
/// exported_names, star_exclusion_set, has_star)`. When `strict`, also
/// enforces Rule 4/5 (undefined variable references), Rule 8 (shadowing),
/// and Rule 10's `NoVariablesInScope`.
pub(crate) fn walk_branch(
    clauses: &[Clause],
    imported: &HashSet<String>,
    strict: bool,
    sink: &mut dyn FnMut(usize, HashSet<String>, HashSet<String>, bool) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    walk_branch_seeded(clauses, imported, &HashSet::new(), strict, sink)
}

/// Like `walk_branch`, but seeds `local_scope` with `carry` in addition to
/// `imported` -- used by CIP-213's NEXT-chain handling, where a later
/// stage's clauses must see whatever an earlier stage in the *same* body
/// declared (its own bound/projected names), without those carried names
/// being treated as CIP-141 "imported from the outer scope" for Rule 8
/// shadow-checking or CIP-218 Rule 10 star-exclusion purposes -- only
/// genuine `imported` names get that treatment; `carry` is purely
/// reference-visibility.
fn walk_branch_seeded(
    clauses: &[Clause],
    imported: &HashSet<String>,
    carry: &HashSet<String>,
    strict: bool,
    sink: &mut dyn FnMut(usize, HashSet<String>, HashSet<String>, bool) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    let mut local_scope: HashSet<String> = imported.union(carry).cloned().collect();
    let mut excluded: HashSet<String> = imported.clone();

    for (idx, clause) in clauses.iter().enumerate() {
        match clause {
            Clause::Match(mc) => {
                bind_pattern_vars_checked(&mc.pattern, imported, &mut local_scope, strict)?;
                if let Some(sv) = search_score_var(mc) {
                    check_and_insert_pattern_var(sv, imported, &mut local_scope, strict)?;
                }
            }
            Clause::Merge(mc) => {
                bind_pattern_vars_checked(
                    std::slice::from_ref(&mc.pattern),
                    imported,
                    &mut local_scope,
                    strict,
                )?;
            }
            Clause::Create(cc) => {
                bind_pattern_vars_checked(&cc.pattern, imported, &mut local_scope, strict)?;
            }
            Clause::Unwind(uw) => {
                if strict {
                    check_expr_vars_defined(&uw.expr, &local_scope)?;
                }
                check_and_insert(&uw.alias, imported, &mut local_scope, strict)?;
            }
            Clause::CallProcedure(cp) => {
                // The effective post-alias yield name comes from the one
                // YieldList model.
                for name in cp.yields.effective_names() {
                    check_and_insert(name, imported, &mut local_scope, strict)?;
                }
            }
            Clause::Use(uc) => {
                if strict {
                    check_expr_vars_defined(&uc.graph_expr, &local_scope)?;
                }
            }
            // A SHOW clause is a fresh row source (like MATCH with no prior
            // clauses), not a variable *reference* -- it introduces its
            // YIELD'd (or, bare/`YIELD *`, its full catalog's) column names
            // into scope for a later clause in this same scope-clause body
            // to reference, e.g. `CALL () { SHOW FUNCTIONS YIELD name ...
            // RETURN collect(name) }`.
            // The row-producing catalog commands (SHOW … and
            // TERMINATE TRANSACTIONS), through the one catalog
            // classification: each introduces its effective output
            // columns into scope exactly like the enumerated arms did —
            // never a second clause-by-clause spelling here.
            c if crate::show_columns::catalog_produces_rows(c) => {
                for v in crate::show_columns::catalog_output_columns(c) {
                    check_and_insert(&v, imported, &mut local_scope, strict)?;
                }
            }
            Clause::With(wc) => {
                if strict {
                    for item in &wc.items {
                        if !item.all {
                            check_expr_vars_defined(&item.expr, &local_scope)?;
                        }
                    }
                    if let Some(w) = &wc.where_expr {
                        check_expr_vars_defined(w, &local_scope)?;
                    }
                }

                let non_star: Vec<&ReturnItem> = wc.items.iter().filter(|i| !i.all).collect();

                // Rule 8: a non-pass-through item whose output name equals
                // an imported name is a forbidden redeclaration.
                for item in &non_star {
                    if let Some(name) = effective_name(item) {
                        if imported.contains(&name) {
                            // Pass-through: a bare `WITH a` or a redundant
                            // self-alias `WITH a AS a` of an imported name
                            // is not a redeclaration.
                            let is_pass_through = matches!(&item.expr, Expr::Variable(v) if v == &name)
                                && item.alias.as_deref().is_none_or(|a| a == name);
                            if !is_pass_through && strict {
                                return Err(shadow_error(&name));
                            }
                        }
                    }
                }

                // Rule 10 promotion: only when this WITH consists *solely*
                // of bare pass-throughs of currently-excluded names (see
                // module doc) do those names become star-visible.
                if !non_star.is_empty()
                    && non_star.iter().all(|item| {
                        item.alias.is_none()
                            && matches!(&item.expr, Expr::Variable(v) if excluded.contains(v))
                    })
                {
                    for item in &non_star {
                        if let Expr::Variable(v) = &item.expr {
                            excluded.remove(v);
                        }
                    }
                }

                let mut new_scope = HashSet::new();
                for item in &wc.items {
                    if item.all {
                        new_scope.extend(local_scope.iter().cloned());
                    } else if let Some(name) = effective_name(item) {
                        new_scope.insert(name);
                    }
                }
                new_scope.extend(imported.iter().cloned()); // Rule 7 stickiness
                local_scope = new_scope;
            }
            Clause::CallSubquery(inner) => {
                let inner_exports = resolve_nested_call(inner, &local_scope, strict)?;
                local_scope.extend(inner_exports);
            }
            Clause::Return(rc) => {
                let mut exported = HashSet::new();
                let mut has_star = false;
                for item in &rc.items {
                    if item.all {
                        has_star = true;
                        for n in &local_scope {
                            if !excluded.contains(n) {
                                exported.insert(n.clone());
                            }
                        }
                    } else if let Some(name) = effective_name(item) {
                        if strict {
                            check_expr_vars_defined(&item.expr, &local_scope)?;
                        }
                        exported.insert(name);
                    }
                }
                if strict && has_star && exported.is_empty() {
                    return Err(EngineError::semantic(
                        ErrorKind::NoVariablesInScope,
                        "RETURN * is not allowed when there are no variables in scope",
                    ));
                }
                sink(idx, exported, excluded.clone(), has_star)?;
            }
            Clause::Finish => {
                sink(idx, HashSet::new(), HashSet::new(), false)?;
            }
            // CIP-135: a `WHEN/ELSE` conditional clause is always the sole
            // element of the clause list it appears in (see
            // `ConditionalClause`'s doc comment), so `local_scope ==
            // imported` here -- no earlier clause in this body could have
            // bound anything. Each branch body (and the ELSE body, plus
            // each one's own UNION chain) is walked as an independent
            // scope-clause body sharing the same `imported` set, and every
            // `Return`/`Finish` it finds is forwarded to this clause's own
            // `sink` under this Conditional clause's `idx` -- Rule 9/10
            // (enforced on the lowered IR by the planner's
            // `validate_conditional_returns`) guarantees every branch
            // reports the same exported names, so the resulting
            // union-of-exports is exactly that one shared set.
            Clause::Conditional(cc) => {
                if strict {
                    for branch in &cc.branches {
                        check_expr_vars_defined(&branch.predicate, &local_scope)?;
                    }
                }
                let mut bodies: Vec<&Statement> = cc.branches.iter().map(|b| &b.body).collect();
                if let Some(else_body) = &cc.else_body {
                    bodies.push(else_body);
                }
                for body in bodies {
                    let mut cur = Some(body);
                    while let Some(s) = cur {
                        walk_branch(
                            &s.clauses,
                            imported,
                            strict,
                            &mut |_, exp, excl, has_star| sink(idx, exp, excl, has_star),
                        )?;
                        cur = s.union.as_deref().map(|u| &u.statement);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Resolves and validates a (possibly nested) `CALL (...) { ... }` clause
/// found *inside* a scope-clause body, returning the set of names it
/// exports back into the enclosing body's local scope. For a naked
/// (`CallScope::None`) nested call, falls back to the pre-existing lenient
/// `compute_subquery_output_vars` (this module's new Rule 1/8/9/10 checks
/// only apply to scope-clause CALLs).
fn resolve_nested_call(
    csq: &CallSubqueryClause,
    outer_scope: &HashSet<String>,
    strict: bool,
) -> Result<HashSet<String>, EngineError> {
    match resolve_import_set(&csq.scope, outer_scope) {
        Err(e) => {
            if strict {
                Err(e)
            } else {
                Ok(HashSet::new())
            }
        }
        Ok(None) => Ok(compute_subquery_output_vars(&csq.statement).unwrap_or_default()),
        Ok(Some(imported)) => {
            branch_exports_with_union(&csq.statement, &imported, outer_scope, strict)
        }
    }
}

/// Computes the union of every branch's (this statement plus its `UNION`
/// chain) exported names for a scope-clause body, enforcing Rule 9 (no
/// exported name may collide with `outer_scope`) when `strict`.
///
/// CIP-213: each branch may itself be a NEXT chain (`s.next`); every stage
/// is still walked (so Rule 4/5/8/10 are checked throughout, and Rule 3 --
/// imported names visible in every statement of the chain -- holds via
/// `carry` always including `imported`), but only the *final* stage's own
/// Return/Finish is a real export: an earlier stage's projection is
/// consumed by the following NEXT stage, not exposed to the enclosing
/// scope, mirroring `Engine::execute_call_scope_stage`'s identical runtime
/// handoff.
fn branch_exports_with_union(
    stmt: &Statement,
    imported: &HashSet<String>,
    outer_scope: &HashSet<String>,
    strict: bool,
) -> Result<HashSet<String>, EngineError> {
    let mut exported = HashSet::new();
    for s in union_chain(stmt) {
        let mut stage = s;
        let mut carry: HashSet<String> = HashSet::new();
        loop {
            let is_final_stage = stage.next.is_none();
            let mut stage_export: HashSet<String> = HashSet::new();
            let mut stage_returned = false;
            walk_branch_seeded(
                &stage.clauses,
                imported,
                &carry,
                strict,
                &mut |_, exp, _, _| {
                    stage_export = exp;
                    stage_returned = true;
                    Ok(())
                },
            )?;
            if is_final_stage {
                if strict {
                    for n in &stage_export {
                        if outer_scope.contains(n) {
                            return Err(duplicate_outer_variable(n));
                        }
                    }
                }
                exported.extend(stage_export);
            } else {
                carry = if stage_returned {
                    stage_export
                } else {
                    HashSet::new()
                };
                carry.extend(imported.iter().cloned());
            }
            match &stage.next {
                Some(next_stmt) => stage = next_stmt,
                None => break,
            }
        }
    }
    Ok(exported)
}

/// Strict, top-level entry point (CIP-141/CIP-218 Rules 1/4/5/8/9/10):
/// recursively validates every `CALL (...) { ... }` clause anywhere in
/// `stmt` (including inside naked `CALL { ... }` bodies and UNION chains,
/// at arbitrary nesting depth).
pub fn validate_call_scope_clauses(stmt: &Statement) -> Result<(), EngineError> {
    walk_union_chain(stmt, HashSet::new())
}

/// Walk one statement plus its whole `UNION` chain at top level: the one
/// spelling of the chain cursor the naked-CALL arm and both Conditional
/// bodies consult.
fn walk_union_chain(stmt: &Statement, base_scope: HashSet<String>) -> Result<(), EngineError> {
    for s in union_chain(stmt) {
        walk_top_level(s, base_scope.clone())?;
    }
    Ok(())
}

/// Walk one statement's own clauses (no `UNION` chain — [`walk_union_chain`]
/// owns the cursor over the chain, so no statement is walked twice).
fn walk_top_level(stmt: &Statement, base_scope: HashSet<String>) -> Result<(), EngineError> {
    let mut scope = base_scope.clone();
    for clause in &stmt.clauses {
        match clause {
            Clause::CallSubquery(csq) => {
                match &csq.scope {
                    CallScope::None => {
                        // Naked call: no new *whole-row visibility* restrictions
                        // from this module (the body still sees the outer row),
                        // but still recurse to catch a nested scope-clause CALL
                        // inside its body.
                        walk_union_chain(&csq.statement, scope.clone())?;
                        // 3.3(f): a naked `CALL { ... }` whose terminal
                        // projection *exports* a name already bound in the outer
                        // scope collides when its row is joined back into the
                        // outer row -- a compile-time error, verbatim from live
                        // Neo4j Cypher 25: `WITH 5 AS x CALL { RETURN 1 AS x }`
                        // raises "Variable `x` already declared in outer scope"
                        // (SyntaxError / VariableAlreadyBound). Only the output
                        // collision is rejected; whole-row visibility of the
                        // outer scope inside the body is unchanged.
                        let exports = call_subquery_exports(csq, &scope);
                        let mut collisions: Vec<&String> =
                            exports.iter().filter(|n| scope.contains(*n)).collect();
                        collisions.sort();
                        if let Some(name) = collisions.first() {
                            return Err(duplicate_outer_variable(name));
                        }
                        // Also make its exported variables visible to
                        // later clauses (e.g. a subsequent scope-clause
                        // `CALL (x) { ... }` importing a name this naked
                        // call produced), matching the scoped arm below.
                        scope.extend(exports);
                    }
                    _ => {
                        let exports = resolve_nested_call(csq, &scope, true)?;
                        scope.extend(exports);
                    }
                }
            }
            // CIP-135: recurse into every branch/ELSE body (and their own
            // UNION chains) so a nested scope-clause `CALL (...) { ... }`
            // inside a `WHEN`/`ELSE` body still gets Rule 1/4/5/8/9/10
            // checked. `Clause::Conditional` is always the sole clause of
            // whatever list it's in, so there is no "after" scope to
            // thread back out from it.
            Clause::Conditional(cc) => {
                for branch in &cc.branches {
                    walk_union_chain(&branch.body, scope.clone())?;
                }
                if let Some(else_body) = &cc.else_body {
                    walk_union_chain(else_body, scope.clone())?;
                }
            }
            // A WITH replaces the scope (a projection), so it cannot ride
            // the introduction-only fold: names it drops must stop being
            // referenceable by a later clause (and importable by a later
            // scope-clause CALL).
            Clause::With(wc) => {
                scope = projected_names(&wc.items, &scope);
            }
            // Every other clause's introduced names come from the one
            // accumulation policy (`validate::clause_introduced_names`):
            // MATCH/MERGE/CREATE patterns, UNWIND and LOAD CSV aliases,
            // CALL … YIELD columns and the catalog commands' output
            // columns — no second clause-by-clause vocabulary here.
            other => scope.extend(clause_introduced_names(other)),
        }
    }
    Ok(())
}

/// Lenient (never errors) computation of the variable names a `CALL
/// (...) { ... }` clause exports into the outer row, given the outer scope
/// at that point. Used by scope-accumulation passes elsewhere in
/// `validate.rs` that need to know what a CALL clause makes available
/// afterward; any Rule violation is caught separately by
/// `validate_call_scope_clauses`, which always runs first.
pub(crate) fn call_subquery_exports(
    csq: &CallSubqueryClause,
    outer_scope: &HashSet<String>,
) -> HashSet<String> {
    match &csq.scope {
        // CIP-213: `compute_subquery_output_vars` already walks
        // MATCH/WITH/UNWIND/RETURN/FINISH/CREATE/MERGE/CallSubquery/
        // Conditional *and* recurses through a `.next` chain to its true
        // final stage, so it subsumes the old direct "scan for a
        // Return-or-Conditional clause" loop this used to do -- which
        // never looked past a naked CALL body's *first* stage, silently
        // reporting nothing exported by `CALL { RETURN 1 AS x NEXT RETURN
        // x + 1 AS y }` (it would have found no top-level `Return`/
        // `Conditional` clause at all, since the real one is one NEXT
        // stage deeper).
        CallScope::None => compute_subquery_output_vars(&csq.statement).unwrap_or_default(),
        _ => resolve_nested_call(csq, outer_scope, false).unwrap_or_default(),
    }
}
