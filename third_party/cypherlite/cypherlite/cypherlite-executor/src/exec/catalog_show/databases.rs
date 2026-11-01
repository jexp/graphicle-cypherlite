//! `SHOW DATABASES` and `SHOW`/`TERMINATE TRANSACTIONS` row producers.

use std::collections::BTreeMap;
use std::sync::Arc;

use cypherlite_core::{Error, Result, Value};
use cypherlite_parser::{DatabaseFilter, SchemaName};

use crate::ctx::QueryCtx;
use crate::eval::{eval, Row};
use crate::overlay::ExecGraph;

use super::super::catalog::catalog_row;

pub(crate) fn show_transactions_rows(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::ShowTransactionsClause,
    rows: &[Row],
    params: &BTreeMap<String, Value>,
    qctx: QueryCtx<'_>,
) -> Result<Vec<Row>> {
    Ok({
        let txid = "neo4j-transaction-1".to_string();
        let mut found = c.ids.is_empty();
        for expr in &c.ids {
            let expr = cypherlite_front::lower_expr(expr)?;
            for row in rows {
                let value = eval(graph, row, &expr, params, qctx.cypher_version)?;
                if value.is_null() && qctx.is_cypher5() {
                    return Err(Error::invalid_argument_type(
                        "transaction id must be STRING or LIST<STRING>",
                    ));
                }
                found |= match value {
                    Value::String(ref s) => **s == *txid,
                    Value::List(ref xs) => xs
                        .iter()
                        .any(|v| matches!(v, Value::String(s) if **s == *txid)),
                    _ => false,
                };
            }
        }
        if found {
            vec![catalog_row([
                ("transactionId", Value::String(txid.into())),
                ("currentQuery", Value::String(qctx.query_text.into())),
                ("database", Value::String("neo4j".into())),
            ])]
        } else {
            Vec::new()
        }
    })
}

pub(crate) fn terminate_transactions_rows(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::TerminateTransactionsClause,
    rows: &[Row],
    params: &BTreeMap<String, Value>,
    qctx: QueryCtx<'_>,
) -> Result<Vec<Row>> {
    Ok({
        let mut out = Vec::new();
        for expr in &c.ids {
            let expr = cypherlite_front::lower_expr(expr)?;
            for outer in rows {
                let values = match eval(graph, outer, &expr, params, qctx.cypher_version)? {
                    Value::Null if qctx.is_cypher5() => {
                        return Err(Error::invalid_argument_type(
                            "transaction id must be STRING or LIST<STRING>",
                        ))
                    }
                    Value::List(v) => v,
                    v => Arc::new(vec![v]),
                };
                for value in values.iter() {
                    let id = match value {
                        Value::String(_) => value.clone(),
                        _ => Value::Null,
                    };
                    if !out
                        .iter()
                        .any(|r: &Row| r.get("transactionId") == Some(&id))
                    {
                        out.push(catalog_row([
                            ("transactionId", id),
                            ("username", Value::Null),
                            ("message", Value::String("Transaction not found.".into())),
                        ]));
                    }
                }
            }
        }
        out
    })
}

/// Which fixed namespaces `SHOW DATABASES` may emit: every one, exactly
/// one, or none (a null name parameter off Cypher 5, or a name no fixed
/// namespace carries — `Namespace::parse` already matches
/// case-insensitively).
#[derive(Clone, Copy)]
enum NamespaceFilter {
    All,
    Only(cypherlite_core::Namespace),
    NoMatch,
}

impl NamespaceFilter {
    /// The filter for a database name: `Only` when it names a fixed
    /// namespace, `NoMatch` otherwise.
    fn named(name: &str) -> Self {
        match cypherlite_core::Namespace::parse(name) {
            Some(ns) => NamespaceFilter::Only(ns),
            None => NamespaceFilter::NoMatch,
        }
    }

    /// Whether a fixed namespace passes the filter.
    fn admits(self, ns: cypherlite_core::Namespace) -> bool {
        match self {
            NamespaceFilter::All => true,
            NamespaceFilter::Only(wanted) => wanted == ns,
            NamespaceFilter::NoMatch => false,
        }
    }
}

