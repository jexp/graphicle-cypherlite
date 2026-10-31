//! Output-name computation: the column names each query stage exports.
//!
//! Used by semantic validation (UNION column agreement, subquery scope
//! exports) and by the planner to seed a `NEXT` stage's binding environment.

use cypherlite_core::{Error, Result};
use cypherlite_ir::{Clause, Expr, PathPattern, PatternElement, Query, SingleQuery};

pub(crate) fn query_output_names(query: &Query) -> Result<Vec<String>> {
    query_output_names_with_seed(query, Vec::new())
}

pub(crate) fn query_output_names_with_seed(
    query: &Query,
    seed: Vec<String>,
) -> Result<Vec<String>> {
    let output = parts_output_names(&query.parts, seed)?;
    if let Some(next) = query.next.as_deref() {
        query_output_names_with_seed(next, output)
    } else {
        Ok(output)
    }
}

/// Output names of a single query stage (its `parts`, ignoring `next`).
pub(crate) fn parts_output_names(parts: &[SingleQuery], seed: Vec<String>) -> Result<Vec<String>> {
    stage_output_names(parts.iter().map(|part| part.clauses.as_slice()), seed)
}

/// Output names of one stage given each UNION arm's clauses: the shared
/// body of [`parts_output_names`] and the planned-statement form
/// `plan::plan_output_names` (a `LogicalPlan` holds bare clause lists).
pub(crate) fn stage_output_names<'a>(
    parts: impl IntoIterator<Item = &'a [Clause]>,
    seed: Vec<String>,
) -> Result<Vec<String>> {
    let mut expected: Option<Vec<String>> = None;
    for part in parts {
        let names = part_output_names(part, seed.clone())?;
        if expected.as_ref().is_some_and(|e| e != &names) {
            return Err(Error::semantic("UNION requires same columns"));
        }
        expected = Some(names);
    }
    Ok(expected.unwrap_or_default())
}

fn part_output_names(clauses: &[Clause], mut names: Vec<String>) -> Result<Vec<String>> {
    for clause in clauses {
        match clause {
            Clause::Match { patterns, .. } | Clause::Create { patterns } => {
                for path in patterns {
                    pattern_names(path, &mut names);
                }
            }
            Clause::Merge { pattern, .. } => pattern_names(pattern, &mut names),
            Clause::With { items, .. } => {
                let mut next = if items.iter().any(|item| matches!(item.expr, Expr::Star)) {
                    names
                } else {
                    Vec::new()
                };
                for (index, item) in items.iter().enumerate() {
                    if !matches!(item.expr, Expr::Star) {
                        push_name(&mut next, item.output_name(index));
                    }
                }
                names = next;
            }
            Clause::Return { items, .. } => {
                let mut out = Vec::new();
                for (index, item) in items.iter().enumerate() {
                    if matches!(item.expr, Expr::Star) {
                        for name in &names {
                            push_name(&mut out, name.clone());
                        }
                    } else {
                        push_name(&mut out, item.output_name(index));
                    }
                }
                return Ok(out);
            }
            Clause::Unwind { alias, .. }
            | Clause::LoadCsv {
                variable: alias, ..
            } => {
                push_name(&mut names, alias.clone());
            }
            Clause::Call { yield_items, .. } => {
                for name in yield_items.effective_names() {
                    push_name(&mut names, name.to_owned());
                }
            }
            Clause::CallSubquery {
                query,
                scope,
                in_transactions,
                ..
            } => {
                let (seed, imported): (Vec<String>, std::collections::HashSet<String>) = match scope
                {
                    cypherlite_ir::CallScope::None => (names.clone(), Default::default()),
                    cypherlite_ir::CallScope::Wildcard => {
                        (names.clone(), names.iter().cloned().collect())
                    }
                    cypherlite_ir::CallScope::Vars(vars) => {
                        (vars.clone(), vars.iter().cloned().collect())
                    }
                };
                for name in query_output_names_with_seed(query, seed)? {
                    if !imported.contains(&name) {
                        push_name(&mut names, name);
                    }
                }
                if let Some(status) = in_transactions
                    .as_ref()
                    .and_then(|transaction| transaction.report_status.as_ref())
                {
                    push_name(&mut names, status.clone());
                }
            }
            Clause::Conditional {
                branches,
                else_query,
            } => {
                if let Some((_, query)) = branches.first() {
                    return query_output_names_with_seed(query, names);
                }
                if let Some(query) = else_query {
                    return query_output_names_with_seed(query, names);
                }
                return Ok(Vec::new());
            }
            Clause::Finish => return Ok(Vec::new()),
            Clause::Catalog(command) => {
                for name in catalog_output_names(command) {
                    push_name(&mut names, name);
                }
            }
            Clause::Foreach { .. }
            | Clause::Use { .. }
            | Clause::Delete { .. }
            | Clause::Set { .. }
            | Clause::Remove { .. }
            | Clause::CreateIndex { .. }
            | Clause::DropIndex { .. }
            | Clause::ShowIndexes
            | Clause::CreateConstraint { .. }
            | Clause::DropConstraint { .. } => {}
        }
    }
    if matches!(clauses.last(), Some(Clause::Catalog(_))) {
        Ok(names)
    } else {
        Ok(Vec::new())
    }
}

