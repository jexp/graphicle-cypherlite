//! Per-clause semantic checks. `validate_part_with_env` dispatches one of these.

use cypherlite_core::{Error, Result};
use cypherlite_ir::{is_agg_expr, Clause, Expr, PatternElement, SingleQuery, UnaryOp, YieldItems};

use crate::agg::{
    check_agg_projections, reject_agg_in_pred, reject_order_by_agg, reject_order_by_unprojected_agg,
};
use crate::binding::{bind_create_path, bind_path, Binding, Env};
use crate::expr_check::{
    check_expr, check_nonneg_int_arg, check_positive_int_arg, check_pred, expr_non_static,
    is_delete_target,
};
use crate::names::{
    catalog_output_names, explicit_return_names, query_output_names, query_output_names_with_seed,
};
use crate::pattern::{
    check_path_exprs, check_write_label_exprs, check_write_rels, collect_qpp_group_names,
    element_has_qpp,
};

use super::{
    check_remove_item, check_set_item, query_contains_in_transactions,
    query_with_persistent_imports, validate_conditional_returns, validate_part_with_env,
    validate_scoped_return_star,
};

pub(super) fn check_catalog_clause(
    env: &mut Env,
    _is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::Catalog(command) = clause else {
        unreachable!("clause arm");
    };
    for name in catalog_output_names(command) {
        env.insert(name, Binding::Value);
    }

    Ok(())
}

pub(super) fn check_match_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::Match {
        patterns,
        where_clause,
        search,
        ..
    } = clause
    else {
        unreachable!("clause arm");
    };
    if let Some(search) = search {
        // Every value expression of the (possibly nested, CIP-278) source
        // tree — the control slots and the CIP-240 decomposed predicates'
        // RHS — checked against the clause-entry scope, the exact row the
        // executor evaluates them on: the RHS may not depend on this
        // clause's own bindings. (Round-4 review, carried over: the
        // CIP-225 SKIP/OFFSET and the accepted-but-unevaluated ANALYZER
        // expression must go through the same variable validation as
        // FOR/LIMIT, or undefined variables hide behind a later
        // NumberOutOfRange / silent acceptance.)
        for expr in search.spec.exprs() {
            check_expr(env, is_write_procedure, expr)?;
        }
    }
    let mut group_names = std::collections::HashMap::new();
    for pattern in patterns {
        collect_qpp_group_names(&pattern.elements, &mut group_names, env)?;
    }
    for p in patterns {
        if p.mode.is_none()
            && p.selector.is_none()
            && p.where_clause.is_none()
            && !p
                .elements
                .iter()
                .any(|element| matches!(element, PatternElement::Quantified(_)))
            && p.elements
                .windows(2)
                .any(|pair| matches!(pair, [PatternElement::Node(_), PatternElement::Node(_)]))
        {
            return Err(Error::syntax(
                "InvalidNodePatternJuxtaposition: adjacent node patterns require a quantified connector",
            ));
        }
        bind_path(env, p, false)?;
    }
    for p in patterns {
        check_path_exprs(env, is_write_procedure, p)?;
    }
    if let Some(search) = search {
        if let Some(score_var) = &search.score_var {
            env.insert(score_var.clone(), Binding::Value);
        }
    }
    if let Some(w) = where_clause {
        let has_qpp = patterns
            .iter()
            .any(|pattern| pattern.elements.iter().any(element_has_qpp));
        if !has_qpp {
            reject_agg_in_pred(w)?;
        }
        if !(has_qpp && is_agg_expr(w)) {
            check_pred(env, is_write_procedure, w)?;
        }
    }

    Ok(())
}

pub(super) fn check_create_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::Create { patterns } = clause else {
        unreachable!("clause arm");
    };
    // Expressions in one updating clause observe only the scope
    // entering that clause, never entities created alongside
    // them (CIP-156).
    let input_env = env.clone();
    for p in patterns {
        check_write_rels(p, true)?;
        check_write_label_exprs(&input_env, is_write_procedure, p)?;
    }
    for p in patterns {
        bind_create_path(env, p)?;
        check_path_exprs(env, is_write_procedure, p)?;
    }

    Ok(())
}

pub(super) fn check_merge_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::Merge {
        pattern,
        on_create,
        on_match,
    } = clause
    else {
        unreachable!("clause arm");
    };
    check_write_rels(pattern, false)?;
    let input_env = env.clone();
    check_write_label_exprs(&input_env, is_write_procedure, pattern)?;
    bind_create_path(env, pattern)?;
    check_path_exprs(env, is_write_procedure, pattern)?;
    for item in on_create.iter().chain(on_match.iter()) {
        check_set_item(env, is_write_procedure, item)?;
    }

    Ok(())
}

