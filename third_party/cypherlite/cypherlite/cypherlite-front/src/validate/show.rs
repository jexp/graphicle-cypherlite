//! `SHOW …`/`TERMINATE …` validation: per-command YIELD column checks
//! (CIP-42/46/79/84/157/184/187), the CIP-123/242/253 composability rules
//! for composing SHOW/TERMINATE clauses with each other and with regular
//! Cypher clauses, and the duplicate-output-name check for the two
//! graph-schema catalogs.
//!
//! The SHOW column tables themselves live in [`crate::show_columns`] (the
//! canonical registry shared with the planner and executor).

use std::collections::HashSet;

use cypherlite_core::Namespace;
use cypherlite_error::{EngineError, ErrorKind};
use cypherlite_parser::{Clause, Expr, Statement, YieldList, YieldMode};

use super::duplicate_variable;
use crate::ast::collect::{check_expr_vars_defined, clause_introduced_names, projected_names};

/// Every explicitly YIELD'd column must be one the command actually
/// projects: the one validity guard the SHOW validator family shares.
fn check_yield_column_names(
    yields: &YieldList,
    full: &[String],
    command: &str,
) -> Result<(), EngineError> {
    if yields.mode() == YieldMode::Star {
        return Ok(());
    }
    for y in yields.names() {
        if !full.iter().any(|c| c.as_str() == y) {
            return Err(EngineError::semantic(
                ErrorKind::InvalidShowColumn,
                format!("'{y}' is not a valid {command} column"),
            ));
        }
    }
    Ok(())
}

/// The one per-command SHOW column policy: every explicit YIELD name must
/// be a real column, and the clause's own WHERE may reference the scope the
/// yield mode defines plus any variable an earlier clause bound.
fn validate_show_columns(
    info: &crate::show_columns::CatalogInfo<'_>,
    outer_scope: &HashSet<String>,
) -> Result<(), EngineError> {
    check_yield_column_names(info.yields, &info.full_columns, info.command)?;
    if let Some(where_expr) = info.where_expr {
        let mut in_scope = show_where_scope(info);
        in_scope.extend(outer_scope.iter().cloned());
        check_expr_vars_defined(where_expr, &in_scope)?;
    }
    Ok(())
}

/// The one SHOW-family WHERE-scope rule, derived from the descriptor's one
/// projection: `YIELD *` makes the full column set referenceable, and every
/// other mode both each source (raw) name and its output (alias-resolved)
/// name (`show::where_probe` makes both resolvable at runtime). Composed-
/// SHOW outer bindings are the caller's addition.
fn show_where_scope(info: &crate::show_columns::CatalogInfo<'_>) -> HashSet<String> {
    match info.projection() {
        crate::show_columns::CatalogProjection::Star => info.full_columns.iter().cloned().collect(),
        crate::show_columns::CatalogProjection::Columns(pairs) => pairs
            .into_iter()
            .flat_map(|(output, source)| [output, source])
            .collect(),
    }
}

/// The Cypher-5-withheld `SHOW CONSTRAINTS` columns and their authored
/// diagnostics: `classification` (CIP-186 graph types) and `enforcedLabel`
/// (CIP-187) only exist under Cypher 25. One table, one case-insensitive
/// lookup, one error construction.
pub(super) fn cypher5_withheld_constraint_column(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "classification" => Some(
            "UnknownColumn: 'classification' is not an available SHOW CONSTRAINTS column in Cypher 5",
        ),
        "enforcedlabel" => {
            Some("InvalidColumnName: Invalid input 'enforcedLabel' for column name")
        }
        _ => None,
    }
}

/// Runs every SHOW-clause-specific column-name check: `SHOW DATABASES`'s
/// CIP-42 checks, and `SHOW FUNCTIONS`/`SHOW PROCEDURES`/`SHOW SETTINGS`'s
/// CIP-84/CIP-157 default/Full-only column split. The remaining CIP-68 SHOW
/// commands have no non-default columns yet, so their `show_*_columns` full
/// sets already double as valid-YIELD-name whitelists with nothing further
/// to enforce.
pub fn validate_show_column_names(stmt: &Statement) -> Result<(), EngineError> {
    // CIP-123/CIP-242: a composed SHOW clause's own `WHERE` may reference a
    // variable bound by an earlier clause in the same statement.
    let mut outer_scope: HashSet<String> = HashSet::new();
    for clause in &stmt.clauses {
        if let Some(info) = crate::show_columns::catalog_info(clause) {
            if info.validate_columns {
                validate_show_columns(&info, &outer_scope)?;
            }
        }
        // A WITH replaces the scope; every other clause extends it.
        if let Clause::With(wc) = clause {
            outer_scope = projected_names(&wc.items, &outer_scope);
        } else {
            outer_scope.extend(clause_introduced_names(clause));
        }
    }
    Ok(())
}

