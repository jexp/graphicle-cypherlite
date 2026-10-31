use super::expr::*;
use super::*;

pub(super) fn lower_clause(c: &cl::Clause) -> Result<mdb::Clause> {
    Ok(match c {
        cl::Clause::Match(m) => mdb::Clause::Match {
            optional: m.optional,
            match_mode: match m.match_mode {
                cl::MatchMode::DifferentRelationships => mdb::MatchMode::DifferentRelationships,
                cl::MatchMode::RepeatableElements => mdb::MatchMode::RepeatableElements,
            },
            patterns: lower_paths(&m.pattern)?,
            where_clause: m.where_expr.as_ref().map(lower_expr).transpose()?,
            search: lower_boxed(m.search.as_ref(), lower_search_clause)?,
        },
        cl::Clause::Create(cr) => mdb::Clause::Create {
            patterns: lower_paths(&cr.pattern)?,
        },
        cl::Clause::Merge(mg) => mdb::Clause::Merge {
            pattern: lower_path(&mg.pattern)?,
            on_create: lower_set_items(&mg.on_create)?,
            on_match: lower_set_items(&mg.on_match)?,
        },
        cl::Clause::Delete(d) => mdb::Clause::Delete {
            detach: d.detach,
            expressions: map_exprs(&d.expressions)?,
        },
        cl::Clause::Set(s) => mdb::Clause::Set {
            items: lower_set_items(&s.items)?,
        },
        cl::Clause::Remove(r) => mdb::Clause::Remove {
            items: lower_remove_items(&r.items)?,
        },
        cl::Clause::Return(r) => mdb::Clause::Return {
            distinct: r.distinct,
            group_by: lower_group_by(&r.group_by, &r.items)?,
            items: lower_projections(&r.items)?,
            order: lower_order(&r.order_by)?,
            skip: r.skip.as_ref().map(lower_expr).transpose()?,
            limit: r.limit.as_ref().map(lower_expr).transpose()?,
        },
        cl::Clause::With(w) => mdb::Clause::With {
            distinct: w.distinct,
            group_by: lower_group_by(&w.group_by, &w.items)?,
            items: lower_projections(&w.items)?,
            order: lower_order(&w.order_by)?,
            skip: w.skip.as_ref().map(lower_expr).transpose()?,
            limit: w.limit.as_ref().map(lower_expr).transpose()?,
            where_clause: w.where_expr.as_ref().map(lower_expr).transpose()?,
            origin: match w.origin {
                cl::WithOrigin::Explicit => mdb::WithOrigin::Explicit,
                cl::WithOrigin::Let => mdb::WithOrigin::Let,
                cl::WithOrigin::Filter => mdb::WithOrigin::Filter,
            },
        },
        cl::Clause::Unwind(u) => mdb::Clause::Unwind {
            expression: lower_expr(&u.expr)?,
            alias: u.alias.clone(),
        },
        cl::Clause::CallProcedure(p) => mdb::Clause::Call {
            name: p.procedure.clone(),
            args: map_exprs(&p.args)?,
            explicit: p.explicit_args,
            yield_items: lower_yield_items(p),
            optional: p.optional,
            where_clause: p.where_expr.as_ref().map(lower_expr).transpose()?,
        },
        // Schema DDL and the catalog/SHOW family lower as one unit in
        // `lower_schema_ddl`: the `CREATE/DROP INDEX` shapes the IR models
        // natively, with every other schema/catalog spelling riding to
        // the planner unchanged as the catalog passthrough.
        cl::Clause::CreateIndex(_)
        | cl::Clause::DropIndex(_)
        | cl::Clause::CreateConstraint(_)
        | cl::Clause::DropConstraint(_)
        | cl::Clause::ShowIndexes(_)
        | cl::Clause::ShowFunctions(_)
        | cl::Clause::ShowConstraints(_)
        | cl::Clause::ShowDatabases(_)
        | cl::Clause::ShowRoles(_)
        | cl::Clause::ShowServers(_)
        | cl::Clause::ShowUsers(_)
        | cl::Clause::ShowProcedures(_)
        | cl::Clause::ShowSettings(_)
        | cl::Clause::ShowTransactions(_)
        | cl::Clause::TerminateTransactions(_)
        | cl::Clause::ShowGraphType(_)
        | cl::Clause::ShowCurrentGraphType(_)
        | cl::Clause::CreateGraphType(_)
        | cl::Clause::AlterCurrentGraphType(_)
        | cl::Clause::AlterDatabase(_) => lower_schema_ddl(c)?,
        cl::Clause::Finish => mdb::Clause::Finish,
        cl::Clause::LoadCSV(l) => mdb::Clause::LoadCsv {
            with_headers: l.with_headers,
            url: lower_expr(&l.source)?,
            variable: l.alias.clone(),
            field_terminator: l.field_terminator.clone(),
        },
        cl::Clause::CallSubquery(cs) => mdb::Clause::CallSubquery {
            query: Box::new(lower_statement_impl(&cs.statement)?),
            optional: cs.optional,
            scope: match &cs.scope {
                cl::CallScope::None => mdb::CallScope::None,
                cl::CallScope::Wildcard => mdb::CallScope::Wildcard,
                cl::CallScope::Vars(vars) => mdb::CallScope::Vars(vars.clone()),
            },
            in_transactions: cs
                .in_transactions
                .as_ref()
                .map(lower_in_transactions)
                .transpose()?,
        },
        cl::Clause::StandaloneOrderBy(_) => {
            // Never reached: lower_clauses intercepts the paging clause
            // and lowers it as one horizon-preserving WITH * per stage
            // (lower/mod.rs) — that is the one implementation.
            unreachable!("lower_clauses intercepts StandaloneOrderBy before lower_clause")
        }
        cl::Clause::Foreach(f) => mdb::Clause::Foreach {
            variable: f.variable.clone(),
            list: lower_expr(&f.list_expr)?,
            clauses: lower_clauses(&f.clauses)?,
        },
        cl::Clause::Conditional(c) => mdb::Clause::Conditional {
            branches: c
                .branches
                .iter()
                .map(|b| Ok((lower_expr(&b.predicate)?, lower_statement_impl(&b.body)?)))
                .collect::<Result<Vec<_>>>()?,
            else_query: lower_boxed(c.else_body.as_ref(), lower_statement_impl)?,
        },
        cl::Clause::Use(use_clause) => mdb::Clause::Use {
            graph: lower_expr(&use_clause.graph_expr)?,
        },
    })
}

