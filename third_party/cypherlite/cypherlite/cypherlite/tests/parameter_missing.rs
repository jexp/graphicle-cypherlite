//! ADR-0308: a statement that references a parameter missing from the
//! supplied parameters fails before execution with the typed
//! `Error::ParameterMissing`, on the engine every surface shares. Nothing
//! executes; EXPLAIN still plans without values; a parameter supplied as
//! `null` is still `null`.

mod common;

use std::collections::BTreeMap;

use cypherlite::{Error, Value};

use common::{connection, params};

/// [`Error::parameters_missing`] over the names the statement referenced —
/// the typed constructor owns the `Expected parameter(s): ` prefix and the
/// `, ` join, so this helper never re-spells them.
fn missing(names: &[&str]) -> Error {
    Error::parameters_missing(names.iter().copied())
}

#[test]
fn an_absent_parameter_fails_with_parameter_missing() {
    let error = connection().query("RETURN $p AS p").unwrap_err();
    assert_eq!(error, missing(&["p"]));
    assert_eq!(
        error.to_string(),
        "semantic error: ParameterMissing: Expected parameter(s): p"
    );
}

#[test]
fn every_missing_name_is_listed_once_in_order_of_appearance() {
    let error = connection()
        .query_with_params(
            "WITH $b AS b, $a AS a, $c AS c RETURN b, a, c, $b AS again",
            params(&[("a", Value::Int(1))]),
        )
        .unwrap_err();
    assert_eq!(error, missing(&["b", "c"]));
}

#[test]
fn a_null_parameter_is_still_null() {
    let result = connection()
        .query_with_params("RETURN $p AS p", params(&[("p", Value::Null)]))
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Null]]);
}

#[test]
fn explain_plans_without_parameter_values() {
    // EXPLAIN plans without parameter values (ADR-0308) — and carries the
    // plan tree with no data columns (ADR-0359).
    let result = connection().query("EXPLAIN RETURN $p AS p").unwrap();
    assert!(result.header.columns.is_empty());
    assert!(result.plan.is_some(), "EXPLAIN stamps the plan tree");
}

#[test]
fn profile_checks_like_a_normal_run() {
    let error = connection().query("PROFILE RETURN $p AS p").unwrap_err();
    assert_eq!(error, missing(&["p"]));
}

#[test]
fn nothing_executes_when_a_parameter_is_missing() {
    let mut conn = connection();
    // The reference is unreachable at run time (no row reaches `$p`), and
    // the write precedes it: the statement still fails before dispatch.
    let error = conn
        .query("CREATE (:Written) WITH 1 AS x WHERE false RETURN $p AS p")
        .unwrap_err();
    assert_eq!(error, missing(&["p"]));
    let count = conn
        .query("MATCH (n:Written) RETURN count(n) AS c")
        .unwrap();
    assert_eq!(count.rows, vec![vec![Value::Int(0)]]);
}

#[test]
fn nested_references_are_checked() {
    for query in [
        "CALL { RETURN $p AS p } RETURN p",
        "RETURN EXISTS { MATCH (n) WHERE n.x = $p } AS e",
        "MATCH (n {k: $p}) RETURN n",
        "UNWIND [1] AS x RETURN x SKIP $p",
        "RETURN 1 AS x NEXT RETURN $p AS p",
        "FOREACH (x IN [1] | CREATE (:N {v: $p}))",
    ] {
        assert_eq!(
            connection().query(query).unwrap_err(),
            missing(&["p"]),
            "{query}"
        );
    }
}

#[test]
fn catalog_command_parameters_are_checked_when_they_run() {
    for query in [
        "SHOW DATABASE $p YIELD name RETURN name",
        "CREATE INDEX $p FOR (n:L) ON (n.x)",
        "CREATE INDEX i FOR (n:L) ON (n.x) OPTIONS $p",
        "SHOW SETTINGS $p",
        // ADR-0337: the constraint OPTIONS map propagates the lowering/eval
        // errors instead of swallowing them into InvalidOptionsMap.
        "CREATE CONSTRAINT c FOR (n:L) REQUIRE n.x IS UNIQUE OPTIONS {indexConfig: $p}",
    ] {
        assert_eq!(
            connection().query(query).unwrap_err(),
            missing(&["p"]),
            "{query}"
        );
    }
}

#[test]
fn a_missing_parameter_fails_the_open_transaction() {
    let mut conn = connection();
    conn.begin().unwrap();
    conn.query("CREATE (:InTxn)").unwrap();
    assert_eq!(conn.query("RETURN $p AS p").unwrap_err(), missing(&["p"]));
    assert_eq!(
        conn.query("RETURN 1 AS n").unwrap_err().to_string(),
        "semantic error: transaction failed; rollback required"
    );
    conn.rollback().unwrap();
    let count = conn.query("MATCH (n:InTxn) RETURN count(n) AS c").unwrap();
    assert_eq!(count.rows, vec![vec![Value::Int(0)]]);
}

/// The example of `docs/api/rust/errors.md` (ADR-0307, ADR-0308), verbatim.
#[test]
fn typed_error_variants_doc_example() {
    use cypherlite::{ArithmeticFailure, Database};

    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let missing = conn.query("RETURN $p AS p").unwrap_err();
    assert_eq!(missing, Error::parameters_missing(["p"]));
    let params = BTreeMap::from([("p".to_string(), Value::Null)]);
    assert_eq!(
        conn.query_with_params("RETURN $p AS p", params)
            .unwrap()
            .rows,
        vec![vec![Value::Null]]
    );
    assert_eq!(
        conn.query("RETURN 1 / 0 AS v").unwrap_err(),
        Error::Arithmetic(ArithmeticFailure::DivisionByZero)
    );
    assert!(matches!(
        conn.query("USE nosuch RETURN 1"),
        Err(Error::DatabaseNotFound { .. })
    ));
}