/// True when a `USE` clause's graph expression is the bare literal name
/// `system` (`USE system`) -- the one spelling CIP-123 errors[8]/[9] test
/// the `SHOW INDEXES`/`SHOW CONSTRAINTS`-on-`system` restriction with.
/// Namespace names match case-insensitively (ADR-0116 §1), so `USE SYSTEM`
/// composes under the same rule now that `USE` routes (ADR-0116 §3).
/// Doesn't attempt to resolve `graph.byName(...)`/parameterized forms to
/// "system" -- those aren't part of this rule's tested surface.
fn use_targets_system(uc: &cypherlite_parser::UseClause) -> bool {
    matches!(&uc.graph_expr, Expr::Variable(name) if Namespace::parse(name) == Some(Namespace::System))
}

/// True for every SHOW/TERMINATE clause kind CIP-123/CIP-242/CIP-253's
/// composability rules apply to.
fn is_show_or_terminate(clause: &Clause) -> bool {
    // The CIP-123/242/253 composability family: the descriptor owns which
    // catalog commands belong to it (the graph-type commands keep their own
    // CIP-184 rules and error ordering).
    crate::show_columns::catalog_info(clause).is_some_and(|info| info.composable)
}

/// True for the two graph-schema catalogs (`SHOW INDEXES`/`SHOW
/// CONSTRAINTS`): the commands that belong to an individual graph database
/// rather than the `system` database, which CIP-123's composability rule
/// singles out. The one home of that two-command classification.
fn is_graph_schema_catalog(clause: &Clause) -> bool {
    crate::show_columns::catalog_info(clause).is_some_and(|info| info.graph_schema_catalog)
}

/// The typed yield mode of a SHOW/TERMINATE clause, uniformly across every
/// variant; `None` for anything else. The mode (not a derived bool pair) is
/// the classification the composability rule branches on.
fn show_yield_state(clause: &Clause) -> Option<YieldMode> {
    crate::show_columns::catalog_info(clause).map(|info| info.yields.mode())
}

/// CIP-123/CIP-242/CIP-253: structural rules for composing SHOW/TERMINATE
/// clauses with each other and with regular Cypher clauses. A SHOW/
/// TERMINATE clause keeps its pre-CIP-123 baseline freedom -- YIELD and
/// RETURN both optional, `YIELD *` allowed -- only in the single canonical
/// standalone shape: the clause alone, or the clause immediately followed
/// by its own trailing RETURN and nothing else (CIP-123
/// `composability-boundaries.feature` [7]/[8]). Any other arrangement -- a
/// preceding clause, an intervening clause, more than one SHOW/TERMINATE
/// clause, or no trailing RETURN at all -- is "composed": every SHOW/
/// TERMINATE clause in it must carry an explicit, non-`*` YIELD, a SHOW/
/// TERMINATE clause can never be the statement's own last clause (its
/// yielded columns would otherwise vanish -- a *different* clause, such as
/// a trailing write with no RETURN, is still a perfectly ordinary terminal
/// clause), and a RETURN clause, if present at all, must be the last one
/// (rejected outright otherwise, whether or not it's the statement's only
/// RETURN).
///
/// Applied to `stmt`'s own clause list and, independently, to each arm of
/// its `.union` chain (each arm's clause list stands on its own for this
/// check). Deliberately does *not* recurse into nested `CALL {}`/`EXISTS
/// {}`/`COUNT {}` subquery bodies -- a SHOW/TERMINATE clause that is the
/// sole clause of such a body (e.g. `CALL { SHOW CONSTRAINTS }`) is exactly
/// as unconstrained as any other bare, non-composed SHOW/TERMINATE command,
/// so leaving it unchecked here matches the canonical-standalone case
/// anyway.
pub fn validate_show_composability(stmt: &Statement) -> Result<(), EngineError> {
    validate_show_composability_clauses(&stmt.clauses, stmt.brace_wrapped)?;
    let mut current = stmt;
    while let Some(union) = &current.union {
        validate_show_composability_clauses(
            &union.statement.clauses,
            union.statement.brace_wrapped,
        )?;
        current = &union.statement;
    }
    Ok(())
}

