//! `SHOW ...` catalog command row producers (extracted `execute_catalog` arms).

use std::collections::{BTreeMap, HashSet};

use cypherlite_core::{Error, Result, Value};
use cypherlite_model::EntityKind;
use cypherlite_parser::RoleModifier;

use crate::ctx::QueryCtx;
use crate::eval::{eval, Row};
use crate::overlay::ExecGraph;

use super::catalog::{catalog_row, project_catalog};

mod constraints;
mod databases;
mod indexes;

pub(crate) use constraints::show_constraints_rows;
pub(crate) use databases::{
    show_databases_rows, show_transactions_rows, terminate_transactions_rows,
};
pub(crate) use indexes::{show_indexes_result, show_indexes_rows};

/// The pattern variable Neo4j CE's rendered statements use — the single
/// place the `n`-for-nodes / `r`-for-relationships decision lives; both
/// the `SHOW INDEXES` and the `SHOW CONSTRAINTS` createStatement
/// renderings read it.
fn entity_variable(entity: EntityKind) -> &'static str {
    match entity {
        EntityKind::Node => "n",
        EntityKind::Relationship => "r",
    }
}

pub(crate) fn show_roles_rows(c: &cypherlite_parser::ShowRolesClause) -> Vec<Row> {
    let roles = [
        ("admin", Some("neo4j")),
        ("architect", None),
        ("publisher", None),
        ("editor", None),
        ("reader", None),
        ("PUBLIC", None),
    ];
    roles
        .into_iter()
        .filter(|(_, member)| !matches!(c.modifier, RoleModifier::Populated) || member.is_some())
        .map(|(role, member)| {
            let mut row = catalog_row([("role", Value::String(role.into()))]);
            if c.with_users {
                row.insert(
                    "member".into(),
                    member
                        .map(|m| Value::String(m.into()))
                        .unwrap_or(Value::Null),
                );
            }
            row
        })
        .collect()
}

/// The fixed four-row SHOW SETTINGS catalog. One canonical table: both the
/// bare form and the names-filtered form select from it.
fn settings_catalog() -> Vec<Row> {
    vec![
        catalog_row([
            ("name", Value::String("server.bolt.enabled".into())),
            ("value", Value::String("true".into())),
            ("isDeprecated", Value::Bool(false)),
        ]),
        catalog_row([
            ("name", Value::String("db.checkpoint".into())),
            ("value", Value::Null),
            ("isDeprecated", Value::Bool(false)),
        ]),
        catalog_row([
            ("name", Value::String("server.db.query_cache_size".into())),
            ("value", Value::Null),
            ("isDeprecated", Value::Bool(true)),
        ]),
        catalog_row([
            (
                "name",
                Value::String("dbms.routing.load_balancing.plugin".into()),
            ),
            ("value", Value::Null),
            ("isDeprecated", Value::Bool(true)),
        ]),
    ]
}