pub(super) fn check_call_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    part: &SingleQuery,
    clause: &Clause,
) -> Result<()> {
    let Clause::Call {
        args,
        yield_items,
        where_clause,
        ..
    } = clause
    else {
        unreachable!("clause arm");
    };
    for a in args {
        check_expr(env, is_write_procedure, a)?;
        if is_agg_expr(a) {
            return Err(Error::syntax(
                "InvalidAggregation: aggregations are not allowed in CALL arguments",
            ));
        }
    }
    let star = matches!(yield_items, YieldItems::Star);
    if star && part.clauses.len() > 1 {
        return Err(Error::syntax(
            "UnexpectedSyntax: YIELD * is not allowed in in-query CALL",
        ));
    }
    // Shadow before duplicate, with the same messages as the front's shared
    // yield-binding policy: the front rejects both before lowering, so this
    // is the backstop for a hand-built IR and must not drift from it.
    for name in yield_items.effective_names() {
        if env.contains_key(name) {
            return Err(Error::syntax(format!("VariableAlreadyBound: `{name}`")));
        }
    }
    let mut seen = std::collections::HashSet::new();
    for name in yield_items.effective_names() {
        if !seen.insert(name) {
            return Err(Error::syntax(format!("VariableAlreadyBound: `{name}`")));
        }
        env.insert(name.to_owned(), Binding::Value);
    }
    if let Some(predicate) = where_clause {
        check_pred(env, is_write_procedure, predicate)?;
    }

    Ok(())
}

pub(super) fn check_delete_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::Delete { expressions, .. } = clause else {
        unreachable!("clause arm");
    };
    for e in expressions {
        check_expr(env, is_write_procedure, e)?;
        if matches!(e, Expr::LabelPredicate { .. }) {
            return Err(Error::syntax(
                "InvalidDelete: cannot delete a literal or label expression",
            ));
        }
        if !is_delete_target(e) {
            return Err(Error::syntax(
                "InvalidArgumentType: DELETE requires a node, relationship, or path",
            ));
        }
        if let Expr::Variable(v) = e {
            if matches!(env.get(v), Some(&Binding::Scalar | &Binding::List)) {
                return Err(Error::syntax("InvalidDelete: cannot delete a non-entity"));
            }
        }
    }

    Ok(())
}

pub(super) fn check_set_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::Set { items } = clause else {
        unreachable!("clause arm");
    };
    for item in items {
        check_set_item(env, is_write_procedure, item)?;
    }

    Ok(())
}

pub(super) fn check_remove_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::Remove { items } = clause else {
        unreachable!("clause arm");
    };
    for item in items {
        check_remove_item(env, is_write_procedure, item)?;
    }

    Ok(())
}

pub(super) fn check_with_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::With {
        items,
        group_by,
        order,
        skip,
        limit,
        where_clause,
        origin,
        ..
    } = clause
    else {
        unreachable!("clause arm");
    };
    if *origin == cypherlite_ir::WithOrigin::Let {
        for item in items.iter().filter(|item| !matches!(item.expr, Expr::Star)) {
            let name = item.alias.as_deref().unwrap_or_default();
            if env.contains_key(name) {
                return Err(Error::syntax(format!(
                    "VariableAlreadyBound: LET cannot rebind `{name}`"
                )));
            }
            if is_agg_expr(&item.expr) {
                return Err(Error::syntax(
                    "InvalidAggregation: aggregations are not allowed in LET",
                ));
            }
        }
    }
    let mut seen = std::collections::HashMap::new();
    for (i, item) in items.iter().enumerate() {
        if matches!(item.expr, Expr::Star) {
            continue;
        }
        if item.alias.is_none() && !matches!(item.expr, Expr::Variable(_)) {
            return Err(Error::syntax(
                "NoExpressionAlias: WITH expressions must be aliased",
            ));
        }
        let name = item.output_name(i);
        if seen.contains_key(&name) {
            return Err(Error::syntax(format!(
                "ColumnNameConflict: `{name}` is already defined"
            )));
        }
        seen.insert(name, ());
    }
    for item in items {
        check_expr(env, is_write_procedure, &item.expr)?;
    }
    check_agg_projections(items, group_by.as_deref())?;
    let has_star = items.iter().any(|item| matches!(item.expr, Expr::Star));
    let mut next = if has_star { env.clone() } else { Env::new() };
    for (i, item) in items.iter().enumerate() {
        if matches!(item.expr, Expr::Star) {
            continue;
        }
        let name = item.output_name(i);
        let b = match &item.expr {
            Expr::Variable(v) => env.get(v).copied().unwrap_or(Binding::Value),
            Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::String(_) => Binding::Scalar,
            Expr::List(_) => Binding::List,
            Expr::Map(_) => Binding::Map,
            _ => Binding::Value,
        };
        next.insert(name, b);
    }
    let mut order_env = env.clone();
    order_env.extend(next.clone());
    reject_order_by_agg(items, order)?;
    reject_order_by_unprojected_agg(items, order)?;
    for o in order {
        check_expr(&order_env, is_write_procedure, &o.expr)?;
    }
    if let Some(s) = skip {
        check_expr(&order_env, is_write_procedure, s)?;
        check_nonneg_int_arg(s)?;
    }
    if let Some(l) = limit {
        check_expr(&order_env, is_write_procedure, l)?;
        check_nonneg_int_arg(l)?;
    }
    if let Some(w) = where_clause {
        reject_agg_in_pred(w)?;
        check_pred(&order_env, is_write_procedure, w)?;
    }
    *env = next;

    Ok(())
}

