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
    CallScope, CallSubqueryClause, Clause, ConditionalClause, Expr, PatternPath, ReturnClause,
    ReturnItem, Statement, WithClause,
};

use crate::ast::collect::{
    check_expr_vars_defined, clause_introduced_names, effective_name, pattern_vars,
    projected_names, search_score_var, union_chain,
};
// The lenient naked-CALL fallback consults the output-vars family at
// the crate root (its other consumer is `validate::scope`, importing
// downward) — no upward edge into `validate` remains.
use crate::output_vars::compute_subquery_output_vars;

/// The one construction site for an outer-scope collision: a subquery
/// output (or CALL export) re-declares a name the enclosing scope already
/// binds. Neo4j's verbatim text names the offender (CIP-218 interactions
/// [1]-[3]); every outer-scope guard routes through here so the diagnostic
/// cannot drift into unnamed variants. Lives at the crate root (its
/// consumers are this module and `validate::scope`, which imports
/// downward from here).
pub(crate) fn duplicate_outer_variable(name: &str) -> EngineError {
    EngineError::semantic(
        ErrorKind::VariableAlreadyBound,
        format!("Variable `{name}` already declared in outer scope"),
    )
}

/// The colliding export reported when a Rule 9 / 3.3(f) check fires: the
/// lexicographically smallest colliding name, not whichever a hash
/// iteration yields first — the rejection set is unchanged, the error
/// text is run-to-run deterministic.
fn smallest_collision<'a>(
    exports: &'a HashSet<String>,
    outer: &HashSet<String>,
) -> Option<&'a String> {
    exports.iter().filter(|n| outer.contains(*n)).min()
}

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

/// The branch-seeding policy, the one spelling: a branch body's initial
/// scope is `imported ∪ carry` — the names an earlier NEXT stage declared
/// ride along as reference-visibility on top of the imported set (see
/// [`walk_branch_seeded`]'s doc for the CIP-213 semantics of `carry`).
fn branch_seed(imported: &HashSet<String>, carry: &HashSet<String>) -> HashSet<String> {
    imported.union(carry).cloned().collect()
}

