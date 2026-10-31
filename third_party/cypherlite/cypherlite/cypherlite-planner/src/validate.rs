//! Clause-level semantic validation: the per-clause scope and composition
//! rules checked against the binding environment before a plan is built.

use cypherlite_core::{Error, Result};
use cypherlite_ir::{Clause, Expr, Projection, Query, SingleQuery};

use crate::binding::Env;
use crate::expr_check::check_expr;
use crate::names::query_output_names;

pub(crate) fn is_schema_clause(c: &Clause) -> bool {
    matches!(
        c,
        Clause::CreateIndex { .. }
            | Clause::DropIndex { .. }
            | Clause::ShowIndexes
            | Clause::CreateConstraint { .. }
            | Clause::DropConstraint { .. }
    )
}

mod clause;

use clause::*;

pub(crate) fn validate_part_with_env(
    part: &SingleQuery,
    mut env: Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
) -> Result<()> {
    if part.clauses.is_empty() {
        return Err(Error::semantic("empty query part"));
    }
    if part.clauses.iter().any(is_schema_clause) && part.clauses.len() != 1 {
        return Err(Error::syntax(
            "InvalidClauseComposition: CREATE/DROP/SHOW INDEX must be a standalone clause",
        ));
    }
    let mut seen_write = false;
    for (clause_index, c) in part.clauses.iter().enumerate() {
        match c {
            Clause::Catalog(_) => check_catalog_clause(&mut env, is_write_procedure, c)?,
            Clause::Match { .. } => check_match_clause(&mut env, is_write_procedure, c)?,
            Clause::Create { .. } => check_create_clause(&mut env, is_write_procedure, c)?,
            Clause::Merge { .. } => check_merge_clause(&mut env, is_write_procedure, c)?,
            Clause::Call { .. } => check_call_clause(&mut env, is_write_procedure, part, c)?,
            Clause::Delete { .. } => check_delete_clause(&mut env, is_write_procedure, c)?,
            Clause::Set { .. } => check_set_clause(&mut env, is_write_procedure, c)?,
            Clause::Remove { .. } => check_remove_clause(&mut env, is_write_procedure, c)?,
            Clause::With { .. } => check_with_clause(&mut env, is_write_procedure, c)?,
            Clause::Return { .. } => {
                check_return_clause(&mut env, is_write_procedure, clause_index, part, c)?
            }
            Clause::Unwind { .. } => check_unwind_clause(&mut env, is_write_procedure, c)?,
            Clause::CreateIndex { .. } | Clause::DropIndex { .. } | Clause::ShowIndexes => {}
            Clause::Finish => {
                check_finish_clause(&mut env, is_write_procedure, clause_index, part, c)?
            }
            Clause::LoadCsv { .. } => check_load_csv_clause(&mut env, is_write_procedure, c)?,
            Clause::CallSubquery { .. } => check_call_subquery_clause(
                &mut env,
                is_write_procedure,
                seen_write,
                clause_index,
                part,
                c,
            )?,
            Clause::Conditional { .. } => {
                check_conditional_clause(&mut env, is_write_procedure, c)?
            }
            Clause::Foreach { .. } => check_foreach_clause(&mut env, is_write_procedure, c)?,
            Clause::Use { .. } => check_use_clause(&mut env, is_write_procedure, seen_write, c)?,
            Clause::CreateConstraint { .. } | Clause::DropConstraint { .. } => {}
        }
        seen_write |= clause_is_write(c, is_write_procedure)
            && !matches!(
                c,
                Clause::CallSubquery {
                    in_transactions: Some(_),
                    ..
                }
            );
    }
    Ok(())
}

/// Push a synthetic bare-variable `Projection` for each imported name the
/// item list does not already project. Shared by
/// [`query_with_persistent_imports`]'s WITH-item and terminal-RETURN walks.
fn push_missing_imports(items: &mut Vec<Projection>, imported: &[String]) {
    for name in imported {
        if !items
            .iter()
            .any(|item| matches!(&item.expr, Expr::Variable(variable) if variable == name))
        {
            items.push(Projection {
                expr: Expr::Variable(name.clone()),
                alias: None,
                source_text: None,
            });
        }
    }
}

pub(crate) fn query_with_persistent_imports(query: &Query, imported: &[String]) -> Query {
    let mut query = query.clone();
    for part in &mut query.parts {
        for clause in &mut part.clauses {
            if let Clause::With { items, .. } = clause {
                push_missing_imports(items, imported);
            }
        }
        if query.next.is_some() {
            if let Some(Clause::Return { items, .. }) = part.clauses.last_mut() {
                push_missing_imports(items, imported);
            }
        }
    }
    if let Some(next) = &mut query.next {
        **next = query_with_persistent_imports(next, imported);
    }
    query
}

