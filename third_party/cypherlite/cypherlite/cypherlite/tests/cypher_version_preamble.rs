//! The CIP-150 `CYPHER <version>` preamble is typed data
//! (`Statement::cypher_version`), lowered onto the IR query and planned into
//! `LogicalPlan::cypher_version`, so version-dependent executor behavior is
//! identical for any whitespace after the preamble — the historical
//! query-text prefix sniff required a literal space after `CYPHER 5` and
//! silently missed `CYPHER 5\nSHOW ...`.

mod common;

use cypherlite::Value;

use common::{connection, params};

/// Run `src` with `$x` bound to `null`: the null-argument cases below pass
/// the parameter explicitly, because an absent one fails the statement with
/// `ParameterMissing` (ADR-0308).
fn query_x_null(src: &str) -> cypherlite::Result<cypherlite::QueryResult> {
    connection().query_with_params(src, params(&[("x", Value::Null)]))
}

#[test]
fn cypher5_show_settings_null_arg_errors_with_any_preamble_whitespace() {
    for src in [
        "CYPHER 5 SHOW SETTINGS $x",
        "CYPHER 5\nSHOW SETTINGS $x",
        "CYPHER 5\tSHOW SETTINGS $x",
    ] {
        let err = query_x_null(src).unwrap_err().to_string();
        assert!(
            err.contains("InvalidArgumentType: setting name must be STRING or LIST<STRING>"),
            "wrong outcome for {src:?}: {err}"
        );
    }
}

#[test]
fn cypher5_null_arg_errors_across_show_and_terminate_commands() {
    for (src, detail) in [
        (
            "CYPHER 5\nSHOW TRANSACTIONS null YIELD transactionId RETURN transactionId",
            "transaction id must be STRING or LIST<STRING>",
        ),
        (
            "CYPHER 5\nTERMINATE TRANSACTIONS null",
            "transaction id must be STRING or LIST<STRING>",
        ),
        (
            "CYPHER 5\nSHOW DATABASE $db YIELD name RETURN name",
            "database name must be STRING",
        ),
    ] {
        let err = connection()
            .query_with_params(src, params(&[("db", Value::Null)]))
            .unwrap_err()
            .to_string();
        assert!(err.contains(detail), "wrong outcome for {src:?}: {err}");
    }
}

#[test]
fn cypher25_and_versionless_show_settings_null_arg_stay_lenient() {
    for src in ["CYPHER 25 SHOW SETTINGS $x", "SHOW SETTINGS $x"] {
        let result = query_x_null(src).unwrap();
        assert!(result.rows.is_empty(), "wrong outcome for {src:?}");
    }
}

#[test]
fn cypher5_version_reaches_nested_call_and_next_stages() {
    let err =
        query_x_null("CYPHER 5 CALL { SHOW SETTINGS $x YIELD name RETURN name } RETURN 1 AS n")
            .unwrap_err()
            .to_string();
    assert!(
        err.contains("InvalidArgumentType: setting name must be STRING or LIST<STRING>"),
        "nested CALL lost the preamble: {err}"
    );

    let err = query_x_null("CYPHER 5\nRETURN 1 AS n NEXT SHOW SETTINGS $x")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("InvalidArgumentType: setting name must be STRING or LIST<STRING>"),
        "NEXT stage lost the preamble: {err}"
    );
}

#[test]
fn cypher5_preamble_reaches_expression_subqueries() {
    // CIP-150: the preamble scopes the whole statement, so EXISTS/COUNT/
    // COLLECT expression subqueries inherit `cypher_version` at evaluation
    // time even though their lowered `Query::cypher_version` stays `None`.
    let detail = "InvalidArgumentType: setting name must be STRING or LIST<STRING>";
    for src in [
        "CYPHER 5 RETURN EXISTS { SHOW SETTINGS $x } AS e",
        "CYPHER 5 RETURN COUNT { SHOW SETTINGS $x } AS c",
        "CYPHER 5 RETURN COLLECT { SHOW SETTINGS $x YIELD name RETURN name } AS c",
        "CYPHER 5\nRETURN EXISTS { SHOW SETTINGS $x } AS e",
        "CYPHER 5\nRETURN COUNT { SHOW SETTINGS $x } AS c",
        "CYPHER 5\nRETURN COLLECT { SHOW SETTINGS $x YIELD name RETURN name } AS c",
    ] {
        let err = query_x_null(src).unwrap_err().to_string();
        assert!(
            err.contains(detail),
            "expression subquery lost the preamble for {src:?}: {err}"
        );
    }
}

#[test]
fn versionless_and_cypher25_expression_subqueries_stay_lenient() {
    for (src, want) in [
        (
            "RETURN EXISTS { SHOW SETTINGS $x } AS e",
            Value::Bool(false),
        ),
        (
            "CYPHER 25 RETURN EXISTS { SHOW SETTINGS $x } AS e",
            Value::Bool(false),
        ),
        ("RETURN COUNT { SHOW SETTINGS $x } AS c", Value::Int(0)),
        (
            "CYPHER 25 RETURN COUNT { SHOW SETTINGS $x } AS c",
            Value::Int(0),
        ),
        (
            "RETURN COLLECT { SHOW SETTINGS $x YIELD name RETURN name } AS c",
            Value::List(vec![].into()),
        ),
        (
            "CYPHER 25 RETURN COLLECT { SHOW SETTINGS $x YIELD name RETURN name } AS c",
            Value::List(vec![].into()),
        ),
    ] {
        let result = query_x_null(src).unwrap();
        assert_eq!(result.rows, vec![vec![want]], "wrong outcome for {src:?}");
    }
}

#[test]
fn show_transactions_current_query_is_the_statement_text() {
    let src = "// a marker comment\nSHOW TRANSACTIONS YIELD currentQuery RETURN currentQuery";
    let result = connection().query(src).unwrap();
    assert_eq!(
        result.rows,
        vec![vec![Value::String(src.into())]],
        "currentQuery must be the full statement text"
    );
}

#[test]
fn show_constraints_create_statement_stays_version_dependent() {
    let mut conn = connection();
    conn.query("CREATE CONSTRAINT x FOR (n:L) REQUIRE n.p IS :: INT")
        .unwrap();
    let cypher5 = conn
        .query("CYPHER 5 SHOW CONSTRAINTS YIELD name, createStatement RETURN name, createStatement")
        .unwrap();
    assert_eq!(
        cypher5.rows,
        vec![vec![Value::String("x".into()), Value::Null]],
        "Cypher 5 hides the propertyType createStatement"
    );
    let cypher25 = conn
        .query(
            "CYPHER 25 SHOW CONSTRAINTS YIELD name, createStatement RETURN name, createStatement",
        )
        .unwrap();
    assert!(
        matches!(&cypher25.rows[0][1], Value::String(s) if s.contains("IS :: INTEGER")),
        "Cypher 25 shows the createStatement: {:?}",
        cypher25.rows
    );
}