/// A `CALL` clause's yield tail, lowered from the one `YieldList` model
/// into the IR's typed `YieldItems`: no `YIELD`, `YIELD *`, or the explicit
/// alias-preserving pairs. No sentinel name crosses the boundary.
fn lower_yield_items(p: &cl::CallProcedureClause) -> mdb::YieldItems {
    match p.yields.mode() {
        cypherlite_parser::YieldMode::Star => mdb::YieldItems::Star,
        cypherlite_parser::YieldMode::Default => mdb::YieldItems::Default,
        cypherlite_parser::YieldMode::Named => mdb::YieldItems::Named(
            p.yields
                .pairs()
                .map(|(name, alias, _)| (name.to_owned(), alias.map(str::to_owned)))
                .collect(),
        ),
    }
}

/// Schema DDL and catalog clauses. `CREATE INDEX`/`DROP INDEX` have
/// native IR forms, but only for the shapes the support matrix covers:
/// the single-label, single-property, no-options range index (named with
/// a literal name or unnamed), and the drop targets split between the
/// legacy `ON :Label(property)` spelling and a literal index name. Every
/// other schema/catalog spelling — an unsupported index shape, or any of
/// the constraint/SHOW/administration clauses — passes through to the
/// planner unchanged as the catalog fallback, which routes it to the
/// catalog layer.
fn lower_schema_ddl(c: &cl::Clause) -> Result<mdb::Clause> {
    Ok(match c {
        cl::Clause::CreateIndex(ix)
            if ix.entity_type == cl::SchemaEntityType::Node
                && ix.labels.len() == 1
                && ix.properties.len() == 1
                && ix.with_properties.is_empty()
                && ix.options.is_empty()
                && ix.options_param.is_none()
                && matches!(ix.kind, cl::IndexKind::Range)
                && ix
                    .name
                    .as_ref()
                    .is_none_or(|n| matches!(n, cl::SchemaName::Literal(_))) =>
        {
            mdb::Clause::CreateIndex {
                name: schema_name_opt(&ix.name)?,
                if_not_exists: ix.if_not_exists,
                label: ix.labels[0].clone(),
                property: ix.properties[0].clone(),
            }
        }
        cl::Clause::DropIndex(ix)
            if ix.legacy_target.is_some() || matches!(ix.name, cl::SchemaName::Literal(_)) =>
        {
            mdb::Clause::DropIndex {
                if_exists: ix.if_exists,
                target: match &ix.legacy_target {
                    Some((label, property)) => mdb::DropIndexTarget::Schema {
                        label: label.clone(),
                        property: property.clone(),
                    },
                    None => mdb::DropIndexTarget::Name(schema_name(&ix.name)?),
                },
            }
        }
        _ => mdb::Clause::Catalog(Box::new(c.clone())),
    })
}

pub(super) fn schema_name(n: &cl::SchemaName) -> Result<String> {
    match n {
        cl::SchemaName::Literal(s) => Ok(s.clone()),
        cl::SchemaName::Parameter(_) => Err(nyi("schema name parameter")),
    }
}