fn validate_scoped_return_star(query: &Query, imported: &[String]) -> Result<()> {
    if imported.is_empty() {
        return Ok(());
    }
    for part in &query.parts {
        let Some(Clause::Return { items, .. }) = part.clauses.last() else {
            continue;
        };
        if !items.iter().any(|item| matches!(item.expr, Expr::Star)) {
            continue;
        }
        let mut has_export = false;
        for clause in &part.clauses {
            match clause {
                Clause::With { items, .. } => {
                    let explicit_names: Vec<String> = items
                        .iter()
                        .enumerate()
                        .filter(|(_, item)| !matches!(item.expr, Expr::Star))
                        .map(|(index, item)| item.output_name(index))
                        .collect();
                    let has_non_imported =
                        explicit_names.iter().any(|name| !imported.contains(name));
                    for (index, item) in items.iter().enumerate() {
                        if matches!(item.expr, Expr::Star) {
                            continue;
                        }
                        let name = item.output_name(index);
                        if imported.contains(&name)
                            && matches!(&item.expr, Expr::Variable(variable) if variable == &name)
                            && !has_non_imported
                        {
                            return Err(Error::syntax(format!("VariableAlreadyBound: `{name}`")));
                        }
                        has_export |= !imported.contains(&name);
                    }
                }
                Clause::Match { patterns, .. } | Clause::Create { patterns } => {
                    has_export |= patterns.iter().any(|path| {
                        path.variable
                            .as_ref()
                            .is_some_and(|name| !imported.contains(name))
                            || path.nodes.iter().any(|node| {
                                node.variable
                                    .as_ref()
                                    .is_some_and(|name| !imported.contains(name))
                            })
                            || path.rels.iter().any(|rel| {
                                rel.variable
                                    .as_ref()
                                    .is_some_and(|name| !imported.contains(name))
                            })
                    });
                }
                Clause::Unwind { alias, .. }
                | Clause::LoadCsv {
                    variable: alias, ..
                } => {
                    has_export |= !imported.contains(alias);
                }
                Clause::CallSubquery { query, .. } => {
                    has_export |= !query_output_names(query)?.is_empty();
                }
                _ => {}
            }
        }
        has_export |= items.iter().enumerate().any(|(index, item)| {
            !matches!(item.expr, Expr::Star) && !imported.contains(&item.output_name(index))
        });
        if !has_export {
            return Err(Error::syntax(
                "NoVariablesInScope: RETURN * has no non-imported variables",
            ));
        }
    }
    Ok(())
}

fn validate_conditional_returns(query: &Query) -> Result<()> {
    for part in &query.parts {
        if let Some(Clause::Return { items, .. }) = part.clauses.last() {
            for item in items {
                if item.alias.is_none() && !matches!(item.expr, Expr::Variable(_) | Expr::Star) {
                    return Err(Error::syntax(
                        "NoExpressionAlias: expressions returned by conditional clauses must use AS",
                    ));
                }
            }
        }
    }
    Ok(())
}

fn check_set_item(
    env: &Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    item: &cypherlite_ir::SetItem,
) -> Result<()> {
    match item {
        cypherlite_ir::SetItem::Property { target, value }
        | cypherlite_ir::SetItem::MergeMap { target, value } => {
            check_expr(env, is_write_procedure, target)?;
            check_expr(env, is_write_procedure, value)?;
        }
        cypherlite_ir::SetItem::Labels { target, .. } => {
            check_expr(env, is_write_procedure, target)?
        }
        cypherlite_ir::SetItem::LabelsFunction { target, value, .. } => {
            check_expr(env, is_write_procedure, target)?;
            check_expr(env, is_write_procedure, value)?;
        }
    }
    Ok(())
}

fn check_remove_item(
    env: &Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    item: &cypherlite_ir::RemoveItem,
) -> Result<()> {
    match item {
        cypherlite_ir::RemoveItem::Property { target }
        | cypherlite_ir::RemoveItem::Labels { target, .. } => {
            check_expr(env, is_write_procedure, target)?;
        }
    }
    Ok(())
}

/// `is_write_procedure` classifies `CALL` targets: the canonical
/// registry-backed predicate, injected by the facade — the procedure
/// registry lives in the executor, which the planner cannot depend on.
/// Every write gate (the `IN TRANSACTIONS` composition rule, the
/// subquery-expression rejection, the §4 namespace gate in
/// `namespaces.rs`) classifies procedures through this one predicate, so
/// no literal procedure name is hardcoded here.
///
/// The clause half is the published per-clause answer
/// ([`crate::clause_mutates_store`]); this function only adds the answers
/// that live below the clause itself — a `CALL { … }` body, a `WHEN`/`ELSE`
/// body, a writes-flagged procedure — which is what the composition rules
/// need. (A nested body is walked here rather than classified outright: a
/// read-only `CALL { … }` body makes no write.)
pub(crate) fn clause_is_write(
    c: &Clause,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
) -> bool {
    crate::clause_mutates_store(c)
        || matches!(c, Clause::CallSubquery { query, .. } if query_contains_write(query, is_write_procedure))
        || matches!(c, Clause::Call { name, .. } if is_write_procedure(name))
        || matches!(c, Clause::Conditional { branches, else_query }
            if branches.iter().any(|(_, query)| query_contains_write(query, is_write_procedure))
                || else_query.as_deref().is_some_and(|q| query_contains_write(q, is_write_procedure)))
}

fn query_contains_write(query: &Query, is_write_procedure: &mut impl FnMut(&str) -> bool) -> bool {
    crate::query_any_clause(query, &mut |c| clause_is_write(c, is_write_procedure))
}

fn query_contains_in_transactions(query: &Query) -> bool {
    crate::query_any_clause(query, &mut |clause| {
        crate::plan::clause_is_in_transactions(clause)
            || matches!(
                clause,
                Clause::CallSubquery { query, .. } if query_contains_in_transactions(query)
            )
    })
}

#[cfg(test)]
mod composition_tests;