pub(super) fn check_return_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause_index: usize,
    part: &SingleQuery,
    clause: &Clause,
) -> Result<()> {
    let Clause::Return {
        items,
        group_by,
        order,
        skip,
        limit,
        distinct,
        ..
    } = clause
    else {
        unreachable!("clause arm");
    };
    if clause_index + 1 != part.clauses.len() {
        return Err(Error::syntax(
            "InvalidClauseComposition: RETURN must terminate its query",
        ));
    }
    if items.len() == 1 && matches!(items[0].expr, Expr::Star) && env.is_empty() {
        return Err(Error::syntax(
            "NoVariablesInScope: RETURN * requires variables in scope",
        ));
    }
    let mut seen = std::collections::HashMap::new();
    for item in items {
        let Some(name) = item.alias.clone() else {
            continue;
        };
        if seen.contains_key(&name) {
            return Err(Error::syntax(format!(
                "ColumnNameConflict: `{name}` is already defined"
            )));
        }
        seen.insert(name, ());
    }
    for item in items {
        check_expr(env, is_write_procedure, &item.expr)?;
    }
    check_agg_projections(items, group_by.as_deref())?;
    let mut order_env = if *distinct { Env::new() } else { env.clone() };
    for (i, item) in items.iter().enumerate() {
        order_env.insert(item.output_name(i), Binding::Value);
    }
    reject_order_by_agg(items, order)?;
    reject_order_by_unprojected_agg(items, order)?;
    for o in order {
        if *distinct && items.iter().any(|item| item.expr == o.expr) {
            continue;
        }
        check_expr(&order_env, is_write_procedure, &o.expr)?;
    }
    if let Some(s) = skip {
        check_expr(&order_env, is_write_procedure, s)?;
        check_nonneg_int_arg(s)?;
    }
    if let Some(l) = limit {
        check_expr(&order_env, is_write_procedure, l)?;
        check_nonneg_int_arg(l)?;
    }

    Ok(())
}

pub(super) fn check_unwind_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::Unwind { expression, alias } = clause else {
        unreachable!("clause arm");
    };
    check_expr(env, is_write_procedure, expression)?;
    env.insert(alias.clone(), Binding::Value);

    Ok(())
}

pub(super) fn check_finish_clause(
    _env: &mut Env,
    _is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause_index: usize,
    part: &SingleQuery,
    clause: &Clause,
) -> Result<()> {
    let Clause::Finish = clause else {
        unreachable!("clause arm");
    };
    if part.clauses[..clause_index]
        .iter()
        .any(|clause| matches!(clause, Clause::Catalog(_)))
    {
        return Err(Error::syntax(
            "InvalidClauseComposition: FINISH is not allowed after SHOW",
        ));
    }
    if clause_index + 1 != part.clauses.len() {
        return Err(Error::syntax(
            "InvalidClauseComposition: FINISH must terminate its query",
        ));
    }

    Ok(())
}

pub(super) fn check_load_csv_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::LoadCsv { url, variable, .. } = clause else {
        unreachable!("clause arm");
    };
    check_expr(env, is_write_procedure, url)?;
    env.insert(variable.clone(), Binding::Map);

    Ok(())
}