pub(crate) fn show_databases_rows(
    graph: &ExecGraph<'_>,
    c: &cypherlite_parser::ShowDatabasesClause,
    params: &BTreeMap<String, Value>,
    qctx: QueryCtx<'_>,
) -> Result<Vec<Row>> {
    Ok({
        let state = graph.show_schema()?;
        // The filter resolves to the typed model at the boundary; an absent
        // parameter fails with `ParameterMissing` (ADR-0308).
        let wanted = match &c.filter {
            DatabaseFilter::All => NamespaceFilter::All,
            DatabaseFilter::Default | DatabaseFilter::Home => {
                NamespaceFilter::Only(cypherlite_core::Namespace::Neo4j)
            }
            DatabaseFilter::Named(SchemaName::Literal(n)) => NamespaceFilter::named(n),
            DatabaseFilter::Named(SchemaName::Parameter(p)) => match params.get(p) {
                None => return Err(Error::parameters_missing([p.as_str()])),
                Some(Value::String(name)) => NamespaceFilter::named(name),
                Some(Value::Null) if qctx.is_cypher5() => {
                    return Err(Error::invalid_argument_type("database name must be STRING"))
                }
                Some(Value::Null) => NamespaceFilter::NoMatch,
                _ => return Err(Error::invalid_argument_type("database name must be STRING")),
            },
        };
        // ADR-0116 §1/§6: exactly the three fixed namespaces; an unknown
        // name returns no rows. The stored `defaultLanguage` comes from the
        // store-level databases catalog (ADR-0116 §8/§10; `CYPHER 25`
        // default).
        cypherlite_core::Namespace::ALL
            .iter()
            .filter(|ns| wanted.admits(**ns))
            .map(|ns| {
                let name = ns.as_str();
                // `DatabasesCatalog::default`/`sanitize` guarantee an
                // entry for every fixed namespace, so indexing cannot miss.
                let language = state.databases[ns].default_language;
                // ADR-0142: the topology/status columns carry fixed
                // Browser-compatible constants, not live status — CypherLite
                // is a single-node embedded engine, so every namespace is
                // always online/primary. `default`/`home` are true only for
                // `neo4j` (ADR-0116). Column order matches cypherlite-front's
                // DEFAULT_DATABASE_COLUMNS so `YIELD *` exposes the Neo4j
                // ordering. ADR-0142: `address` is the constant
                // `localhost:7687` — Neo4j Desktop builds the instance id
                // `name@address` and rejects an empty address; the value is
                // a display placeholder correct for the default Bolt bind.
                let is_neo4j = matches!(ns, cypherlite_core::Namespace::Neo4j);
                catalog_row([
                    ("name", Value::String(name.into())),
                    (
                        "type",
                        Value::String(
                            if matches!(ns, cypherlite_core::Namespace::System) {
                                "system"
                            } else {
                                "standard"
                            }
                            .into(),
                        ),
                    ),
                    ("aliases", Value::List(Vec::new().into())),
                    (
                        "access",
                        Value::String(
                            if ns.is_read_only() {
                                "read-only"
                            } else {
                                "read-write"
                            }
                            .into(),
                        ),
                    ),
                    ("address", Value::String("localhost:7687".into())),
                    ("role", Value::String("primary".into())),
                    ("writer", Value::Bool(true)),
                    ("requestedStatus", Value::String("online".into())),
                    ("currentStatus", Value::String("online".into())),
                    ("statusMessage", Value::String("".into())),
                    ("default", Value::Bool(is_neo4j)),
                    ("home", Value::Bool(is_neo4j)),
                    ("constituents", Value::List(Vec::new().into())),
                    (
                        "creationTime",
                        Value::Temporal(cypherlite_core::Temporal::DateTime {
                            utc: chrono::Utc::now().naive_utc() - chrono::Duration::hours(1),
                            offset: 0,
                            zone: None,
                        }),
                    ),
                    (
                        "lastStartTime",
                        Value::Temporal(cypherlite_core::Temporal::DateTime {
                            utc: chrono::Utc::now().naive_utc() - chrono::Duration::minutes(30),
                            offset: 0,
                            zone: None,
                        }),
                    ),
                    ("lastStopTime", Value::Null),
                    ("store", Value::String("cypherlite-native-1.0".into())),
                    (
                        "defaultLanguage",
                        Value::String(format!("CYPHER {language}").into()),
                    ),
                ])
            })
            .collect()
    })
}