pub(super) fn lower_in_transactions(t: &cl::InTransactionsClause) -> Result<mdb::InTransactions> {
    Ok(mdb::InTransactions {
        concurrency: match &t.concurrency {
            cl::Concurrency::Sequential => mdb::Concurrency::Sequential,
            cl::Concurrency::DefaultUnspecified => mdb::Concurrency::Default,
            cl::Concurrency::Value(expr) => mdb::Concurrency::Value(lower_expr(expr)?),
        },
        batch_size: t.batch_size.as_ref().map(lower_expr).transpose()?,
        on_error: match &t.on_error {
            cl::OnErrorMode::Fail => mdb::OnError::Fail,
            cl::OnErrorMode::Continue => mdb::OnError::Continue,
            cl::OnErrorMode::Break => mdb::OnError::Break,
            cl::OnErrorMode::Retry { duration, fallback } => mdb::OnError::Retry {
                duration: duration.as_ref().map(lower_expr).transpose()?,
                fallback: match fallback {
                    cl::RetryFallback::Fail => mdb::RetryFallback::Fail,
                    cl::RetryFallback::Continue => mdb::RetryFallback::Continue,
                    cl::RetryFallback::Break => mdb::RetryFallback::Break,
                },
            },
        },
        report_status: t.report_status.clone(),
    })
}

pub(super) fn schema_name_opt(n: &Option<cl::SchemaName>) -> Result<Option<String>> {
    n.as_ref().map(schema_name).transpose()
}

pub(super) fn lower_projections(items: &[cl::ReturnItem]) -> Result<Vec<mdb::Projection>> {
    items
        .iter()
        .map(|i| {
            let expr = if i.all {
                mdb::Expr::Star
            } else {
                lower_expr(&i.expr)?
            };
            Ok(mdb::Projection {
                source_text: column_label(&i.expr, &i.source_text, &expr),
                expr,
                alias: i.alias.clone(),
            })
        })
        .collect()
}

/// Column label for an unaliased projection: the parser's verbatim source
/// text, except for aggregate aliases (`percentile_cont`, `stdev_samp`,
/// …) whose column name follows the CIP-128 canonical function name
/// resolved during expression lowering. Doing the rewrite once at
/// lowering time keeps [`cypherlite_ir::Projection::output_name`] a plain
/// field read.
fn column_label(
    parsed: &cl::Expr,
    source_text: &Option<String>,
    lowered: &mdb::Expr,
) -> Option<String> {
    let src = source_text.as_ref()?;
    // CIP-128: aggregate aliases execute under their canonical name
    // (`percentile_cont` → `percentileCont`); the column name follows.
    // Scoped to the function-call arm: `RETURN (1 + 2)` keeps its parens
    // in the column name, matching Neo4j. `Expr::Paren` is unwrapped by
    // `lower_expr`, so unwrap it in the parsed AST too — and strip the
    // matching outer parens from the source text in lockstep — before
    // checking for the function call under the parens.
    if let mdb::Expr::Function { name, .. } = lowered {
        if let Some(args_src) = unwrapped_call_source(parsed, src) {
            return Some(format!("{name}{args_src}"));
        }
    }
    Some(src.clone())
}

/// If `parsed` is a (possibly parenthesized) function call, returns the
/// argument-list substring of `src` starting at the call's `(`. Advances
/// the AST and text views in lockstep so a parse/text mismatch degrades
/// to `None` rather than desynchronizing.
fn unwrapped_call_source<'a>(parsed: &cl::Expr, src: &'a str) -> Option<&'a str> {
    let mut inner = parsed;
    let mut text = src;
    while let cl::Expr::Paren(p) = inner {
        let stripped = text
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .map(str::trim)?;
        inner = p;
        text = stripped;
    }
    if !matches!(inner, cl::Expr::FunctionCall(_)) {
        return None;
    }
    text.find('(').map(|open| &text[open..])
}

pub(super) fn lower_order(items: &[cl::OrderByItem]) -> Result<Vec<mdb::OrderItem>> {
    items
        .iter()
        .map(|o| {
            Ok(mdb::OrderItem {
                expr: lower_expr(&o.expr)?,
                ascending: !o.descending,
            })
        })
        .collect()
}