pub(super) fn check_call_subquery_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    seen_write: bool,
    clause_index: usize,
    part: &SingleQuery,
    clause: &Clause,
) -> Result<()> {
    let Clause::CallSubquery {
        query,
        scope,
        in_transactions,
        ..
    } = clause
    else {
        unreachable!("clause arm");
    };
    if query_contains_in_transactions(query) {
        return Err(Error::syntax(
            "NestedCallInTransactions: CALL IN TRANSACTIONS cannot be nested",
        ));
    }
    if in_transactions.is_some() && seen_write {
        return Err(Error::syntax(
            "WriteBeforeCallInTransactions: a write cannot precede CALL IN TRANSACTIONS",
        ));
    }
    let imported = match scope {
        cypherlite_ir::CallScope::Wildcard => env.clone(),
        cypherlite_ir::CallScope::Vars(vars) => {
            let mut imported = Env::new();
            for name in vars {
                let Some(binding) = env.get(name).copied() else {
                    return Err(Error::syntax(format!("UndefinedVariable: `{name}`")));
                };
                if imported.insert(name.clone(), binding).is_some() {
                    return Err(Error::syntax(format!("VariableAlreadyBound: `{name}`")));
                }
            }
            imported
        }
        cypherlite_ir::CallScope::None => env.clone(),
    };
    let persistent_names: Vec<String> = if matches!(scope, cypherlite_ir::CallScope::None) {
        Vec::new()
    } else {
        imported.keys().cloned().collect()
    };
    for part in &query.parts {
        for clause in &part.clauses {
            if let Clause::With { items, .. } = clause {
                for item in items {
                    let Some(alias) = &item.alias else {
                        continue;
                    };
                    if persistent_names.contains(alias)
                        && !matches!(
                            &item.expr,
                            Expr::Variable(variable) if variable == alias
                        )
                    {
                        return Err(Error::syntax(format!("VariableShadowing: `{alias}`")));
                    }
                }
            }
        }
    }
    validate_scoped_return_star(query, &persistent_names)?;
    let scoped_query = query_with_persistent_imports(query, &persistent_names);
    // Validation-only planning of the subquery body; the scoped
    // query is still needed below, so this clone pays what the
    // old borrow-then-clone planner paid internally.
    crate::plan::plan_with_env(
        scoped_query.clone(),
        imported.clone(),
        is_write_procedure,
        // Validation discards this plan. The kept plan is rewritten
        // with the caller's stats in `plan_with_env`.
        &crate::stats::NoPatternStats,
    )?;
    let excluded_imports: std::collections::HashSet<String> = match scope {
        cypherlite_ir::CallScope::None => Default::default(),
        _ => imported.keys().cloned().collect(),
    };
    for name in explicit_return_names(&scoped_query) {
        if env.contains_key(&name) {
            return Err(Error::syntax(format!("VariableAlreadyBound: `{name}`")));
        }
    }
    if let Some(tx) = in_transactions {
        if let Some(n) = &tx.batch_size {
            check_expr(env, is_write_procedure, n)?;
            check_positive_int_arg(n, "batch size")?;
        }
        if let cypherlite_ir::Concurrency::Value(n) = &tx.concurrency {
            check_expr(env, is_write_procedure, n)?;
            check_positive_int_arg(n, "concurrency")?;
            if expr_non_static(n) {
                return Err(Error::syntax(
                    "NonStaticConcurrency: concurrency cannot reference row variables",
                ));
            }
        }
        if let cypherlite_ir::OnError::Retry {
            duration: Some(duration),
            ..
        } = &tx.on_error
        {
            check_expr(env, is_write_procedure, duration)?;
            if expr_non_static(duration) {
                return Err(Error::syntax(
                    "InvalidArgumentExpression: retry duration must be static",
                ));
            }
            match duration {
                Expr::Int(value) if *value >= 0 => {}
                Expr::Float(value) if *value >= 0.0 => {}
                Expr::Unary {
                    op: UnaryOp::Minus, ..
                } => {
                    return Err(Error::syntax(
                        "NumberOutOfRange: retry duration must be non-negative",
                    ))
                }
                Expr::Null | Expr::Bool(_) | Expr::String(_) | Expr::List(_) | Expr::Map(_) => {
                    return Err(Error::syntax(
                        "InvalidArgumentType: retry duration must be numeric",
                    ))
                }
                _ => {}
            }
        }
        if let Some(status) = &tx.report_status {
            if matches!(tx.on_error, cypherlite_ir::OnError::Fail) {
                return Err(Error::syntax(
                    "InvalidReportStatusUsage: REPORT STATUS requires ON ERROR",
                ));
            }
            if matches!(
                tx.on_error,
                cypherlite_ir::OnError::Retry {
                    fallback: cypherlite_ir::RetryFallback::Fail,
                    ..
                }
            ) {
                return Err(Error::syntax(
                    "InvalidClause: REPORT STATUS requires THEN CONTINUE or THEN BREAK",
                ));
            }
            if clause_index + 1 == part.clauses.len() {
                return Err(Error::syntax(
                    "InvalidClauseComposition: REPORT STATUS cannot terminate a query",
                ));
            }
            if env.contains_key(status) {
                return Err(Error::syntax(format!("VariableAlreadyBound: `{status}`")));
            }
            env.insert(status.clone(), Binding::Map);
        }
    }
    for name in query_output_names_with_seed(&scoped_query, imported.keys().cloned().collect())? {
        if excluded_imports.contains(&name) {
            continue;
        }
        if env.contains_key(&name) {
            return Err(Error::syntax(format!("VariableAlreadyBound: `{name}`")));
        }
        env.insert(name, Binding::Value);
    }

    Ok(())
}

