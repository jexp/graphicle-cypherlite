//! Typed-IR regression tests: the CIP-214 distance metric and map
//! projections are typed `Expr` variants (not magic-string pseudo-calls), so
//! the binder sees their variables; and the static-concurrency check folds
//! over `Expr::children`, so no expression shape can smuggle a variable past
//! it.

mod common;

use std::collections::BTreeMap;

use cypherlite::Value;

use common::connection;

#[test]
fn non_static_concurrency_cannot_hide_behind_a_list_comprehension() {
    let mut connection = connection();
    // The direct-variable form was always rejected...
    let err = connection
        .query("MATCH (n) CALL (n) { RETURN n AS m } IN n CONCURRENT TRANSACTIONS RETURN m")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("NonStaticConcurrency"),
        "direct variable: {err}"
    );
    // ...and the variable smuggled through `[x IN [n] | x][0]` is now
    // rejected at compile time too (previously evaded the check and died
    // later with a runtime type error).
    let err = connection
        .query(
            "MATCH (n) CALL (n) { RETURN n AS m } \
             IN [x IN [n] | x][0] CONCURRENT TRANSACTIONS RETURN m",
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("NonStaticConcurrency"),
        "smuggled variable: {err}"
    );
}

#[test]
fn non_static_concurrency_rejects_patterns_and_subqueries_at_any_depth() {
    let mut connection = connection();
    for query in [
        "CALL { RETURN 1 AS y } IN size(()--()) CONCURRENT TRANSACTIONS RETURN y",
        "CALL { RETURN 1 AS y } IN (COUNT { RETURN 1 }) + 1 CONCURRENT TRANSACTIONS RETURN y",
        "CALL { RETURN 1 AS y } IN [p IN ()--() | 5] CONCURRENT TRANSACTIONS RETURN y",
    ] {
        let err = connection.query(query).unwrap_err().to_string();
        // `size(<pattern>)` is rejected by an earlier, more specific check.
        assert!(
            err.contains("NonStaticConcurrency") || err.contains("UnexpectedSyntax"),
            "{query}: {err}"
        );
    }
}

#[test]
fn vector_metric_is_typed_and_visible_to_the_binder() {
    let mut connection = connection();
    // Known bare keyword: resolves to a typed metric literal.
    let result = connection
        .query(
            "RETURN vector_distance(vector([3, 4], 2, INTEGER), vector([0, 0], 2, INTEGER), EUCLIDEAN) AS d",
        )
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Float(5.0)]]);

    // An identifier that is not a known metric stays a variable reference,
    // so binding rejects it (previously tunnelled through `__metric:`).
    let err = connection
        .query("RETURN vector_distance([1.0], [2.0], nosuchvar)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("UndefinedVariable: `nosuchvar`"),
        "smuggled metric variable: {err}"
    );

    // A STRING literal is not a bare keyword (CIP-214).
    let err = connection
        .query(
            "RETURN vector_distance(vector([1, 2, 3], 3, INTEGER), vector([1, 2, 4], 3, INTEGER), 'COSINE')",
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("UnknownDistanceMetric"),
        "string metric: {err}"
    );

    // `vector_norm` only supports true norms.
    let err = connection
        .query("RETURN vector_norm(vector([1, 2, 3], 3, INTEGER), COSINE)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("UnknownDistanceMetric"),
        "unsupported norm metric: {err}"
    );
}

#[test]
fn map_projection_is_typed_and_checks_its_variables() {
    let mut connection = connection();
    connection
        .query("CREATE (:Person {name: 'Alice', age: 33})")
        .unwrap();
    let result = connection
        .query("MATCH (n:Person) RETURN n { .name, years: n.age } AS m")
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![Value::Map(
            [
                ("name".to_string(), Value::String("Alice".into())),
                ("years".to_string(), Value::Int(33)),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into()
        )]]
    );

    // A bare-variable entry is a variable reference the binder must see.
    let err = connection
        .query("WITH 1 AS x RETURN x { nosuchvar }")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("UndefinedVariable: `nosuchvar`"),
        "map projection variable: {err}"
    );
}