pub(super) fn lower_set_items(items: &[cl::SetItem]) -> Result<Vec<mdb::SetItem>> {
    items
        .iter()
        .map(|s| {
            if let Some((target, op)) = &s.labels_function {
                return Ok(mdb::SetItem::LabelsFunction {
                    target: lower_expr(target)?,
                    value: s
                        .value
                        .as_ref()
                        .map(lower_expr)
                        .transpose()?
                        .unwrap_or(mdb::Expr::Null),
                    op: match op {
                        cl::LabelsFunctionOp::Overwrite => mdb::LabelsFunctionOp::Overwrite,
                        cl::LabelsFunctionOp::Extend => mdb::LabelsFunctionOp::Extend,
                        cl::LabelsFunctionOp::Remove => mdb::LabelsFunctionOp::Remove,
                    },
                });
            }
            if !s.labels.is_empty() {
                let labels = s
                    .labels
                    .iter()
                    .map(lower_label_expr)
                    .collect::<Result<_>>()?;
                let target = s
                    .variable
                    .as_ref()
                    .map(|v| mdb::Expr::Variable(v.clone()))
                    .ok_or_else(|| nyi("SET labels without variable"))?;
                return Ok(mdb::SetItem::Labels { target, labels });
            }
            let target = if let Some(p) = &s.property {
                if let Some(key) = &p.key_expr {
                    mdb::Expr::DynamicProperty {
                        expr: Box::new(lower_expr(&p.expr)?),
                        key: Box::new(lower_expr(key)?),
                    }
                } else {
                    mdb::Expr::Property {
                        expr: Box::new(lower_expr(&p.expr)?),
                        key: p.property.clone(),
                    }
                }
            } else if let Some(v) = &s.variable {
                mdb::Expr::Variable(v.clone())
            } else {
                return Err(nyi("SET item shape"));
            };
            let value = s
                .value
                .as_ref()
                .map(lower_expr)
                .transpose()?
                .unwrap_or(mdb::Expr::Null);
            if s.merge {
                Ok(mdb::SetItem::MergeMap { target, value })
            } else {
                Ok(mdb::SetItem::Property { target, value })
            }
        })
        .collect()
}

pub(super) fn lower_remove_items(items: &[cl::RemoveItem]) -> Result<Vec<mdb::RemoveItem>> {
    items
        .iter()
        .map(|r| {
            if !r.labels.is_empty() {
                let labels = r
                    .labels
                    .iter()
                    .map(lower_label_expr)
                    .collect::<Result<_>>()?;
                let target = r
                    .variable
                    .as_ref()
                    .map(|v| mdb::Expr::Variable(v.clone()))
                    .ok_or_else(|| nyi("REMOVE labels without variable"))?;
                return Ok(mdb::RemoveItem::Labels { target, labels });
            }
            let Some(p) = &r.property else {
                return Err(nyi("REMOVE item shape"));
            };
            Ok(mdb::RemoveItem::Property {
                target: if let Some(key) = &p.key_expr {
                    mdb::Expr::DynamicProperty {
                        expr: Box::new(lower_expr(&p.expr)?),
                        key: Box::new(lower_expr(key)?),
                    }
                } else {
                    mdb::Expr::Property {
                        expr: Box::new(lower_expr(&p.expr)?),
                        key: p.property.clone(),
                    }
                },
            })
        })
        .collect()
}

/// Lowers a CIP-278 `SEARCH` clause: the bound variable, its source
/// specification, and the score variable.
fn lower_search_clause(search: &cl::SearchClause) -> Result<mdb::SearchClause> {
    Ok(mdb::SearchClause {
        binding_var: search.binding_var.clone(),
        spec: lower_search_spec(&search.binding_var, &search.spec)?,
        score_var: search.score_var.clone(),
    })
}

/// Lowers a CIP-278 `<source specification clause>`: one index
/// specification or a (possibly nested) combined one.
fn lower_search_spec(binding_var: &str, spec: &cl::SearchSpec) -> Result<mdb::SearchSpec> {
    Ok(match spec {
        cl::SearchSpec::Index(spec) => mdb::SearchSpec::Index(Box::new(mdb::SearchIndexSpec {
            kind: match spec.kind {
                cl::SearchKind::Vector => mdb::SearchKind::Vector,
                cl::SearchKind::Fulltext => mdb::SearchKind::Fulltext,
            },
            index_name: spec.index_name.clone(),
            for_expr: lower_expr(&spec.for_expr)?,
            analyzer: lower_boxed(spec.analyzer.as_ref(), lower_expr)?,
            filters: super::search_filter::lower_search_filters(
                binding_var,
                spec.inner_where.as_ref(),
            )?,
            offset_expr: lower_boxed(spec.offset_expr.as_ref(), lower_expr)?,
            limit_expr: lower_expr(&spec.limit_expr)?,
        })),
        cl::SearchSpec::Combine(spec) => mdb::SearchSpec::Combine(mdb::SearchCombineSpec {
            sources: spec
                .sources
                .iter()
                .map(|source| {
                    Ok(mdb::SearchWeightedSource {
                        spec: lower_search_spec(binding_var, &source.spec)?,
                        weight_expr: lower_boxed(source.weight_expr.as_ref(), lower_expr)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            constant_expr: lower_boxed(spec.constant_expr.as_ref(), lower_expr)?,
            limit_expr: lower_boxed(spec.limit_expr.as_ref(), lower_expr)?,
        }),
    })
}