pub(crate) fn show_settings_rows(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::ShowSettingsClause,
    rows: Vec<Row>,
    clause: &cypherlite_parser::Clause,
    params: &BTreeMap<String, Value>,
    qctx: QueryCtx<'_>,
) -> Result<Vec<Row>> {
    let Some(names_expr) = &c.names else {
        return project_catalog(
            graph,
            rows,
            settings_catalog(),
            clause,
            params,
            qctx.cypher_version,
        );
    };
    let lowered = cypherlite_front::lower_expr(names_expr)?;
    let mut output = Vec::new();
    for row in rows {
        let value = eval(graph, &row, &lowered, params, qctx.cypher_version)?;
        if value.is_null() && qctx.is_cypher5() {
            return Err(Error::invalid_argument_type(
                "setting name must be STRING or LIST<STRING>",
            ));
        }
        let names = match value {
            Value::String(name) => Some(vec![name]),
            Value::List(values) if values.is_empty() => None,
            Value::List(values) => Some(
                values
                    .iter()
                    .filter_map(|value| match value {
                        Value::String(name) => Some(name.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => Some(Vec::new()),
        };
        let selected = settings_catalog()
            .into_iter()
            .filter(|candidate| {
                names.as_ref().is_none_or(|names| {
                    matches!(candidate.get("name"), Some(Value::String(name)) if names.contains(name))
                })
            })
            .collect();
        output.extend(project_catalog(
            graph,
            vec![row],
            selected,
            clause,
            params,
            qctx.cypher_version,
        )?);
    }
    Ok(output)
}

pub(crate) fn show_graph_type_rows(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::ShowGraphTypeClause,
) -> Result<Vec<Row>> {
    Ok({
        let state = graph.show_schema()?;
        let body = state
            .data
            .graph_types
            .get(&c.name)
            .ok_or_else(|| Error::semantic("GraphTypeNotFound"))?;
        vec![catalog_row([
            ("name", Value::String(c.name.as_str().into())),
            (
                "body",
                Value::String(
                    crate::graph_type::canonicalize_graph_type_body(&body.to_body())
                        .map_err(crate::graph_type::persisted_derive_error)?
                        .into(),
                ),
            ),
        ])]
    })
}

pub(crate) fn show_current_graph_type_rows(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::ShowCurrentGraphTypeClause,
) -> Result<Vec<Row>> {
    let state = graph.show_schema()?;
    let derived_names: HashSet<_> = state.data.derived_constraints.keys().cloned().collect();
    let extras = graph
        .named_constraints()
        .into_iter()
        .filter(|constraint| !derived_names.contains(&constraint.name))
        .filter_map(|constraint| {
            use std::str::FromStr;
            Some(cypherlite_model::Constraint {
                name: constraint.name,
                label: constraint.label,
                properties: constraint.properties,
                constraint_type: cypherlite_model::ConstraintType::from_str(&constraint.kind)
                    .ok()?,
                property_type: constraint.property_type,
                entity_type: EntityKind::from_relationship_flag(constraint.relationship)
                    .display_name()
                    .into(),
                enforced_label: None,
            })
        })
        .collect::<Vec<_>>();
    Ok(if let Some(as_graph) = c.as_graph {
        let (nodes, rels) = crate::graph_type_visualize::render_graph_type_as_graph(
            &state.data.current_graph_type.to_body(),
            &extras,
            as_graph,
        )?;
        vec![catalog_row([
            (
                "nodes",
                Value::List(
                    nodes
                        .iter()
                        .map(cypherlite_core::cypher_to_value)
                        .collect::<Vec<_>>()
                        .into(),
                ),
            ),
            (
                "relationships",
                Value::List(
                    rels.iter()
                        .map(cypherlite_core::cypher_to_value)
                        .collect::<Vec<_>>()
                        .into(),
                ),
            ),
        ])]
    } else {
        vec![catalog_row([(
            "specification",
            Value::String(
                crate::graph_type::current_graph_type_specification(
                    &state.data.current_graph_type.to_body(),
                    &extras,
                )
                .map_err(crate::graph_type::persisted_derive_error)?
                .into(),
            ),
        )])]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;

    /// Runs a SHOW DATABASES statement through the real catalog path
    /// (parse → execute_catalog projection) against the default theater,
    /// which carries exactly the `neo4j` and `system` namespaces.
    fn show_databases(query: &str) -> Vec<Row> {
        let graph = Graph::default();
        let view = ExecGraph::read(&graph);
        let stmt = cypherlite_parser::parse(query).expect("parse");
        let clause = stmt.clauses.into_iter().next().expect("one clause");
        super::super::catalog::execute_catalog(
            &view,
            &clause,
            vec![Row::new()],
            &BTreeMap::new(),
            QueryCtx::new(None, query),
        )
        .expect("show databases")
    }

    fn row_named<'a>(rows: &'a [Row], name: &str) -> &'a Row {
        rows.iter()
            .find(|row| row.get("name") == Some(&Value::String(name.into())))
            .unwrap_or_else(|| panic!("no row for {name}"))
    }

    /// Pins the ADR-0142 Browser/Desktop-compat values *and* types
    /// on every row: strings for access/address/role/requestedStatus/
    /// currentStatus/statusMessage, booleans for writer/default/home.
    /// `access` is the one namespace-dependent value (ADR-0116 §4:
    /// `system_ontologies` is read-only).
    fn assert_browser_compat_row(row: &Row, is_default: bool, expected_access: &str) {
        assert!(
            matches!(row.get("access"), Some(Value::String(s)) if &**s == expected_access),
            "access: {:?}",
            row.get("access")
        );
        assert!(
            matches!(row.get("address"), Some(Value::String(s)) if &**s == "localhost:7687"),
            "address: {:?}",
            row.get("address")
        );
        assert!(
            matches!(row.get("role"), Some(Value::String(s)) if &**s == "primary"),
            "role: {:?}",
            row.get("role")
        );
        assert!(
            matches!(row.get("requestedStatus"), Some(Value::String(s)) if &**s == "online"),
            "requestedStatus: {:?}",
            row.get("requestedStatus")
        );
        assert!(
            matches!(row.get("currentStatus"), Some(Value::String(s)) if &**s == "online"),
            "currentStatus: {:?}",
            row.get("currentStatus")
        );
        assert!(
            matches!(row.get("statusMessage"), Some(Value::String(s)) if s.is_empty()),
            "statusMessage: {:?}",
            row.get("statusMessage")
        );
        assert_eq!(row.get("writer"), Some(&Value::Bool(true)));
        assert_eq!(row.get("default"), Some(&Value::Bool(is_default)));
        assert_eq!(row.get("home"), Some(&Value::Bool(is_default)));
    }

    /// The default theater carries exactly the three fixed namespaces
    /// (ADR-0116 §1): `neo4j` (default/home), `system` and
    /// `system_ontologies` (read-only); every row must carry the
    /// Browser-compat values.
    fn assert_fixed_namespace_rows(rows: &[Row]) {
        assert_eq!(rows.len(), 3);
        assert_browser_compat_row(row_named(rows, "neo4j"), true, "read-write");
        assert_browser_compat_row(row_named(rows, "system"), false, "read-write");
        assert_browser_compat_row(row_named(rows, "system_ontologies"), false, "read-only");
    }

    #[test]
    fn bare_show_databases_rows_carry_browser_compat_values() {
        assert_fixed_namespace_rows(&show_databases("SHOW DATABASES"));
    }

    #[test]
    fn show_databases_yield_exposes_browser_compat_values() {
        // Front validation must still accept a YIELD of these columns
        // (values changed, not the column set).
        cypherlite_front::parse_and_lower(
            "SHOW DATABASES YIELD name, access, address, role, writer, \
             requestedStatus, currentStatus, statusMessage, default, home \
             RETURN name",
        )
        .expect("front validates the YIELD list");
        assert_fixed_namespace_rows(&show_databases(
            "SHOW DATABASES YIELD name, access, address, role, writer, \
             requestedStatus, currentStatus, statusMessage, default, home",
        ));
    }

    #[test]
    fn show_databases_yield_star_exposes_browser_compat_values() {
        assert_fixed_namespace_rows(&show_databases("SHOW DATABASES YIELD *"));
    }

    #[test]
    fn show_default_database_row_marks_neo4j_default_and_home() {
        let rows = show_databases("SHOW DEFAULT DATABASE");
        assert_eq!(rows.len(), 1);
        assert_browser_compat_row(row_named(&rows, "neo4j"), true, "read-write");
    }

    /// Pins the load-bearing YIELD-* invariant: `show_databases_rows`
    /// inserts keys in exactly cypherlite-front's
    /// `DEFAULT_DATABASE_COLUMNS` ++ `NON_DEFAULT_DATABASE_COLUMNS` order
    /// (`Row` is an `IndexMap` and `catalog_result` reads the first row's
    /// key order), so `SHOW DATABASES YIELD *` exposes the Neo4j ordering.
    #[test]
    fn show_databases_row_key_order_matches_front_column_tables() {
        let expected: Vec<&str> = cypherlite_front::show_columns::DEFAULT_DATABASE_COLUMNS
            .iter()
            .chain(cypherlite_front::show_columns::NON_DEFAULT_DATABASE_COLUMNS.iter())
            .copied()
            .collect();
        let rows = show_databases("SHOW DATABASES YIELD *");
        assert!(!rows.is_empty());
        for row in &rows {
            let keys: Vec<&str> = row.keys().map(String::as_str).collect();
            assert_eq!(keys, expected);
        }
    }
}
