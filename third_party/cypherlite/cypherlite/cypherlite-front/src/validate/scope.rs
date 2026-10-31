//! Cross-clause variable scope and usage-type tracking.
//!
//! [`validate_variable_types`] detects variable-reuse type conflicts at
//! compile time by tracking variable usage types (Node, Relationship, Path)
//! across clauses. A clash involving a path is raised as `SyntaxError:
//! VariableAlreadyBound`; any other clash (node↔relationship, scalar↔node)
//! as `SyntaxError: VariableTypeConflict`, matching the openCypher TCK.
//! Cypher requires that a variable is used consistently: a relationship
//! variable cannot also appear as a node, a path variable cannot appear as a
//! node, and a scalar value cannot be used as a node or relationship in a
//! MATCH pattern.
//!
//! The subquery output-variable analysis this builds on lives in
//! [`super::output_vars`]; `call_scope` builds its CALL-subquery
//! scope-clause checks on it.

use std::collections::{HashMap, HashSet};

use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{
    CallSubqueryClause, Clause, Expr, LiteralValue, PatternElement, PatternPath, Statement,
    YieldMode,
};

use super::output_vars::{compute_subquery_output_vars, compute_subquery_output_vars_seeded};
use super::writes::{
    validate_create_bindings, validate_dynamic_label_no_any, validate_dynamic_label_scope,
    validate_merge_bindings,
};
use super::{duplicate_outer_variable, duplicate_variable};
use crate::ast::collect::{bind_pattern_vars, effective_name, search_score_var};
use crate::show_columns::{catalog_output_columns, catalog_produces_rows};

/// The TCK-keyed error kind for a type-conflicting reuse: the kind keys on
/// the NEW (later) usage — reused as a path is `VariableAlreadyBound`
/// (Match6 scenarios 21–24); reused as a node/relationship — including
/// reusing a path variable as one — is `VariableTypeConflict` (Match1
/// scenarios 7/8/10, Match2 9/10). Both the cross-clause and the
/// same-clause reuse checks select through this one predicate.
fn reuse_error_kind(new_type: VarType) -> ErrorKind {
    if new_type == VarType::Path {
        ErrorKind::VariableAlreadyBound
    } else {
        ErrorKind::VariableTypeConflict
    }
}

/// The type of a variable as determined by its usage context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarType {
    Node,
    Relationship,
    Path,
    /// A value projected through WITH that is not a simple variable pass-through.
    /// e.g., `WITH true AS n` or `WITH 123 AS n`
    Scalar,
}

impl VarType {
    fn name(&self) -> &'static str {
        match self {
            VarType::Node => "node",
            VarType::Relationship => "relationship",
            VarType::Path => "path",
            VarType::Scalar => "scalar value",
        }
    }
}
/// Validate a statement for variable type conflicts and VariableAlreadyBound errors.
///
/// Returns `Ok(())` if valid, or `Err(..)` with a `VariableTypeConflict` /
/// `VariableAlreadyBound` kind. Both the cross-clause and within-pattern checks
/// key the kind on the *reuse* (later) usage: reused as a path is
/// `VariableAlreadyBound`, reused as a node/relationship is
/// `VariableTypeConflict` (a path's element variables are treated as bound
/// before the path variable — see the ordering note in the collection loop).
pub fn validate_variable_types(stmt: &Statement) -> Result<(), EngineError> {
    let mut var_types: HashMap<String, VarType> = HashMap::new();
    // Track all variables that are "bound" (defined by MATCH/CREATE/UNWIND/WITH)
    let mut bound_vars: HashSet<String> = HashSet::new();
    validate_variable_types_in_clauses(&stmt.clauses, &mut var_types, &mut bound_vars)
}

