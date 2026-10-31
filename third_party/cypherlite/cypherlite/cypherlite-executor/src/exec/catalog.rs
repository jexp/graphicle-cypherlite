//! Catalog command execution: row shaping for `SHOW ...` and dispatch into
//! `catalog_show` / `catalog_schema`.

use std::collections::BTreeMap;

use cypherlite_core::{Error, Result, Value};
use cypherlite_parser::token::quote_identifier_always;

use crate::eval::{eval, Row};
use crate::graph::QueryStats;
use crate::overlay::ExecGraph;

use super::scope::visible_keys;
use super::{catalog_schema, catalog_show, QueryResult};
use crate::ctx::QueryCtx;

pub(crate) fn imported_catalog(
    rows: Vec<indexmap::IndexMap<String, cypherlite_model::CypherValue>>,
) -> Vec<Row> {
    rows.into_iter()
        .map(|r| {
            r.into_iter()
                .map(|(k, v)| (k, cypherlite_core::cypher_to_value(&v)))
                .collect()
        })
        .collect()
}

pub(crate) fn project_catalog(
    graph: &ExecGraph<'_>,
    input: Vec<Row>,
    mut catalog: Vec<Row>,
    clause: &cypherlite_parser::Clause,
    params: &BTreeMap<String, Value>,
    cypher_version: Option<u32>,
) -> Result<Vec<Row>> {
    let info = cypherlite_front::show_columns::catalog_info(clause)
        .ok_or_else(|| Error::internal("catalog projection called for a non-catalog clause"))?;
    // The function/procedure/setting catalog rows are built from metadata
    // tables that may omit keys; pre-fill every full column with Null so
    // `YIELD *` + WHERE sees a uniform shape.
    if info.prefill {
        for row in &mut catalog {
            for column in &info.full_columns {
                row.entry(column.clone()).or_insert(Value::Null);
            }
        }
    }
    let predicate = info
        .where_expr
        .map(cypherlite_front::lower_expr)
        .transpose()?;
    // The projection rule is selected once; `None` is the `YIELD *` raw-row
    // passthrough, `Some` the (output, source) column lists. The cross join
    // skeleton then exists once. The two arms' collision policies are the
    // two projection contracts, stated once each: `YIELD *` merges the raw
    // catalog row into the outer row and keeps an outer variable that
    // collides with a column (the passthrough must not clobber the row it
    // is joining), while a projected column list names the composed
    // columns outright, so its `insert` wins (a column the catalog row
    // lacks composes as Null, like a missing projected cell).
    let projection = info.projection();
    let fill: Option<&[(String, String)]> = match &projection {
        cypherlite_front::show_columns::CatalogProjection::Star => None,
        cypherlite_front::show_columns::CatalogProjection::Columns(pairs) => Some(pairs),
    };
    let mut output = Vec::new();
    for outer in input {
        for raw in &catalog {
            let mut row = outer.clone();
            match fill {
                None => {
                    for (k, v) in raw {
                        row.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
                Some(pairs) => {
                    for (out, src) in pairs {
                        row.insert(out.clone(), raw.get(src).cloned().unwrap_or(Value::Null));
                    }
                }
            }
            output.push(row);
        }
    }
    // SHOW paging applies to the flattened cross join, and in written order
    // before the trailing WHERE. Applying it once per input row changes both
    // composed SHOW cardinality and which row a subsequent WHERE observes.
    super::project::apply_yield_page_stages(
        graph,
        params,
        cypher_version,
        info.stages,
        &mut output,
    )?;
    if let Some(predicate) = predicate.as_ref() {
        // ADR-0335: the predicate lowers at run time, so an undefined
        // variable surfaces its typed error here instead of silently
        // filtering every row; a null or non-true value still filters.
        let mut filtered = Vec::with_capacity(output.len());
        for row in output {
            if eval(graph, &row, predicate, params, cypher_version)?.truthy() == Some(true) {
                filtered.push(row);
            }
        }
        output = filtered;
    }
    Ok(output)
}

/// Statement result of a catalog clause: `Some` of the projected catalog
/// rows for row-producing commands (the SHOW family and `TERMINATE
/// TRANSACTIONS`), `None` for schema/admin DDL, which updates the catalog
/// and finishes with an empty result — zero rows, zero columns — matching
/// Neo4j and CIP-161's `CREATE CONSTRAINT` expectation. The row-producing
/// classification is the canonical `cypherlite-front` predicate, so this
/// module never re-enumerates the command set.
pub(crate) fn catalog_result(
    rows: &[Row],
    clause: &cypherlite_parser::Clause,
) -> Option<QueryResult> {
    let info = cypherlite_front::show_columns::catalog_info(clause)?;
    // The projection: `YIELD *` keeps the first row's key order when a row
    // exists (falling back to the command's full column table for a
    // zero-row result, so the header survives), and both other modes come
    // from the one column projection.
    let columns = match info.projection() {
        cypherlite_front::show_columns::CatalogProjection::Star => rows
            .first()
            .map(visible_keys)
            .unwrap_or_else(|| info.full_columns.clone()),
        cypherlite_front::show_columns::CatalogProjection::Columns(pairs) => {
            pairs.into_iter().map(|(output, _)| output).collect()
        }
    };
    let data = rows
        .iter()
        .map(|row| {
            columns
                .iter()
                .map(|column| row.get(column).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    Some(QueryResult::intermediate(
        columns,
        data,
        QueryStats::default(),
    ))
}

pub(crate) fn catalog_row(pairs: impl IntoIterator<Item = (&'static str, Value)>) -> Row {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

pub(crate) fn string_list(values: impl IntoIterator<Item = String>) -> Value {
    Value::List(
        values
            .into_iter()
            .map(|s| Value::String(s.into()))
            .collect::<Vec<_>>()
            .into(),
    )
}

/// Render an identifier in the always-backtick-quoted form SHOW-rendered
/// Cypher text and the CIP-180 canonical graph-type body use: the parser's
/// one forced-quoting constructor,
/// [`quote_identifier_always`](cypherlite_parser::token::quote_identifier_always)
/// (`Person` → `` `Person` ``), so the escape rule (embedded backticks
/// doubled) is never re-spelled here.
pub(crate) fn backtick(identifier: &str) -> String {
    quote_identifier_always(identifier)
}

pub(crate) fn schema_name_value(
    name: &cypherlite_parser::SchemaName,
    params: &BTreeMap<String, Value>,
) -> Result<String> {
    match name {
        cypherlite_parser::SchemaName::Literal(name) => Ok(name.clone()),
        cypherlite_parser::SchemaName::Parameter(name) => match params.get(name) {
            Some(Value::String(value)) => Ok(String::from(&**value)),
            // ADR-0308: an absent parameter is ParameterMissing, not a type.
            None => Err(Error::parameters_missing([name.as_str()])),
            Some(_) => Err(Error::type_error("schema name must be STRING")),
        },
    }
}

pub(crate) fn execute_catalog(
    graph: &ExecGraph<'_>,
    clause: &cypherlite_parser::Clause,
    rows: Vec<Row>,
    params: &BTreeMap<String, Value>,
    qctx: QueryCtx<'_>,
) -> Result<Vec<Row>> {
    use cypherlite_parser::{Clause as C, FunctionScope};
    let catalog = match clause {
        C::ShowFunctions(c) => {
            if matches!(c.scope, FunctionScope::UserDefined) {
                Vec::new()
            } else {
                imported_catalog(crate::show_catalog::function_catalog_rows())
            }
        }
        C::ShowProcedures(_) => imported_catalog(crate::show_catalog::procedure_catalog_rows(
            graph.base().preview(),
        )),
        C::ShowRoles(c) => catalog_show::show_roles_rows(c),
        C::ShowServers(_) => vec![catalog_row([(
            "name",
            Value::String("cypherlite-server-1".into()),
        )])],
        C::ShowUsers(_) => vec![catalog_row([("user", Value::String("neo4j".into()))])],
        C::ShowSettings(c) => {
            return catalog_show::show_settings_rows(graph, c, rows, clause, params, qctx);
        }
        C::ShowTransactions(c) => {
            catalog_show::show_transactions_rows(graph, c, &rows, params, qctx)?
        }
        C::TerminateTransactions(c) => {
            catalog_show::terminate_transactions_rows(graph, c, &rows, params, qctx)?
        }
        C::ShowDatabases(c) => catalog_show::show_databases_rows(graph, c, params, qctx)?,
        C::ShowIndexes(c) => catalog_show::show_indexes_rows(graph, c, qctx)?,
        C::ShowConstraints(c) => catalog_show::show_constraints_rows(graph, c, qctx)?,
        C::ShowGraphType(c) => catalog_show::show_graph_type_rows(graph, c)?,
        C::ShowCurrentGraphType(c) => catalog_show::show_current_graph_type_rows(graph, c)?,
        C::CreateIndex(c) => {
            return catalog_schema::catalog_create_index(
                graph,
                c,
                rows,
                params,
                qctx.cypher_version,
            )
        }
        C::DropIndex(c) => return catalog_schema::catalog_drop_index(graph, c, rows, params),
        C::CreateConstraint(c) => {
            return catalog_schema::catalog_create_constraint(
                graph,
                c,
                rows,
                params,
                qctx.cypher_version,
            )
        }
        C::DropConstraint(c) => {
            return catalog_schema::catalog_drop_constraint(graph, c, rows, params)
        }
        C::AlterDatabase(c) => {
            return catalog_schema::catalog_alter_database(graph, c, rows, params)
        }
        C::CreateGraphType(c) => return catalog_schema::catalog_create_graph_type(graph, c, rows),
        C::AlterCurrentGraphType(c) => {
            return catalog_schema::alter_current_graph_type(graph, c, rows)
        }
        _ => return Err(Error::semantic("unsupported catalog command")),
    };
    project_catalog(graph, rows, catalog, clause, params, qctx.cypher_version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imported_catalog_preserves_temporal_and_map_list_values() {
        let date = chrono::NaiveDate::from_ymd_opt(2024, 2, 29).unwrap();
        let mut attrs = indexmap::IndexMap::new();
        attrs.insert("nested".to_string(), cypherlite_model::CypherValue::Int(7));
        let mut row = indexmap::IndexMap::new();
        row.insert(
            "created".to_string(),
            cypherlite_model::CypherValue::Date(date),
        );
        row.insert(
            "attrs".to_string(),
            cypherlite_model::CypherValue::Map(attrs),
        );
        row.insert(
            "tags".to_string(),
            cypherlite_model::CypherValue::List(vec![
                cypherlite_model::CypherValue::Str("a".to_string()),
                cypherlite_model::CypherValue::Int(2),
            ]),
        );

        let rows = imported_catalog(vec![row]);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert!(matches!(
            row.get("created"),
            Some(Value::Temporal(cypherlite_core::temporal::Temporal::Date(d))) if *d == date
        ));
        assert!(
            matches!(row.get("attrs"), Some(Value::Map(m)) if m.get("nested") == Some(&Value::Int(7)))
        );
        match row.get("tags") {
            Some(Value::List(items)) => {
                assert_eq!(items[0], Value::String("a".into()));
                assert_eq!(items[1], Value::Int(2));
            }
            other => panic!("expected list, got {other:?}"),
        }
    }
}