pub(super) fn check_conditional_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::Conditional {
        branches,
        else_query,
    } = clause
    else {
        unreachable!("clause arm");
    };
    let mut expected = None;
    for (predicate, query) in branches {
        check_pred(env, is_write_procedure, predicate)?;
        validate_conditional_returns(query)?;
        for part in &query.parts {
            validate_part_with_env(part, env.clone(), is_write_procedure)?;
        }
        let names = query_output_names(query)?;
        if expected.as_ref().is_some_and(|e| e != &names) {
            return Err(Error::syntax(
                "ColumnNameConflict: conditional branches must return the same columns",
            ));
        }
        expected = Some(names);
    }
    if let Some(query) = else_query {
        validate_conditional_returns(query)?;
        for part in &query.parts {
            validate_part_with_env(part, env.clone(), is_write_procedure)?;
        }
        let names = query_output_names(query)?;
        if expected.as_ref().is_some_and(|e| e != &names) {
            return Err(Error::syntax(
                "ColumnNameConflict: conditional branches must return the same columns",
            ));
        }
        expected = Some(names);
    }
    *env = expected
        .unwrap_or_default()
        .into_iter()
        .map(|name| (name, Binding::Value))
        .collect();

    Ok(())
}

pub(super) fn check_foreach_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::Foreach {
        variable,
        list,
        clauses,
    } = clause
    else {
        unreachable!("clause arm");
    };
    check_expr(env, is_write_procedure, list)?;
    let mut inner = env.clone();
    inner.insert(variable.clone(), Binding::Value);
    validate_part_with_env(
        &SingleQuery {
            clauses: clauses.clone(),
        },
        inner,
        is_write_procedure,
    )?;

    Ok(())
}

pub(super) fn check_use_clause(
    env: &mut Env,
    is_write_procedure: &mut impl FnMut(&str) -> bool,
    seen_write: bool,
    clause: &Clause,
) -> Result<()> {
    let Clause::Use { graph } = clause else {
        unreachable!("clause arm");
    };
    if seen_write {
        return Err(Error::syntax(
            "InvalidPlacementOfUseClause: USE must be the first clause",
        ));
    }
    // A static graph name (`USE neo4j`, a dotted catalog name
    // like `composite.constituent` that lowers to a property
    // chain, or a string literal) is a selector, not a variable
    // access, so there is nothing to scope-check.
    // `use_target::static_graph_name` is the one static-name
    // test; it also resolves the target itself.
    if crate::use_target::static_graph_name(graph).is_none() {
        // The composite-database `graph.*` selectors are `USE`
        // targets, not row expressions: the call itself is
        // resolved (and rejected, ADR-0116 §3) by
        // `use_target::resolve_use_target`, but its argument
        // variables are still ordinary scoped references
        // (`USE graph.byName(g)` with `g` out of scope is
        // UndefinedVariable, as CIP-141 expects).
        if let Some(args) = crate::use_target::graph_selector_args(graph) {
            for arg in args {
                check_expr(env, is_write_procedure, arg)?;
            }
        } else {
            check_expr(env, is_write_procedure, graph)?;
        }
    }

    Ok(())
}