/// The clause-list body of [`validate_variable_types`], factored out so
/// `FOREACH` bodies — which share the enclosing scope, unlike `CALL { }`
/// subqueries — are validated against the enclosing variable state instead
/// of skipping every check (LABS-81: a `FOREACH` body used to lower even
/// when the same clause was rejected at top level).
fn validate_variable_types_in_clauses(
    clauses: &[Clause],
    var_types: &mut HashMap<String, VarType>,
    bound_vars: &mut HashSet<String>,
) -> Result<(), EngineError> {
    for clause in clauses {
        match clause {
            Clause::Match(mc) => {
                // Collect all variable usages in this MATCH clause
                validate_match_patterns(&mc.pattern, var_types)?;
                // Track bound variables from MATCH
                bind_pattern_vars(&mc.pattern, bound_vars);
                if let Some(sv) = search_score_var(mc) {
                    bound_vars.insert(sv.to_string());
                }
            }
            Clause::With(wc) => {
                // WITH resets the scope. Variables projected through WITH get their
                // types from the expression. If the expression is a literal (not a
                // variable reference), the resulting variable is Scalar.
                let mut new_types: HashMap<String, VarType> = HashMap::new();
                let mut new_bound: HashSet<String> = HashSet::new();
                for item in &wc.items {
                    if item.all {
                        // WITH * passes through all existing variables with their types
                        for (k, v) in var_types.iter() {
                            new_types.insert(k.clone(), *v);
                        }
                        new_bound = bound_vars.clone();
                        continue;
                    }
                    let Some(alias) = effective_name(item) else {
                        continue;
                    };

                    new_bound.insert(alias.clone());

                    // Determine the type of the projected expression
                    match &item.expr {
                        Expr::Variable(v) => {
                            // Pass through the existing type
                            if let Some(t) = var_types.get(v) {
                                new_types.insert(alias, *t);
                            }
                            // If unknown, don't constrain it
                        }
                        // Null literal doesn't constrain the variable type (null is compatible with any type)
                        Expr::Literal(LiteralValue::Null) => {}
                        // Known scalar types: literals, list literals, map literals
                        // These definitely can't be nodes or relationships
                        Expr::Literal(_) | Expr::List(_) | Expr::Map(_) => {
                            new_types.insert(alias, VarType::Scalar);
                        }
                        // For function calls and other complex expressions,
                        // we can't determine the type at compile time
                        // (e.g., coalesce(b, c) could return a node)
                        _ => {}
                    }
                }
                *var_types = new_types;
                *bound_vars = new_bound;
            }
            Clause::Unwind(uw) => {
                // UNWIND introduces a new variable
                bound_vars.insert(uw.alias.clone());
            }
            Clause::LoadCSV(lc) => {
                bound_vars.insert(lc.alias.clone());
            }
            Clause::Create(cc) => {
                // Check for VariableAlreadyBound: if a variable used in CREATE
                // for a node is already bound (from MATCH/previous CREATE), error
                validate_create_bindings(&cc.pattern, bound_vars)?;
                // CIP-156: `$any()` is a MATCH-only disjunction quantifier;
                // a dynamic label/type expression can't reference a
                // variable this very CREATE clause creates.
                validate_dynamic_label_no_any(&cc.pattern)?;
                validate_dynamic_label_scope(&cc.pattern, bound_vars)?;
                // Track new variables created by CREATE
                bind_pattern_vars(&cc.pattern, bound_vars);
            }
            Clause::Merge(mc) => {
                // Check for VariableAlreadyBound in MERGE: node/rel variables that
                // are already bound should not be redefined
                validate_merge_bindings(&mc.pattern, bound_vars)?;
                // CIP-156: same rules as CREATE (see above).
                let merge_pattern = std::slice::from_ref(&mc.pattern);
                validate_dynamic_label_no_any(merge_pattern)?;
                validate_dynamic_label_scope(merge_pattern, bound_vars)?;
                // Track new variables from MERGE
                bind_pattern_vars(merge_pattern, bound_vars);
            }
            Clause::CallProcedure(cp) => {
                // Check for YIELD * in in-query context (not standalone)
                // In-query = there are other clauses besides this CALL
                let is_in_query = clauses.len() > 1;
                if cp.yields.mode() == YieldMode::Star && is_in_query {
                    return Err(EngineError::semantic(
                        ErrorKind::UnexpectedSyntax,
                        "YIELD * is not allowed in in-query CALL".to_string(),
                    ));
                }

                // The effective variable names (alias takes precedence) come
                // from the one YieldList model, decoded once.
                let effective_names: Vec<String> =
                    cp.yields.effective_names().map(str::to_owned).collect();

                // YIELD variables shadow already-bound names, and a
                // duplicate alias is a collision too — one policy, shared
                // with the SHOW family (`check_yield_names_not_bound`).
                check_yield_names_not_bound(effective_names, bound_vars)?;
            }
            Clause::CallSubquery(csq) => {
                validate_legacy_import_collision(csq, bound_vars)?;

                // Bind the names the subquery exports through the canonical
                // lenient derivation (the same one the expression validator's
                // CALL arm and the SEARCH scope walk use).
                let exports = crate::call_scope::call_subquery_exports(csq, bound_vars);
                bound_vars.extend(exports);

                // CIP-30 syntax [7]: `REPORT STATUS AS <var>` must bind a
                // NEW variable name -- one not already in scope when the
                // CALL clause is reached.
                if let Some(cit) = &csq.in_transactions {
                    if let Some(var) = &cit.report_status {
                        if bound_vars.contains(var) {
                            return Err(duplicate_variable(var));
                        }
                        bound_vars.insert(var.clone());
                    }
                }
            }
            // A SHOW/TERMINATE clause's output columns are new bindings,
            // exactly like a `CALL proc() YIELD ...`'s (see the
            // `Clause::CallProcedure` arm above, which this mirrors): a
            // name that's already bound by an earlier clause in this same
            // statement (CIP-242 errors.feature [5]/[6]: a preceding
            // `MATCH`'s pattern variable colliding with a plain or
            // `AS`-aliased YIELD column) is a compile-time
            // VariableAlreadyBound error, not a silent shadow.
            c if catalog_produces_rows(c) => {
                check_yield_names_not_bound(catalog_output_columns(c), bound_vars)?;
            }
            // A FOREACH body shares the enclosing scope (it is not an
            // isolated subquery): validate the body clauses against the
            // enclosing bindings plus the loop variable. Bindings the body
            // creates do not escape (FOREACH exports nothing), so the body
            // is walked with a cloned state.
            Clause::Foreach(f) => {
                let mut body_types = var_types.clone();
                let mut body_bound = bound_vars.clone();
                body_bound.insert(f.variable.clone());
                validate_variable_types_in_clauses(&f.clauses, &mut body_types, &mut body_bound)?;
            }
            _ => {}
        }
    }

    Ok(())
}