/// Non-star `RETURN` output names of a query's final stage.
pub(crate) fn explicit_return_names(query: &Query) -> Vec<String> {
    if let Some(next) = query.next.as_deref() {
        return explicit_return_names(next);
    }
    query
        .parts
        .iter()
        .filter_map(|part| part.clauses.last())
        .filter_map(|clause| match clause {
            Clause::Return { items, .. } => Some(items),
            _ => None,
        })
        .flat_map(|items| {
            items
                .iter()
                .enumerate()
                .filter(|(_, item)| !matches!(item.expr, Expr::Star))
                .map(|(index, item)| item.output_name(index))
        })
        .collect()
}

fn push_name(names: &mut Vec<String>, name: String) {
    if !names.iter().any(|existing| existing == &name) {
        names.push(name);
    }
}

/// Names a catalog (`SHOW …`/`TERMINATE …`) clause introduces into scope.
/// The column tables themselves are owned by `cypherlite-front`
/// (`show_columns`), the single source of truth shared with front
/// validation and executor materialization.
pub(crate) fn catalog_output_names(command: &cypherlite_parser::Clause) -> Vec<String> {
    cypherlite_front::show_columns::catalog_output_columns(command)
}

/// The names a path pattern introduces, in the order it spells them: the
/// path variable, each node and relationship variable, then the GPM element
/// variables. The one walk both the `MATCH`/`CREATE` and `MERGE` arms use.
fn pattern_names(path: &PathPattern, names: &mut Vec<String>) {
    if let Some(name) = &path.variable {
        push_name(names, name.clone());
    }
    for node in &path.nodes {
        if let Some(name) = &node.variable {
            push_name(names, name.clone());
        }
    }
    for rel in &path.rels {
        if let Some(name) = &rel.variable {
            push_name(names, name.clone());
        }
    }
    push_gpm_names(names, &path.elements);
}

fn push_gpm_names(names: &mut Vec<String>, elements: &[PatternElement]) {
    for element in elements {
        match element {
            PatternElement::Node(node) => {
                if let Some(name) = &node.variable {
                    push_name(names, name.clone());
                }
            }
            PatternElement::Relationship(rel) => {
                if let Some(name) = &rel.variable {
                    push_name(names, name.clone());
                }
            }
            PatternElement::Quantified(qpp) => {
                if let Some(name) = &qpp.subpath_variable {
                    push_name(names, name.clone());
                }
                push_gpm_names(names, &qpp.elements);
            }
        }
    }
}