/// CIP-184 syntax[12]: unlike the older CIP-68/CIP-123 SHOW/TERMINATE
/// commands (`validate_show_composability_clauses`'s "canonical standalone"
/// leniency, which allows a bare `SHOW X RETURN col` with no `YIELD` at
/// all), `SHOW CURRENT GRAPH TYPE`'s own grammar always requires an
/// explicit `YIELD` before a `RETURN` -- verified live: `SHOW CURRENT GRAPH
/// TYPE RETURN specification` raises `MissingYield` even though it's the
/// single-clause-plus-trailing-RETURN shape every other SHOW command
/// tolerates without YIELD. Applied to `stmt`'s own clause list and each
/// union arm, same recursion shape as `validate_show_composability`.
pub fn validate_show_current_graph_type_requires_yield(
    stmt: &Statement,
) -> Result<(), EngineError> {
    validate_show_current_graph_type_requires_yield_clauses(&stmt.clauses)?;
    let mut current = stmt;
    while let Some(union) = &current.union {
        validate_show_current_graph_type_requires_yield_clauses(&union.statement.clauses)?;
        current = &union.statement;
    }
    Ok(())
}

fn validate_show_current_graph_type_requires_yield_clauses(
    clauses: &[Clause],
) -> Result<(), EngineError> {
    if !clauses.iter().any(|c| matches!(c, Clause::Return(_))) {
        return Ok(());
    }
    for clause in clauses {
        if let Clause::ShowCurrentGraphType(scgt) = clause {
            if !scgt.yields.present() {
                return Err(EngineError::semantic(
                    ErrorKind::InvalidClauseComposition,
                    "MissingYield: 22N93 an explicit YIELD is \
                     required before RETURN"
                        .to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_show_composability_clauses(
    clauses: &[Clause],
    brace_wrapped: bool,
) -> Result<(), EngineError> {
    if !clauses.iter().any(is_show_or_terminate) {
        return Ok(());
    }

    // CIP-123 errors[8]/[9]: `SHOW INDEXES`/`SHOW CONSTRAINTS` -- schema
    // catalogs that belong to an individual graph database, not the
    // `system` database -- are rejected outright when composed with
    // another SHOW/TERMINATE command on `system` (verified live: `USE
    // system SHOW PROCEDURES YIELD ... SHOW FUNCTIONS YIELD ... SHOW
    // SETTINGS YIELD ...` succeeds there, so the restriction is specific to
    // the two graph-schema catalogs, not composed SHOW on `system` in
    // general).
    let show_count = clauses.iter().filter(|c| is_show_or_terminate(c)).count();
    if show_count > 1 {
        let all_use_before_first_show = clauses
            .iter()
            .take_while(|c| !is_show_or_terminate(c))
            .all(|c| matches!(c, Clause::Use(_)));
        let targets_system = all_use_before_first_show
            && clauses
                .iter()
                .any(|c| matches!(c, Clause::Use(uc) if use_targets_system(uc)));
        if targets_system && clauses.iter().any(is_graph_schema_catalog) {
            return Err(EngineError::semantic(
                ErrorKind::InvalidClauseComposition,
                "NotAllowedOnSystemDatabase: SHOW INDEXES/SHOW \
                 CONSTRAINTS cannot be composed with another SHOW/TERMINATE command on the \
                 system database"
                    .to_string(),
            ));
        }
    }

    // CIP-123 errors[7]: once any SHOW/TERMINATE clause is present, a
    // RETURN clause may only be the statement's very last clause -- it
    // can't be followed by a further composed command (or anything else).
    if let Some(return_idx) = clauses.iter().position(|c| matches!(c, Clause::Return(_))) {
        if return_idx != clauses.len() - 1 {
            return Err(EngineError::semantic(
                ErrorKind::InvalidClauseComposition,
                "ReturnNotLast: RETURN is only allowed as the last \
                 clause when composing a SHOW/TERMINATE command"
                    .to_string(),
            ));
        }
    }

    // CIP-242 errors[1]: explicit top-level braces (`{ SHOW INDEXES }`) mark
    // the body as a `SingleQuery` known to be composed/embedded per CIP-146,
    // even when its clause list alone would otherwise qualify for the
    // lenient "canonical standalone" leniency below -- so a brace-wrapped
    // bare SHOW/TERMINATE still needs its own explicit YIELD.
    let canonical_standalone = !brace_wrapped
        && (clauses.len() == 1
            || (clauses.len() == 2
                && is_show_or_terminate(&clauses[0])
                && matches!(&clauses[1], Clause::Return(_))));
    if canonical_standalone {
        return Ok(());
    }

    // Composed: every SHOW/TERMINATE clause needs its own explicit,
    // non-`*` YIELD (CIP-123 errors[1]/[2]/[4]/[5]/[11], CIP-242
    // errors[1]/[2]/[3]).
    for clause in clauses {
        match show_yield_state(clause) {
            Some(YieldMode::Default) => {
                return Err(EngineError::semantic(
                    ErrorKind::InvalidClauseComposition,
                    "MissingYield: 22N93 an explicit YIELD is \
                     required when composing a SHOW/TERMINATE command"
                        .to_string(),
                ));
            }
            Some(YieldMode::Star) => {
                return Err(EngineError::semantic(
                    ErrorKind::InvalidClauseComposition,
                    "InvalidYieldStar: 22N94 YIELD * is not allowed \
                     when composing a SHOW/TERMINATE command"
                        .to_string(),
                ));
            }
            Some(YieldMode::Named) | None => {}
        }
    }

    // Composed: a SHOW/TERMINATE clause can never be the statement's own
    // final clause -- its yielded columns would otherwise just vanish
    // (CIP-123 errors[3]: `SHOW PROCEDURES YIELD ... SHOW FUNCTIONS YIELD
    // ...` with no trailing RETURN; CIP-242 errors[4]: `MATCH (n) SHOW
    // INDEXES YIELD name` ending on a bare YIELD). Any *other* clause is a
    // legitimate terminal clause on its own ordinary terms -- a trailing
    // write clause with no RETURN (CIP-242 comp[9]: `SHOW INDEXES YIELD
    // ... CREATE (...)`) is exactly as valid after a composed SHOW as it
    // is anywhere else in Cypher, so this deliberately does *not* require
    // RETURN unconditionally, only that a SHOW/TERMINATE clause never has
    // the last word.
    if is_show_or_terminate(clauses.last().expect("checked non-empty above")) {
        return Err(EngineError::semantic(
            ErrorKind::InvalidClauseComposition,
            "MissingReturn: 42N71 a composed SHOW/TERMINATE command \
             cannot be the statement's last clause without a trailing RETURN"
                .to_string(),
        ));
    }

    Ok(())
}

/// Duplicate-output-name check for the two graph-schema catalog commands
/// (`SHOW CONSTRAINTS` / `SHOW INDEXES`): no output column name -- an
/// explicit YIELD name or its non-empty `AS` alias -- may repeat, whether
/// within one command's YIELD list or across the commands of one statement
/// (a name collision would silently drop one of the row's columns). This
/// runs on every statement the lowering sees, including nested subquery
/// bodies: unlike `scope::validate_variable_types` (which enforces the same
/// rule at top level as part of its full scope walk), it needs no outer
/// scope, so it is safe to apply to a nested body whose enclosing
/// bindings are not visible here.
pub fn validate_show_output_uniqueness(clauses: &[Clause]) -> Result<(), EngineError> {
    let mut command_outputs = std::collections::HashSet::new();
    for clause in clauses {
        let yields = crate::show_columns::catalog_info(clause)
            .filter(|info| info.graph_schema_catalog)
            .map(|info| info.yields);
        if let Some(yields) = yields {
            for (_, _, output) in yields.pairs() {
                if !command_outputs.insert(output.to_owned()) {
                    return Err(duplicate_variable(output));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Through the public lowering path: the pass is wired for top-level
    /// and nested statements alike.
    fn lower(query: &str) -> Result<(), String> {
        crate::parse_and_lower(query)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    #[test]
    fn duplicate_show_output_names_are_rejected() {
        for query in [
            // Duplicated within one YIELD list, plain and via `AS` alias.
            "SHOW CONSTRAINTS YIELD name, name RETURN name",
            "SHOW INDEXES YIELD name, owningConstraint AS name RETURN name",
            // Duplicated across the two graph-schema catalogs of one
            // statement.
            "SHOW CONSTRAINTS YIELD name SHOW INDEXES YIELD name RETURN name",
            // Nested bodies are checked too (no outer scope needed).
            "CALL { SHOW CONSTRAINTS YIELD name, name RETURN name } RETURN name",
        ] {
            let err = lower(query).expect_err(&format!("`{query}` must be rejected"));
            assert!(err.contains("VariableAlreadyBound"), "{query}: {err}");
        }
    }

    #[test]
    fn distinct_show_output_names_are_accepted() {
        assert!(lower("SHOW CONSTRAINTS YIELD name, id RETURN name").is_ok());
        assert!(
            lower("SHOW CONSTRAINTS YIELD name SHOW INDEXES YIELD name AS ix RETURN ix").is_ok()
        );
        // Other SHOW commands are not this pass's surface (the top-level
        // scope tracker covers them).
        assert!(lower("SHOW FUNCTIONS YIELD name RETURN name").is_ok());
    }
}
