//! Expression-position rules: pattern predicates in WHERE, patterns in
//! RETURN/WITH projections and SET values, property access and `labels()`/
//! `type()` on statically-known path/node variables, DELETE target shapes,
//! and undefined-variable checks for the expression positions they appear
//! in.

use std::collections::HashSet;

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{Clause, Expr, LiteralValue, PatternElement, PatternPath, Statement};

use super::duplicate_variable;
use crate::agg::contains_aggregation;
use crate::ast::collect::{
    check_expr_vars_defined, check_name_defined, clause_introduced_names,
    collect_pattern_element_vars, interp_embeds, projected_names, search_score_var,
};
use crate::ast::tree::walk_expr;

/// Validate expression-position constraints:
/// - Pattern predicates in WHERE must not introduce new (unbound) variables for relationships
///   or nodes that aren't already in scope.
/// - A single-node pattern predicate like `(n)` is not valid (InvalidArgumentType).
/// - Pattern predicates must not appear in RETURN/WITH projections or SET values (UnexpectedSyntax).
/// - size() on a pattern predicate is invalid (UnexpectedSyntax).
/// - Property access and labels() on named path variables are invalid (InvalidArgumentType).
/// - DELETE targets must be entity-shaped (variables, property/subscript access, CASE), and
///   MERGE/CREATE patterns reject element-pattern predicates.
pub fn validate_expression_constraints(stmt: &Statement) -> Result<(), EngineError> {
    let mut scope: HashSet<String> = HashSet::new();
    let mut path_vars: HashSet<String> = HashSet::new();

    for clause in &stmt.clauses {
        match clause {
            Clause::Match(mc) => {
                validate_match_clause(mc, &mut scope, &mut path_vars)?;
            }
            Clause::With(wc) => {
                validate_with_clause(wc, &mut scope, &mut path_vars)?;
            }
            Clause::Return(rc) => {
                validate_return_clause(rc, &mut scope, &mut path_vars)?;
            }
            Clause::Unwind(uw) => {
                validate_unwind_clause(uw, &mut scope)?;
            }
            Clause::LoadCSV(lc) => {
                validate_loadcsv_clause(lc, &mut scope)?;
            }
            Clause::Create(cc) => {
                validate_create_clause(cc, &mut scope)?;
            }
            Clause::Set(sc) => {
                validate_set_clause(sc, &mut scope)?;
            }
            Clause::Delete(dc) => {
                validate_delete_clause(dc, &mut scope)?;
            }
            Clause::CallProcedure(cp) => {
                validate_callprocedure_clause(cp, &mut scope)?;
            }
            // The row-producing catalog commands, through the one catalog
            // classification (the same guard the scope walkers use): each
            // introduces its effective output columns into scope.
            c if crate::show_columns::catalog_produces_rows(c) => {
                scope.extend(clause_introduced_names(c));
            }
            Clause::CallSubquery(csq) => {
                validate_callsubquery_clause(csq, &mut scope)?;
            }
            Clause::Merge(mc) => {
                validate_merge_clause(mc, &mut scope)?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// The Match clause's expression constraints: the body of the clause
/// dispatch above, over the shared scope and path-variable sets.
fn validate_match_clause(
    mc: &cypherlite_parser::MatchClause,
    scope: &mut HashSet<String>,
    path_vars: &mut HashSet<String>,
) -> Result<(), EngineError> {
    // Collect bound variables from match pattern
    //
    // The same path variable cannot be assigned to more than one
    // top-level path pattern of a single MATCH clause's
    // comma-separated graph pattern (CIP-60 Section 4.2 Rule 1 /
    // 3.3 point 4: a path variable is always an unconditional
    // singleton and cannot appear in a later constituent of the
    // conjunction).
    let mut path_vars_this_match: HashSet<String> = HashSet::new();
    for path in &mc.pattern {
        if let Some(pv) = &path.variable {
            if !path_vars_this_match.insert(pv.clone()) {
                return Err(duplicate_variable(pv));
            }
            scope.insert(pv.clone());
            path_vars.insert(pv.clone());
        }
        for elem in &path.elements {
            collect_pattern_element_vars(elem, &mut *scope);
        }
    }
    // Rule 6: a WHERE on either side of SEARCH may reference the
    // score variable -- add it to scope before any WHERE below
    // (element-pattern, path-pattern, or this MATCH's own) is
    // checked.
    if let Some(sv) = search_score_var(mc) {
        scope.insert(sv.to_string());
    }
    // Validate element-pattern WHERE clauses, e.g. `(n WHERE
    // n.age > 30)` / `[r WHERE r.p > 0]` (CIP-17/CIP-18). Scope
    // was fully populated above from every path in this MATCH
    // clause before this check runs, so peer element variables
    // are visible -- including forward references to a variable
    // declared later in the same path (per CIP-17/18 5.2.1) --
    // as well as variables from prior clauses.
    for path in &mc.pattern {
        for elem in &path.elements {
            // A `Quantified` element's own per-iteration WHERE
            // (and any element-pattern WHERE inside its operand)
            // is deliberately not scope-checked here: the
            // operand's local group variables are visible only
            // inside it. The planner's quantified-predicate
            // binding (`NonLocalGroupVariableReference`/
            // `UndefinedVariable` in `cypherlite-planner`)
            // checks those on the lowered IR.
            let where_expr = match elem {
                PatternElement::Node(np) => np.where_expr.as_ref(),
                PatternElement::Relationship(rp) => rp.where_expr.as_ref(),
                PatternElement::Quantified(_) => None,
            };
            if let Some(where_expr) = where_expr {
                check_expr_vars_defined(where_expr, scope)?;
                check_property_on_paths(where_expr, path_vars)?;
            }
        }
        // Validate the parenthesized path pattern's own WHERE,
        // e.g. `((n)-[r]->(m) WHERE n.p > m.q)` (CIP-19 Rule 5).
        // It may reference element variables local to the same
        // path, variables from a prior clause, or unconditional
        // singleton variables of the same MATCH -- all already
        // present in `scope` -- but not an undeclared variable.
        if let Some(where_expr) = &path.where_expr {
            check_expr_vars_defined(where_expr, scope)?;
            check_property_on_paths(where_expr, path_vars)?;
        }
    }
    // Validate WHERE expression
    if let Some(where_expr) = &mc.where_expr {
        validate_expr_in_where(where_expr, scope)?;
        // Check for undefined variables in WHERE
        check_expr_vars_defined(where_expr, scope)?;
        // Check for property access on path variables
        check_property_on_paths(where_expr, path_vars)?;
    }
    Ok(())
}

/// The With clause's expression constraints: the body of the clause
/// dispatch above, over the shared scope and path-variable sets.
fn validate_with_clause(
    wc: &cypherlite_parser::WithClause,
    scope: &mut HashSet<String>,
    path_vars: &mut HashSet<String>,
) -> Result<(), EngineError> {
    // Check for patterns in WITH projection items
    for item in &wc.items {
        if !item.all {
            check_no_pattern_in_projection(&item.expr, "WITH")?;
            // labels() on a named-path variable (Graph3 [8]).
            check_labels_on_paths(&item.expr, path_vars)?;
        }
    }
    // WITH WHERE can have pattern predicates
    if let Some(where_expr) = &wc.where_expr {
        validate_expr_in_where(where_expr, scope)?;
    }
    // Reset scope to the projected names through the one WITH/RETURN
    // projection rule (`projected_names` also keeps names projected before
    // a `*` item, unlike an ad-hoc reset-on-star loop).
    *scope = projected_names(&wc.items, scope);
    Ok(())
}

/// The Return clause's expression constraints: the body of the clause
/// dispatch above, over the shared scope and path-variable sets.
fn validate_return_clause(
    rc: &cypherlite_parser::ReturnClause,
    scope: &mut HashSet<String>,
    path_vars: &mut HashSet<String>,
) -> Result<(), EngineError> {
    // Check for patterns in RETURN projection items
    for item in &rc.items {
        if !item.all {
            check_no_pattern_in_projection(&item.expr, "RETURN")?;
            // Check for size() on paths/patterns in RETURN
            check_size_constraints(&item.expr, scope)?;
            // labels() on a named-path variable (Graph3 [8]).
            check_labels_on_paths(&item.expr, path_vars)?;
        }
    }
    Ok(())
}

/// The Unwind clause's expression constraints: the body of the clause
/// dispatch above, over the shared scope and path-variable sets.
fn validate_unwind_clause(
    uw: &cypherlite_parser::UnwindClause,
    scope: &mut HashSet<String>,
) -> Result<(), EngineError> {
    scope.insert(uw.alias.clone());
    Ok(())
}

/// The LoadCSV clause's expression constraints: the body of the clause
/// dispatch above, over the shared scope and path-variable sets.
fn validate_loadcsv_clause(
    lc: &cypherlite_parser::LoadCSVClause,
    scope: &mut HashSet<String>,
) -> Result<(), EngineError> {
    scope.insert(lc.alias.clone());
    Ok(())
}

/// The Create clause's expression constraints: the body of the clause
/// dispatch above, over the shared scope and path-variable sets.
fn validate_create_clause(
    cc: &cypherlite_parser::CreateClause,
    scope: &mut HashSet<String>,
) -> Result<(), EngineError> {
    for path in &cc.pattern {
        if let Some(pv) = &path.variable {
            scope.insert(pv.clone());
        }
        walk_write_elements(&path.elements, WriteClause::Create, scope)?;
    }
    Ok(())
}

/// The Set clause's expression constraints: the body of the clause
/// dispatch above, over the shared scope and path-variable sets.
fn validate_set_clause(
    sc: &cypherlite_parser::SetClause,
    scope: &mut HashSet<String>,
) -> Result<(), EngineError> {
    // The one SET-item rule, shared with MERGE's ON CREATE/ON MATCH SET.
    check_set_items(&sc.items, scope)
}

/// The Delete clause's expression constraints: the body of the clause
/// dispatch above, over the shared scope and path-variable sets.
fn validate_delete_clause(
    dc: &cypherlite_parser::DeleteClause,
    scope: &mut HashSet<String>,
) -> Result<(), EngineError> {
    // Check undefined variables and invalid argument types in DELETE
    for expr in &dc.expressions {
        // Check that DELETE expressions are variables, property access, or subscripts
        // Arithmetic expressions like `1 + 1` are not valid
        check_delete_expr(expr)?;
        // Check for undefined root variable in DELETE expression
        if let Some(root_var) = find_root_variable(expr) {
            check_name_defined(&root_var, scope)?;
        }
    }
    Ok(())
}

/// The CallProcedure clause's expression constraints: the body of the clause
/// dispatch above, over the shared scope and path-variable sets.
fn validate_callprocedure_clause(
    cp: &cypherlite_parser::CallProcedureClause,
    scope: &mut HashSet<String>,
) -> Result<(), EngineError> {
    // Aggregation functions are not allowed in procedure arguments
    for arg in &cp.args {
        if contains_aggregation(arg) {
            return Err(EngineError::semantic(
                ErrorKind::InvalidAggregation,
                "aggregation functions are not allowed in procedure call arguments".to_string(),
            ));
        }
    }
    // Add yielded variables to scope (the effective post-alias
    // name comes from the one YieldList model)
    for name in cp.yields.effective_names() {
        scope.insert(name.to_owned());
    }
    Ok(())
}

/// The CallSubquery clause's expression constraints: the body of the clause
/// dispatch above, over the shared scope and path-variable sets.
fn validate_callsubquery_clause(
    csq: &cypherlite_parser::CallSubqueryClause,
    scope: &mut HashSet<String>,
) -> Result<(), EngineError> {
    // CIP-141/CIP-218: what a CALL clause exports depends on
    // its scope clause (a scope-clause `RETURN *` excludes
    // imported names per Rule 10); delegate to the shared
    // analysis rather than assuming `RETURN *` always inherits
    // outer scope (only true for a naked `CALL { ... }`).
    let exports = crate::call_scope::call_subquery_exports(csq, scope);
    scope.extend(exports);
    // CIP-30: `REPORT STATUS AS <var>` binds `<var>` into scope
    // for every clause after the CALL (newness of the name
    // itself is checked separately by `validate_variable_types`).
    if let Some(cit) = &csq.in_transactions {
        if let Some(var) = &cit.report_status {
            scope.insert(var.clone());
        }
    }
    Ok(())
}

/// The Merge clause's expression constraints: the body of the clause
/// dispatch above, over the shared scope and path-variable sets.
fn validate_merge_clause(
    mc: &cypherlite_parser::MergeClause,
    scope: &mut HashSet<String>,
) -> Result<(), EngineError> {
    // Add MERGE pattern variables to scope
    if let Some(pv) = &mc.pattern.variable {
        scope.insert(pv.clone());
    }
    walk_write_elements(&mc.pattern.elements, WriteClause::Merge, scope)?;
    // The ON CREATE SET / ON MATCH SET checks are one rule
    // over two item lists (the clause's own two fields).
    check_set_items(&mc.on_create, scope)?;
    check_set_items(&mc.on_match, scope)?;
    Ok(())
}

/// The two write clauses whose patterns reject element predicates. The
/// variant is the single source of the diagnostic label and of whether the
/// element property values are scope-checked (CREATE's are, MERGE's are
/// not), so the two facts cannot drift apart.
#[derive(Clone, Copy)]
enum WriteClause {
    Create,
    Merge,
}

impl WriteClause {
    fn label(self) -> &'static str {
        match self {
            WriteClause::Create => "CREATE",
            WriteClause::Merge => "MERGE",
        }
    }

    fn checks_pattern_props(self) -> bool {
        matches!(self, WriteClause::Create)
    }
}

/// The CREATE/MERGE pattern-element walker: every element's pattern
/// predicate is rejected (the clause named in the message), a quantified
/// element is rejected outright, and the element's variable joins the
/// scope. The clause kind also decides whether the property values are
/// scope-checked.
fn walk_write_elements(
    elements: &[PatternElement],
    clause: WriteClause,
    scope: &mut HashSet<String>,
) -> Result<(), EngineError> {
    for elem in elements {
        // One body over the shared element shape: the where-expression,
        // the property map and the variable are all a node and a
        // relationship element carry alike.
        let (where_expr, properties, variable) = match elem {
            PatternElement::Node(np) => {
                (np.where_expr.as_ref(), &np.properties, np.variable.as_ref())
            }
            PatternElement::Relationship(rp) => {
                (rp.where_expr.as_ref(), &rp.properties, rp.variable.as_ref())
            }
            PatternElement::Quantified(_) => {
                return Err(EngineError::semantic(
                    ErrorKind::QuantifiedPathPatternNotSupported,
                    format!(
                        "quantified path patterns cannot be used in {}",
                        clause.label()
                    ),
                ));
            }
        };
        // Element-pattern WHERE (CIP-17) is scoped to matching only;
        // Neo4j rejects it in a write clause.
        if where_expr.is_some() {
            return Err(EngineError::semantic(
                ErrorKind::InvalidNodePatternPredicate,
                format!(
                    "node and relationship pattern predicates cannot be used in a {} clause",
                    clause.label()
                ),
            ));
        }
        if clause.checks_pattern_props() {
            for (_key, val_expr) in properties {
                check_expr_vars_defined(val_expr, scope)?;
            }
        }
        if let Some(v) = variable {
            scope.insert(v.clone());
        }
    }
    Ok(())
}

/// The one SET-item rule, shared by a `SET` clause and MERGE's ON CREATE
/// SET / ON MATCH SET: no pattern predicate in a value, the value
/// expression's variables must be bound, and the item's target — a
/// property access's variable or a bare variable — must be too.
fn check_set_items(
    items: &[cypherlite_parser::SetItem],
    scope: &HashSet<String>,
) -> Result<(), EngineError> {
    for item in items {
        if let Some(val) = &item.value {
            check_no_pattern_in_set_value(val)?;
            check_expr_vars_defined(val, scope)?;
        }
        if let Some(prop) = &item.property {
            if let Expr::Variable(var_name) = &prop.expr {
                check_name_defined(var_name, scope)?;
            }
        } else if let Some(var_name) = &item.variable {
            check_name_defined(var_name, scope)?;
        }
    }
    Ok(())
}

/// Check that an expression used in WHERE doesn't violate pattern predicate rules.
fn validate_expr_in_where(expr: &Expr, scope: &HashSet<String>) -> Result<(), EngineError> {
    validate_expr_in_where_inner(expr, scope, true)
}

fn validate_expr_in_where_inner(
    expr: &Expr,
    scope: &HashSet<String>,
    top_level: bool,
) -> Result<(), EngineError> {
    match expr {
        Expr::PatternPredicate(pp) => {
            validate_pattern_predicate(&pp.pattern, scope)?;
        }
        Expr::Unary(u) if u.op == "NOT" => {
            validate_expr_in_where_inner(&u.expr, scope, top_level)?;
        }
        Expr::Binary(b) if b.op == "AND" || b.op == "OR" || b.op == "XOR" => {
            validate_expr_in_where_inner(&b.left, scope, top_level)?;
            validate_expr_in_where_inner(&b.right, scope, top_level)?;
        }
        Expr::Paren(inner) => {
            // A parenthesized single variable like `(a)` or `(n)` used as a boolean
            // predicate in WHERE looks like a single-node pattern predicate.
            if top_level {
                if let Expr::Variable(v) = inner.as_ref() {
                    if !scope.contains(v) {
                        return Err(EngineError::semantic(
                            ErrorKind::UndefinedVariable,
                            format!("variable '{v}' not defined"),
                        ));
                    } else {
                        return Err(EngineError::semantic(
                            ErrorKind::InvalidArgumentType,
                            "a single node is not a valid pattern predicate".to_string(),
                        ));
                    }
                }
            }
            validate_expr_in_where_inner(inner, scope, top_level)?;
        }
        Expr::StringInterp(parts) => {
            for e in interp_embeds(parts) {
                validate_expr_in_where_inner(e, scope, false)?;
            }
        }
        _ => {
            // Check for size() constraints in WHERE expressions too
            check_size_constraints(expr, scope)?;
        }
    }
    Ok(())
}

/// Validate a pattern predicate used in WHERE.
/// - Must have at least one relationship (not just a single node).
/// - Must not introduce new unbound variables.
fn validate_pattern_predicate(
    pattern: &PatternPath,
    scope: &HashSet<String>,
) -> Result<(), EngineError> {
    // Check: single-node pattern predicate like `(n)` is invalid
    let has_rel = pattern
        .elements
        .iter()
        .any(|e| matches!(e, PatternElement::Relationship(_)));
    if !has_rel {
        return Err(EngineError::semantic(
            ErrorKind::InvalidArgumentType,
            "a single node is not a valid pattern predicate".to_string(),
        ));
    }

    // Check for unbound variables introduced by pattern predicate
    for elem in &pattern.elements {
        match elem {
            PatternElement::Relationship(rp) => {
                if let Some(v) = &rp.variable {
                    if !scope.contains(v) {
                        return Err(EngineError::semantic(
                            ErrorKind::UndefinedVariable,
                            format!("variable '{v}' not defined"),
                        ));
                    }
                }
            }
            PatternElement::Node(np) => {
                if let Some(v) = &np.variable {
                    if !scope.contains(v) {
                        // New node variables with labels/properties => unbound
                        // New node variables without labels/properties => also unbound
                        // Any new variable introduced in a pattern predicate is an error
                        return Err(EngineError::semantic(
                            ErrorKind::UndefinedVariable,
                            format!("variable '{v}' not defined"),
                        ));
                    }
                }
            }
            // Quantified path patterns as pattern predicates aren't part of
            // this CIP-28 batch's tested surface.
            PatternElement::Quantified(_) => {}
        }
    }
    Ok(())
}

/// Check that a projection expression does not contain pattern predicates.
/// Patterns like `(n)-[]->()` are not valid in RETURN or WITH projections.
fn check_no_pattern_in_projection(expr: &Expr, ctx: &str) -> Result<(), EngineError> {
    // A pattern predicate is illegal anywhere in a projection item.
    walk_expr(expr, &mut |e| {
        if matches!(e, Expr::PatternPredicate(_)) {
            Err(EngineError::semantic(
                ErrorKind::UnexpectedSyntax,
                format!("pattern expressions are not allowed in {ctx} projection"),
            ))
        } else {
            Ok(())
        }
    })
}

/// Check for property access on path variables.
fn check_property_on_paths(expr: &Expr, path_vars: &HashSet<String>) -> Result<(), EngineError> {
    walk_expr(expr, &mut |e| {
        if let Expr::Property(p) = e {
            if let Expr::Variable(v) = &p.expr {
                if path_vars.contains(v) {
                    return Err(EngineError::semantic(
                        ErrorKind::InvalidArgumentType,
                        "property access is not allowed on paths".to_string(),
                    ));
                }
            }
        }
        Ok(())
    })
}

/// `labels()` applied to a named-path variable is a compile-time
/// `InvalidArgumentType` (TCK expressions/graph/Graph3 [8]: `MATCH p = (a)
/// RETURN labels(p)` — `p` is statically a PATH, and `labels()` requires a
/// node). CypherLite has no general type inferencer, so this is deliberately
/// scoped to the statically-certain case: the sole argument is *directly* a
/// path variable. Anything else (a variable that might be a node, a property
/// access, …) is left to the runtime `labels()` type check.
fn check_labels_on_paths(expr: &Expr, path_vars: &HashSet<String>) -> Result<(), EngineError> {
    walk_expr(expr, &mut |e| {
        if let Expr::FunctionCall(fc) = e {
            if fc.name.eq_ignore_ascii_case("labels") && fc.args.len() == 1 {
                if let Expr::Variable(v) = &fc.args[0] {
                    if path_vars.contains(v) {
                        return Err(EngineError::semantic(
                            ErrorKind::InvalidArgumentType,
                            "labels() requires a node, got PATH".to_string(),
                        ));
                    }
                }
            }
        }
        Ok(())
    })
}

/// Find the root variable name from an expression (traversing property access and subscripts).
fn find_root_variable(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Variable(v) => Some(v.clone()),
        Expr::Property(p) => find_root_variable(&p.expr),
        Expr::Subscript(s) => find_root_variable(&s.expr),
        Expr::Paren(inner) => find_root_variable(inner),
        Expr::StringInterp(parts) => interp_embeds(parts).find_map(find_root_variable),
        _ => None,
    }
}

/// Base sentence of every DELETE-target diagnostic. The HasLabel and
/// generic arms use it verbatim; the CIP-55 keyword-literal arm appends
/// the `inf`/`infinity`/`nan` explanation to it.
const DELETE_TARGET_MSG: &str = "DELETE requires a node or relationship, not an expression";

/// Check that a DELETE expression is a valid deletable target.
///
/// This can't be a full static type check (CypherLite has no type
/// inferencer), so it's deliberately permissive about expression forms whose
/// *runtime* type could plausibly be a node/relationship/path/list
/// (variables, property/subscript access, and `CASE` -- CIP-118 explicitly
/// requires DELETE to accept "any expression, including subquery
/// expressions", e.g. `DELETE (CASE WHEN EXISTS {...} THEN q ELSE p END)`;
/// the subquery expression driving the condition is nested inside a `WHEN`,
/// not itself a delete target, so only the `THEN`/`ELSE` result branches
/// need checking) -- `delete::collect_delete_targets` raises a runtime error
/// if such an expression actually produces something undeletable. Only
/// expression forms that are *statically* certain to never produce a
/// deletable value (arithmetic, literals, boolean/comparison operators, ...)
/// are rejected here, matching TCK `clauses/delete/Delete5.feature` scenario
/// 9 (`DELETE 1 + 1` must be a compile-time `InvalidArgumentType`
/// SyntaxError, even against an empty graph where DELETE never actually
/// runs).
fn check_delete_expr(expr: &Expr) -> Result<(), EngineError> {
    match expr {
        Expr::Variable(_) => Ok(()),
        Expr::Property(_) => Ok(()),
        Expr::Subscript(_) => Ok(()),
        Expr::Paren(inner) => check_delete_expr(inner),
        Expr::Case(c) => {
            for w in &c.whens {
                check_delete_expr(&w.result)?;
            }
            if let Some(e) = &c.else_expr {
                check_delete_expr(e)?;
            }
            Ok(())
        }
        // `DELETE n:Person` / `DELETE r:T` parse as a label-predicate boolean
        // expression; Neo4j rejects these as a compile-time `SyntaxError:
        // InvalidDelete` (TCK clauses/delete Delete1 scenario 8, Delete2
        // scenario 5) — a distinct detail from the generic non-entity case.
        Expr::HasLabel(_) => Err(EngineError::semantic(
            ErrorKind::InvalidDelete,
            DELETE_TARGET_MSG.to_string(),
        )),
        // A bare `inf`/`infinity`/`nan` DELETE target parses as the CIP-55
        // float literal (parse_primary lowers the INFINITY/NAN keyword tokens
        // unconditionally). The parser rejects overflowing numeric literals
        // like `1e400`, so a *non-finite* Float literal in the AST can only
        // have come from one of those keyword tokens — name the cause and the
        // backtick-escape workaround (ADR-0280). The error stays a
        // compile-time `InvalidArgumentType`, as TCK Delete5 scenario 9 pins.
        Expr::Literal(LiteralValue::Float(f)) if !f.is_finite() => Err(EngineError::semantic(
            ErrorKind::InvalidArgumentType,
            format!(
                "{DELETE_TARGET_MSG}: `inf`, `infinity` and `nan` are CIP-55 float-literal \
                 keywords (Infinity/NaN), not variables; backtick-escape the name (e.g. DELETE \
                 `inf`) to delete a variable of that name"
            ),
        )),
        // Any other non-entity expression (`DELETE 1 + 1`) is a compile-time
        // `SyntaxError: InvalidArgumentType` (TCK Delete5 scenario 9).
        _ => Err(EngineError::semantic(
            ErrorKind::InvalidArgumentType,
            DELETE_TARGET_MSG.to_string(),
        )),
    }
}

/// Check that a SET value expression doesn't contain patterns.
fn check_no_pattern_in_set_value(expr: &Expr) -> Result<(), EngineError> {
    // A pattern predicate is illegal anywhere in a SET value.
    walk_expr(expr, &mut |e| {
        if matches!(e, Expr::PatternPredicate(_)) {
            Err(EngineError::semantic(
                ErrorKind::UnexpectedSyntax,
                "pattern expressions are not allowed in SET values".to_string(),
            ))
        } else {
            Ok(())
        }
    })
}

/// Check the size() constraint this pass owns: size() on a pattern
/// predicate is invalid (UnexpectedSyntax). (size() on a path *variable* is
/// rejected on the lowered IR by the planner's `size()` argument check.)
fn check_size_constraints(expr: &Expr, _scope: &HashSet<String>) -> Result<(), EngineError> {
    // `size(<pattern predicate>)` is illegal anywhere.
    walk_expr(expr, &mut |e| {
        if let Expr::FunctionCall(fc) = e {
            if fc.name.to_lowercase() == "size"
                && fc.args.len() == 1
                && matches!(&fc.args[0], Expr::PatternPredicate(_))
            {
                return Err(EngineError::semantic(
                    ErrorKind::UnexpectedSyntax,
                    "size() cannot be applied to a pattern predicate".to_string(),
                ));
            }
        }
        Ok(())
    })
}
/// Validate that type() is not called on node variables.
pub fn validate_type_on_nodes(stmt: &Statement) -> Result<(), EngineError> {
    // First, collect which variables are nodes from MATCH patterns
    let mut node_vars: HashSet<String> = HashSet::new();

    for clause in &stmt.clauses {
        match clause {
            Clause::Match(mc) => {
                for path in &mc.pattern {
                    for elem in &path.elements {
                        if let PatternElement::Node(np) = elem {
                            if let Some(v) = &np.variable {
                                node_vars.insert(v.clone());
                            }
                        }
                    }
                }
            }
            Clause::With(_) => {
                // WITH resets scope
                node_vars.clear();
            }
            _ => {}
        }
    }

    // Now check all expressions in the statement for type() calls on node variables
    for clause in &stmt.clauses {
        match clause {
            Clause::Return(rc) => {
                for item in &rc.items {
                    check_type_on_nodes(&item.expr, &node_vars)?;
                }
            }
            Clause::With(wc) => {
                for item in &wc.items {
                    check_type_on_nodes(&item.expr, &node_vars)?;
                }
            }
            Clause::Match(mc) => {
                if let Some(w) = &mc.where_expr {
                    check_type_on_nodes(w, &node_vars)?;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn check_type_on_nodes(expr: &Expr, node_vars: &HashSet<String>) -> Result<(), EngineError> {
    // `type(<node variable>)` is illegal anywhere (type() is for
    // relationships).
    walk_expr(expr, &mut |e| {
        if let Expr::FunctionCall(fc) = e {
            if fc.name.eq_ignore_ascii_case("type") && fc.args.len() == 1 {
                if let Expr::Variable(v) = &fc.args[0] {
                    if node_vars.contains(v) {
                        return Err(EngineError::semantic(
                            ErrorKind::InvalidArgumentType,
                            "type() can only be applied to relationships, not nodes".to_string(),
                        ));
                    }
                }
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `query` and run the expression-constraints pass that owns the
    /// DELETE-target check.
    fn parse_and_validate(query: &str) -> Result<(), EngineError> {
        let stmt = cypherlite_parser::parse(query).expect("parse");
        validate_expression_constraints(&stmt)
    }

    // ADR-0280 ruling 2: a bare `inf`/`infinity`/`nan` DELETE target is the
    // CIP-55 float literal, not a variable; the static rejection names the
    // keyword and prescribes the backtick escape.
    #[test]
    fn delete_keyword_literal_target_names_backtick_escape() {
        for kw in ["inf", "infinity", "nan", "INF", "Infinity", "NaN"] {
            let err = parse_and_validate(&format!("MATCH (n) DELETE {kw}"))
                .expect_err(&format!("DELETE {kw} must be rejected"));
            assert!(
                matches!(err, EngineError::Semantic { .. }),
                "DELETE {kw} must fail at compile time (Semantic), got {err:?}"
            );
            assert_eq!(
                err.kind(),
                ErrorKind::InvalidArgumentType,
                "DELETE {kw} keeps the TCK Delete5 scenario 9 error class"
            );
            assert_eq!(
                err.to_string(),
                "InvalidArgumentType: DELETE requires a node or relationship, not an \
                 expression: `inf`, `infinity` and `nan` are CIP-55 float-literal keywords \
                 (Infinity/NaN), not variables; backtick-escape the name (e.g. DELETE `inf`) \
                 to delete a variable of that name",
                "DELETE {kw} message"
            );
        }
    }

    #[test]
    fn delete_keyword_literal_diagnostic_through_parens_and_detach() {
        // The dedicated arm is reached through Paren unwrapping and DETACH.
        for query in [
            "MATCH (n) DELETE (inf)",
            "MATCH (n) DETACH DELETE nan",
            "OPTIONAL MATCH (n)-[:R]->(inf:N) DETACH DELETE n, inf",
        ] {
            let err = parse_and_validate(query)
                .expect_err(&format!("`{query}` must be rejected statically"));
            assert_eq!(err.kind(), ErrorKind::InvalidArgumentType);
            assert!(
                err.to_string().contains("CIP-55 float-literal keywords"),
                "`{query}` must carry the keyword diagnostic, got {err}"
            );
        }
    }

    #[test]
    fn delete_non_literal_expression_keeps_generic_message() {
        // TCK Delete5 scenario 9 pins `DELETE 1 + 1`: compile-time
        // InvalidArgumentType with the generic message (no keyword note).
        for query in ["MATCH () DELETE 1 + 1", "MATCH (n) DELETE 1.5"] {
            let err = parse_and_validate(query)
                .expect_err(&format!("`{query}` must be rejected statically"));
            assert_eq!(err.kind(), ErrorKind::InvalidArgumentType);
            assert_eq!(
                err.to_string(),
                "InvalidArgumentType: DELETE requires a node or relationship, not an expression",
                "`{query}` keeps the generic message"
            );
        }
    }

    #[test]
    fn delete_backticked_keyword_variable_is_accepted() {
        // The backtick escape forces variable resolution (ADR-0280 ruling 1):
        // a bound `` `inf` `` is a legal DELETE target.
        parse_and_validate("MATCH (`inf`) DELETE `inf`")
            .expect("backticked `inf` variable must be a valid DELETE target");
        parse_and_validate("MATCH (`nan`)-[r]->(`infinity`) DELETE r, `nan`, `infinity`")
            .expect("backticked keyword variables must be valid DELETE targets");
    }
}