/// Walks one branch (`clauses`; callers handle `.union` chains themselves)
/// of a scope-clause CALL subquery body, seeding `local_scope` from
/// `imported` (plus `carry` when the caller has one). Invokes `sink` for
/// every `Clause::Return`/`Clause::Finish` encountered with the exported
/// name set that clause produces. When `strict`, also enforces Rule 4/5
/// (undefined variable references), Rule 8 (shadowing), and Rule 10's
/// `NoVariablesInScope`.
///
/// `carry` seeds extra reference-visible names beyond `imported` -- used
/// by CIP-213's NEXT-chain handling, where a later stage's clauses must
/// see whatever an earlier stage in the *same* body declared (its own
/// bound/projected names), without those carried names being treated as
/// CIP-141 "imported from the outer scope" for Rule 8 shadow-checking or
/// CIP-218 Rule 10 star-exclusion purposes -- only genuine `imported`
/// names get that treatment; `carry` is purely reference-visibility.
fn walk_branch_seeded(
    clauses: &[Clause],
    imported: &HashSet<String>,
    carry: &HashSet<String>,
    strict: bool,
    sink: &mut dyn FnMut(HashSet<String>) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    let mut local_scope: HashSet<String> = branch_seed(imported, carry);
    let mut excluded: HashSet<String> = imported.clone();

    for clause in clauses.iter() {
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
            // The row-producing catalog commands (e.g. a SHOW clause, a
            // fresh row source like MATCH with no prior clauses) put
            // their effective output columns — YIELD'd, or the full
            // catalog's under a bare `YIELD *` — into scope for a later
            // clause in this same scope-clause body to reference, e.g.
            // `CALL () { SHOW FUNCTIONS YIELD name ...
            // RETURN collect(name) }`: one guard, the same catalog
            // classification the other scope walkers use.
            c if crate::show_columns::catalog_produces_rows(c) => {
                for v in crate::show_columns::catalog_output_columns(c) {
                    check_and_insert(&v, imported, &mut local_scope, strict)?;
                }
            }
            Clause::With(wc) => {
                apply_with_to_scope(wc, imported, &mut excluded, &mut local_scope, strict)?;
            }
            Clause::CallSubquery(inner) => {
                let inner_exports = resolve_nested_call(inner, &local_scope, strict)?;
                local_scope.extend(inner_exports);
            }
            Clause::Return(rc) => {
                let exported = return_exports(rc, &local_scope, &excluded, strict)?;
                sink(exported)?;
            }
            Clause::Finish => {
                sink(HashSet::new())?;
            }
            // CIP-135: [`walk_conditional_bodies`] owns the arm's policy
            // (including the sole-element invariant the grammar gives it).
            Clause::Conditional(cc) => {
                walk_conditional_bodies(cc, imported, carry, strict, &mut *sink)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// The CIP-135 Conditional arm's policy, one sibling helper beside
/// [`apply_with_to_scope`]/[`return_exports`] so the walker stays a
/// clause dispatch: a `WHEN/ELSE` clause is always the sole element of
/// the clause list it appears in (see `ConditionalClause`'s doc
/// comment), so nothing beyond the seeded `imported ∪ carry` could have
/// been bound where it stands — the seed is derived here, once, and the
/// predicates are checked against exactly the visibility each branch
/// body is then walked with (`imported` for the Rule 8/10 policy sets,
/// `carry` for reference-visibility). Rule 9/10 (enforced on the
/// lowered IR by the planner's `validate_conditional_returns`)
/// guarantees every branch reports the same exported names, so the
/// resulting union-of-exports is exactly that one shared set.
fn walk_conditional_bodies(
    cc: &ConditionalClause,
    imported: &HashSet<String>,
    carry: &HashSet<String>,
    strict: bool,
    sink: &mut dyn FnMut(HashSet<String>) -> Result<(), EngineError>,
) -> Result<(), EngineError> {
    let seed = branch_seed(imported, carry);
    if strict {
        for branch in &cc.branches {
            check_expr_vars_defined(&branch.predicate, &seed)?;
        }
    }
    // Every branch body, plus the ELSE body, each with its own UNION chain.
    let bodies = cc
        .branches
        .iter()
        .map(|b| &b.body)
        .chain(cc.else_body.iter());
    for body in bodies {
        for s in union_chain(body) {
            walk_branch_seeded(&s.clauses, imported, carry, strict, &mut *sink)?;
        }
    }
    Ok(())
}

/// The alias policy a pass-through check runs under: Rule 10's star
/// promotion accepts bare pass-throughs only, Rule 8's redeclaration
/// check also accepts the redundant self-alias — the module's subtlest
/// invariant, named at the type level so each rule's call site states
/// its own policy.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AliasPolicy {
    /// `WITH a` — no alias.
    BareOnly,
    /// `WITH a` or the redundant self-alias `WITH a AS a`.
    AllowSelfAlias,
}

/// One WITH item's pass-through variable under `policy`: the item's
/// variable under its own output name when the item is a pass-through
/// the policy accepts, `None` otherwise.
fn pass_through_var(item: &ReturnItem, policy: AliasPolicy) -> Option<&str> {
    let Expr::Variable(v) = &item.expr else {
        return None;
    };
    match item.alias.as_deref() {
        None => Some(v),
        Some(a) if policy == AliasPolicy::AllowSelfAlias && a == v.as_str() => Some(v),
        _ => None,
    }
}

/// The RETURN arm's export computation, one sibling helper beside
/// [`apply_with_to_scope`] so the walker stays a clause dispatch: the
/// exported names (star expansion minus the still-excluded names, plus
/// every item's effective name) — the strict star-with-empty-scope
/// rejection included, so the arm is the helper call plus the sink.
fn return_exports(
    rc: &ReturnClause,
    local_scope: &HashSet<String>,
    excluded: &HashSet<String>,
    strict: bool,
) -> Result<HashSet<String>, EngineError> {
    if strict {
        check_non_star_items_defined(&rc.items, local_scope)?;
    }
    // The star-visible bound is the incoming scope minus the still-
    // excluded names; the exported set is then the canonical
    // [`projected_names`] projection over that bound (a `*` passes the
    // bound through, every other item contributes its effective name).
    // The bound matters only for a `*` item, so a star-less RETURN
    // passes an empty bound and skips the filtered clone.
    let has_star = rc.items.iter().any(|i| i.all);
    let bound = if has_star {
        local_scope
            .iter()
            .filter(|n| !excluded.contains(*n))
            .cloned()
            .collect()
    } else {
        HashSet::new()
    };
    let exported = projected_names(&rc.items, &bound);
    if strict && has_star && exported.is_empty() {
        return Err(EngineError::semantic(
            ErrorKind::NoVariablesInScope,
            "RETURN * is not allowed when there are no variables in scope",
        ));
    }
    Ok(exported)
}

/// Rule 1's strict pre-check over a projection item list, the one
/// spelling both projection arms share: every non-star item's
/// expression must reference only visible names.
fn check_non_star_items_defined(
    items: &[ReturnItem],
    scope: &HashSet<String>,
) -> Result<(), EngineError> {
    for item in items {
        if !item.all {
            check_expr_vars_defined(&item.expr, scope)?;
        }
    }
    Ok(())
}

/// The WITH arm's four named policies, one sibling helper so the
/// walker stays a clause dispatch (LABS-518): the strict pre-check
/// (Rule 1 — every referenced name visible), the Rule 8
/// imported-name-redeclaration check, the Rule 10 star-exclusion
/// promotion, and the Rule 7 scope rebuild over [`projected_names`].
fn apply_with_to_scope(
    wc: &WithClause,
    imported: &HashSet<String>,
    excluded: &mut HashSet<String>,
    local_scope: &mut HashSet<String>,
    strict: bool,
) -> Result<(), EngineError> {
    // Rule 1 (strict pre-check) and Rule 8 (the redeclaration check)
    // share one gate: both are strict-only policies, spelled one way.
    if strict {
        check_non_star_items_defined(&wc.items, local_scope)?;
        if let Some(w) = &wc.where_expr {
            check_expr_vars_defined(w, local_scope)?;
        }

        // Rule 8: a non-pass-through item whose output name equals
        // an imported name is a forbidden redeclaration — a bare
        // `WITH a` or a redundant self-alias `WITH a AS a` of an
        // imported name is not one.
        for item in wc.items.iter().filter(|i| !i.all) {
            if let Some(name) = effective_name(item) {
                if imported.contains(&name)
                    && pass_through_var(item, AliasPolicy::AllowSelfAlias).is_none()
                {
                    return Err(shadow_error(&name));
                }
            }
        }
    }

    // Rule 10 promotion: only when this WITH consists *solely*
    // of bare pass-throughs of currently-excluded names (see
    // module doc) do those names become star-visible. The
    // classification is collected once — every item a bare
    // pass-through (the length equality subsumes the star scan: a
    // `*` item classifies to None) and every one currently
    // excluded — so bundling with a `*` or any other item kind
    // disqualifies promotion exactly as computed expressions and
    // fresh bindings do.
    let promotions: Vec<&str> = wc
        .items
        .iter()
        .filter_map(|i| pass_through_var(i, AliasPolicy::BareOnly))
        .collect();
    if promotions.len() == wc.items.len() && promotions.iter().all(|v| excluded.contains(*v)) {
        for v in promotions {
            excluded.remove(v);
        }
    }

    // The star projection is [`projected_names`] (a `*` item
    // passes the incoming local scope through, every other
    // item contributes its effective name) plus the Rule 7
    // stickiness: the imported names stay referenceable.
    let mut new_scope = projected_names(&wc.items, local_scope);
    new_scope.extend(imported.iter().cloned()); // Rule 7 stickiness
    *local_scope = new_scope;
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
/// CIP-213: each branch may itself be a NEXT chain (`s.next`) —
/// [`stage_exports`] walks it — mirroring
/// `Engine::execute_call_scope_stage`'s identical runtime handoff.
fn branch_exports_with_union(
    stmt: &Statement,
    imported: &HashSet<String>,
    outer_scope: &HashSet<String>,
    strict: bool,
) -> Result<HashSet<String>, EngineError> {
    let mut exported = HashSet::new();
    for s in union_chain(stmt) {
        exported.extend(stage_exports(
            s,
            imported,
            &HashSet::new(),
            strict,
            outer_scope,
        )?);
    }
    Ok(exported)
}

/// One NEXT chain's export, stage by stage: walk this stage (so Rule
/// 4/5/8/10 are checked throughout), then either hand its projection to
/// the following stage as `carry` — an earlier stage's export is consumed
/// by the next stage, not exposed to the enclosing scope — or, for the
/// final stage, check Rule 9 and return its exports. Rule 3 (imported
/// names visible in every statement of the chain) holds via
/// [`walk_branch_seeded`] seeding `local_scope` from
/// `imported.union(carry)` — the one owner of that invariant.
fn stage_exports(
    stage: &Statement,
    imported: &HashSet<String>,
    carry: &HashSet<String>,
    strict: bool,
    outer_scope: &HashSet<String>,
) -> Result<HashSet<String>, EngineError> {
    // The sink collects the stage's export set; a stage whose walk never
    // fires a Return/Finish leaves it empty, which is already the
    // intended carry for the next stage.
    let mut stage_export: HashSet<String> = HashSet::new();
    walk_branch_seeded(&stage.clauses, imported, carry, strict, &mut |exports| {
        stage_export = exports;
        Ok(())
    })?;
    match &stage.next {
        Some(next) => stage_exports(next, imported, &stage_export, strict, outer_scope),
        None => {
            if strict {
                if let Some(n) = smallest_collision(&stage_export, outer_scope) {
                    return Err(duplicate_outer_variable(n));
                }
            }
            Ok(stage_export)
        }
    }
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
                        if let Some(name) = smallest_collision(&exports, &scope) {
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