/// The one yield-binding policy of `validate_variable_types`, shared by a
/// `CALL … YIELD` and by every SHOW/TERMINATE arm: `cols` (already
/// alias-resolved) must contain no duplicates and none already in
/// `bound_vars` -- both raise the same `VariableAlreadyBound` error -- then
/// every name is added to `bound_vars`.
fn check_yield_names_not_bound(
    cols: Vec<String>,
    bound_vars: &mut HashSet<String>,
) -> Result<(), EngineError> {
    // Two passes, one policy: every shadow error against the pre-clause
    // bound set fires before any duplicate error, so a query with both
    // violations always names the shadowed outer variable, never the
    // earlier duplicate. The SHOW family and `CALL … YIELD` share it.
    for name in &cols {
        if bound_vars.contains(name) {
            return Err(duplicate_variable(name));
        }
    }
    let mut seen = HashSet::new();
    for name in &cols {
        if !seen.insert(name.clone()) {
            return Err(duplicate_variable(name));
        }
    }
    bound_vars.extend(cols);
    Ok(())
}
/// Validate match patterns for variable type conflicts.
///
/// Checks each pattern path in a MATCH clause. For each variable:
/// - Node positions => VarType::Node
/// - Relationship positions => VarType::Relationship
/// - Named path variables => VarType::Path
///
/// If a variable already has a different type, it's a conflict.
fn validate_match_patterns(
    patterns: &[PatternPath],
    var_types: &mut HashMap<String, VarType>,
) -> Result<(), EngineError> {
    // First, collect all new variable usages from this MATCH before committing
    // (variable_name, type, is_variable_length_rel)
    let mut new_usages: Vec<(String, VarType, bool)> = Vec::new();

    for path in patterns {
        // Order matters for the reuse-keyed error kind below: a path's element
        // variables (nodes/relationships) are considered bound BEFORE the path
        // variable that names the whole pattern. So in `p = (p)-[]-()` the node
        // `p` is the original binding and the path variable `p` is the reuse
        // (VariableAlreadyBound), while in `p = ()-[]-(), (p)` the path variable
        // is the original and the later node `(p)` is the reuse
        // (VariableTypeConflict) — matching clauses/match/Match1 scenario 10 and
        // Match6 scenario 23. Hence the path-variable push is deferred to after
        // this path's elements.
        for elem in &path.elements {
            match elem {
                PatternElement::Node(np) => {
                    if let Some(var) = &np.variable {
                        new_usages.push((var.clone(), VarType::Node, false));
                    }
                }
                PatternElement::Relationship(rp) => {
                    if let Some(var) = &rp.variable {
                        let is_varlen = rp.min_hops.is_some() || rp.max_hops.is_some();
                        new_usages.push((var.clone(), VarType::Relationship, is_varlen));
                    }
                }
                PatternElement::Quantified(qpp) => {
                    // A quantified path pattern's own named operand elements
                    // become "group variables" (CIP-28); basic node/
                    // relationship type tracking still applies to each. The
                    // group-specific reuse rules (a group variable name
                    // cannot be reused anywhere else at all) are enforced on
                    // the lowered IR by the planner's quantified-path binding
                    // (`GroupVariableReuse` in `cypherlite-planner`).
                    for inner in &qpp.elements {
                        match inner {
                            PatternElement::Node(np) => {
                                if let Some(var) = &np.variable {
                                    new_usages.push((var.clone(), VarType::Node, false));
                                }
                            }
                            PatternElement::Relationship(rp) => {
                                if let Some(var) = &rp.variable {
                                    new_usages.push((var.clone(), VarType::Relationship, false));
                                }
                            }
                            PatternElement::Quantified(_) => {
                                // The parser rejects nested QPPs (CIP-28
                                // Rule 4), but a hand-built AST can still
                                // reach this validator — return a typed
                                // error, never abort (LABS-70).
                                return Err(EngineError::semantic(
                                    ErrorKind::SyntaxError,
                                    "NestedQuantification: quantified path patterns cannot be nested"
                                        .to_string(),
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Path variable binds after its elements (see the ordering note above).
        if let Some(path_var) = &path.variable {
            new_usages.push((path_var.clone(), VarType::Path, false));
        }
    }

    // Now check all usages for conflicts
    for (i, (var, new_type, is_varlen)) in new_usages.iter().enumerate() {
        // Check against already-established types from previous clauses
        if let Some(existing_type) = var_types.get(var) {
            if *existing_type != *new_type {
                // A Scalar variable from WITH can be rebound as a Relationship
                // in a variable-length pattern (e.g., WITH [r1, r2] AS rs ... [rs*])
                // because variable-length rel patterns accept list variables.
                let allowed = *existing_type == VarType::Scalar
                    && *new_type == VarType::Relationship
                    && *is_varlen;
                if !allowed {
                    // Cross-clause reuse (Match6 21/22 vs Match1 7/8, Match2 9/10).
                    let kind = reuse_error_kind(*new_type);
                    return Err(EngineError::semantic(
                        kind,
                        format!(
                            "variable '{}' is already used as a {} but is used as a {} here",
                            var,
                            existing_type.name(),
                            new_type.name(),
                        ),
                    ));
                }
            }
        }

        // Check against EARLIER usages within the same MATCH clause: the
        // current usage is the later one (the reuse), so its type keys the
        // error kind — reused as a path is VariableAlreadyBound (Match6 23/24),
        // reused as a node/relationship is VariableTypeConflict (Match1 10).
        for (other_var, other_type, _) in &new_usages[..i] {
            if other_var == var && *other_type != *new_type {
                let kind = reuse_error_kind(*new_type);
                return Err(EngineError::semantic(
                    kind,
                    format!(
                        "variable '{}' is used as both a {} and a {} in the same pattern",
                        var,
                        new_type.name(),
                        other_type.name(),
                    ),
                ));
            }
        }
    }

    // Commit all new types
    for (var, new_type, _) in new_usages {
        var_types.insert(var, new_type);
    }

    Ok(())
}

/// CIP-141 "legacy importing WITH": `CALL { WITH a, b ... }` (no scope-clause
/// parens) is the pre-CIP-141 idiom for correlating outer variables into a
/// subquery — a bare WITH of plain, unaliased variable references as the
/// subquery's very first clause. When the subquery's terminal projection
/// re-exposes one of those imported names, the CALL block's output column
/// collides with the already-bound outer variable on the join back
/// (CIP-218 NOTES.md "interaction with existing features" [1]-[3]: Neo4j
/// rejects this with "already declared in outer scope"). `WITH *` alone is
/// also a valid legacy importing form — it pulls in every outer variable,
/// same as naming them all explicitly (interaction [3]) — and is the one
/// predicate that picks both the import set and the seeded output
/// computation.
fn validate_legacy_import_collision(
    csq: &CallSubqueryClause,
    bound_vars: &HashSet<String>,
) -> Result<(), EngineError> {
    let Some(Clause::With(first)) = csq.statement.clauses.first() else {
        return Ok(());
    };
    let with_star_import = first.items.len() == 1 && first.items[0].all;
    let imported: Option<HashSet<String>> = if with_star_import {
        Some(bound_vars.clone())
    } else {
        first
            .items
            .iter()
            .map(|item| match (&item.expr, &item.alias) {
                (Expr::Variable(v), None) if !item.all => Some(v.clone()),
                _ => None,
            })
            .collect()
    };
    let Some(imported) = imported.filter(|s| !s.is_empty()) else {
        return Ok(());
    };
    let exported = if with_star_import {
        compute_subquery_output_vars_seeded(&csq.statement, &imported)
    } else {
        compute_subquery_output_vars(&csq.statement)
    };
    if let Some(exported) = exported {
        if let Some(name) = imported
            .iter()
            .find(|v| exported.contains(*v) && bound_vars.contains(*v))
        {
            return Err(duplicate_outer_variable(name));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_and_validate(query: &str) -> Result<(), String> {
        let stmt = cypherlite_parser::parse(query).map_err(|e| e.to_string())?;
        validate_variable_types(&stmt).map_err(|e| e.to_string())
    }
    #[test]
    fn test_rel_var_used_as_node_same_pattern() {
        // node↔relationship clash (no path) is VariableTypeConflict (Match1[7]).
        let result = parse_and_validate("MATCH ()-[r]-() MATCH (r) RETURN r");
        assert!(result.is_err(), "Expected VariableTypeConflict");
        assert!(result.unwrap_err().contains("VariableTypeConflict"));
    }

    #[test]
    fn test_rel_var_used_as_node_in_same_match() {
        let result = parse_and_validate("MATCH ()-[r]-(r) RETURN r");
        assert!(result.is_err(), "Expected VariableTypeConflict");
        assert!(result.unwrap_err().contains("VariableTypeConflict"));
    }

    #[test]
    fn test_path_var_used_as_node() {
        // Cross-clause: reusing a path variable AS A NODE keys on the new
        // (node) usage -> VariableTypeConflict (Match1[8]).
        let result = parse_and_validate("MATCH r = ()-[]-() MATCH (r) RETURN r");
        assert!(result.is_err(), "Expected VariableTypeConflict");
        assert!(result.unwrap_err().contains("VariableTypeConflict"));
    }

    #[test]
    fn test_path_var_used_as_node_same_pattern() {
        // The path variable `r` binds first, then the later `(r)` node reuses it
        // -> the reuse is a node -> VariableTypeConflict (Match1[10]).
        let result = parse_and_validate("MATCH r = ()-[]-(), (r) RETURN r");
        assert!(result.is_err(), "Expected VariableTypeConflict");
        assert!(result.unwrap_err().contains("VariableTypeConflict"));
    }

    #[test]
    fn test_node_then_path_same_pattern_is_already_bound() {
        // Node `p` binds first (element), then the path variable `p` reuses it
        // -> the reuse is a path -> VariableAlreadyBound (Match6[23]).
        let result = parse_and_validate("MATCH p = (p)-[]-() RETURN p");
        assert!(result.is_err(), "Expected VariableAlreadyBound");
        assert!(result.unwrap_err().contains("VariableAlreadyBound"));
    }

    #[test]
    fn test_scalar_used_as_node() {
        // scalar↔node clash (no path) is VariableTypeConflict.
        let result = parse_and_validate("WITH true AS n MATCH (n) RETURN n");
        assert!(result.is_err(), "Expected VariableTypeConflict");
        assert!(result.unwrap_err().contains("VariableTypeConflict"));
    }

    #[test]
    fn test_valid_node_reuse() {
        // Same variable used as node in multiple patterns — valid
        let result = parse_and_validate("MATCH (n) MATCH (n)-[]-() RETURN n");
        assert!(result.is_ok(), "Expected valid query: {result:?}");
    }

    #[test]
    fn test_call_subquery_legacy_importing_with_return_star_rebinds_outer_var() {
        // CIP-218 NOTES.md "interaction with existing features" [2]: a bare
        // `WITH a` (no scope-clause parens) imports the outer `a`; `RETURN *`
        // re-exposes it, colliding with the already-bound outer `a`.
        let result = parse_and_validate("WITH 1 AS a CALL { WITH a RETURN * } RETURN a");
        assert!(result.is_err(), "Expected VariableAlreadyBound");
        assert!(result.unwrap_err().contains("VariableAlreadyBound"));
    }

    #[test]
    fn test_call_subquery_legacy_importing_with_star_and_extra_item() {
        // CIP-218 interaction [1]: `WITH *, a AS x` re-selects the imported
        // `a` before the terminal `RETURN *` re-exports it.
        let result = parse_and_validate(
            "WITH 1 AS a, 2 AS b CALL { WITH a WITH *, a AS x RETURN * } RETURN *",
        );
        assert!(result.is_err(), "Expected VariableAlreadyBound");
        assert!(result.unwrap_err().contains("VariableAlreadyBound"));
    }

    #[test]
    fn test_call_subquery_new_variable_name_is_fine() {
        // A CALL {} subquery that returns a *new* name (not colliding with
        // anything bound outside it) must not be rejected.
        let result = parse_and_validate("WITH 1 AS a CALL { WITH a RETURN a AS b } RETURN a, b");
        assert!(result.is_ok(), "Expected valid query: {result:?}");
    }

    #[test]
    fn test_call_subquery_without_importing_with_is_unaffected() {
        // No bare importing WITH at the head of the subquery -- the new
        // check must not fire at all.
        let result = parse_and_validate("CALL { MATCH (n) RETURN n } RETURN n");
        assert!(result.is_ok(), "Expected valid query: {result:?}");
    }

    #[test]
    fn test_call_in_transactions_importing_with_write_only_body_is_valid() {
        // ADR-0210 / LABS-264: the importing `WITH n` is not the terminal
        // projection — a trailing write clause ends the body, which exports
        // nothing, so no collision with the outer `n` can exist. This is
        // the standard bulk-delete idiom.
        for query in [
            "MATCH (n) CALL { WITH n DETACH DELETE n } IN TRANSACTIONS OF 10 ROWS",
            "MATCH (n) CALL { WITH n SET n.seen = true } IN TRANSACTIONS",
            "MATCH (n) CALL { WITH n REMOVE n.seen } IN TRANSACTIONS OF 2 ROWS",
            "MATCH (n) CALL { WITH n FOREACH (i IN [1] | SET n.i = i) } IN TRANSACTIONS",
        ] {
            let result = parse_and_validate(query);
            assert!(result.is_ok(), "Expected valid query `{query}`: {result:?}");
        }
    }

    #[test]
    fn test_call_in_transactions_importing_with_reexported_still_rejected() {
        // ADR-0210 keeps the collision rule for bodies whose terminal
        // projection DOES re-export the imported name.
        let result =
            parse_and_validate("MATCH (n) CALL { WITH n RETURN n } IN TRANSACTIONS OF 10 ROWS");
        assert!(result.is_err(), "Expected VariableAlreadyBound");
        assert!(result.unwrap_err().contains("VariableAlreadyBound"));
    }

    #[test]
    fn test_write_then_return_in_subquery_body_still_projects() {
        // A write before the terminal RETURN does not hide the projection:
        // the RETURN still re-exports the imported name and collides.
        let result =
            parse_and_validate("MATCH (n) CALL { WITH n SET n.seen = true RETURN n } RETURN n");
        assert!(result.is_err(), "Expected VariableAlreadyBound");
        assert!(result.unwrap_err().contains("VariableAlreadyBound"));
    }

    #[test]
    fn test_valid_rel_reuse() {
        // Same rel variable in different MATCHes is fine as long as it's always a rel
        let result = parse_and_validate("MATCH ()-[r]-() RETURN r");
        assert!(result.is_ok(), "Expected valid query: {result:?}");
    }

    #[test]
    fn nested_quantified_path_pattern_is_a_typed_error_not_a_panic() {
        // LABS-70: the parser forbids nested QPPs (CIP-28 Rule 4), but a
        // hand-built AST can still reach this validator through the pub
        // entry point — it must return a typed error, never `unreachable!`.
        use cypherlite_parser::{MatchClause, MatchMode, PathQuantifier, QuantifiedPathPattern};
        let nested_qpp = |elements: Vec<PatternElement>| {
            PatternElement::Quantified(QuantifiedPathPattern {
                subpath_variable: None,
                elements,
                where_expr: None,
                quantifier: PathQuantifier { min: 1, max: None },
            })
        };
        let stmt = Statement {
            clauses: vec![Clause::Match(MatchClause {
                optional: false,
                match_mode: MatchMode::DifferentRelationships,
                pattern: vec![PatternPath {
                    variable: None,
                    elements: vec![nested_qpp(vec![nested_qpp(vec![])])],
                    where_expr: None,
                    mode: None,
                    selector: None,
                    is_shortest_path_function: false,
                }],
                where_expr: None,
                search: None,
            })],
            ..Statement::default()
        };
        let err = validate_variable_types(&stmt).unwrap_err();
        assert!(
            err.to_string().contains("NestedQuantification"),
            "wrong error: {err}"
        );
    }

    #[test]
    fn foreach_bodies_share_enclosing_scope_for_write_checks() {
        // LABS-81: a FOREACH body is not a validation blind spot — the same
        // write-pattern rules as top level apply, against the enclosing
        // scope.
        let result = parse_and_validate("FOREACH (x IN [1] | CREATE (a)-[:R]-(b))");
        assert!(
            result.unwrap_err().contains("RequiresDirectedRelationship"),
            "undirected CREATE in a FOREACH body must be rejected"
        );
        let result = parse_and_validate("CREATE (n) FOREACH (x IN [1] | CREATE (n))");
        assert!(
            result.unwrap_err().contains("VariableAlreadyBound"),
            "re-creating an enclosing-scope variable in a FOREACH body must be rejected"
        );
        // Regression: ordinary FOREACH bodies still validate.
        for query in [
            "FOREACH (x IN [1] | CREATE (:L))",
            "MATCH (n) FOREACH (x IN [1] | SET n.p = x)",
            "FOREACH (x IN [1] | FOREACH (y IN [2] | CREATE (:L)))",
        ] {
            let result = parse_and_validate(query);
            assert!(result.is_ok(), "Expected valid query {query}: {result:?}");
        }
    }
}
