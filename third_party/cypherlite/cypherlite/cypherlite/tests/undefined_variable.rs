//! ADR-0335: an unbound variable fails with the typed undefined-variable
//! error instead of evaluating to null — including the run-time-lowered
//! `SHOW … WHERE` predicates the plan-time binding walk cannot see.

mod common;

use cypherlite::{Error, Value};

use common::connection;

#[test]
fn show_where_with_an_unknown_name_fails() {
    let error = connection()
        .query("SHOW INDEXES WHERE nosuch = 1")
        .unwrap_err();
    assert_eq!(error, Error::undefined_variable("nosuch"));
    assert_eq!(
        error.to_string(),
        "syntax error: UndefinedVariable: variable 'nosuch' not defined"
    );
}

#[test]
fn show_where_with_a_yielded_column_still_filters() {
    let mut conn = connection();
    conn.query("CREATE INDEX idx FOR (n:L) ON (n.x)").unwrap();
    let result = conn
        .query("SHOW INDEXES YIELD name WHERE name = 'idx' RETURN name")
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::String("idx".into())]]);
}

/// ADR-0335 decision 5 (CIP-159 [12], Neo4j-verified): the unit row a void
/// `OPTIONAL CALL` yields over a zero-row driving table binds the outer
/// scope's variables to null, so the following projection reads null
/// instead of an unbound reference.
#[test]
fn a_void_optional_call_over_an_empty_table_reads_outer_variables_as_null() {
    let result = connection()
        .query(
            "MATCH (p:Person) OPTIONAL CALL db.createLabel('UnusedLabelC') \
             RETURN p.name AS name",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Null]]);
}

/// ADR-0335 decision 5 (CIP-28 [5], Neo4j-verified): a non-local singleton
/// declared later in the pattern reads as null inside the quantification
/// predicate, so only the zero-repetition matches survive.
#[test]
fn a_later_declared_singleton_reads_as_null_inside_a_quantification_predicate() {
    let mut conn = connection();
    conn.query(
        "CREATE (:V {name: 'v1', h: 1})-[:R]->(:V {name: 'v2', h: 2})\
         -[:R]->(:V {name: 'v3', h: 3})",
    )
    .unwrap();
    let result = conn
        .query(
            "MATCH (x)-->(y)((a)-[e]->(b) WHERE a.h + 2 < u.h)*(s)-->(u) \
             RETURN x.name AS x, u.name AS u",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![Value::String("v1".into()), Value::String("v3".into())]]
    );
}
