//! ADR-0327: the statement-scoped byte array through the public Rust API.
//! A `Value::Bytes` parameter flows through expressions, collections,
//! DISTINCT and ORDER BY into the result, types as `LIST<INTEGER>`, stamps
//! the `BINARY` column type, and fails with the typed `TypeError` wherever
//! it would be stored as a property, writing nothing.

mod common;

use cypherlite::{Error, Value};

use common::{connection, params};

fn bytes(b: &[u8]) -> Value {
    Value::Bytes(b.into())
}

#[test]
fn a_bytes_parameter_round_trips_through_expressions_and_collections() {
    let mut conn = connection();
    let r = conn
        .query_with_params(
            "RETURN $b AS b, [$b, $b] AS l, {k: $b} AS m, $b = $c AS same, \
             valueType($b) AS t, $b IS :: LIST<INTEGER NOT NULL> AS typed",
            params(&[("b", bytes(&[0, 1, 255])), ("c", bytes(&[0, 1, 255]))]),
        )
        .unwrap();
    let row = &r.rows[0];
    assert_eq!(row[0], bytes(&[0, 1, 255]));
    assert_eq!(
        row[1],
        Value::List(vec![bytes(&[0, 1, 255]), bytes(&[0, 1, 255])].into())
    );
    let Value::Map(m) = &row[2] else {
        panic!("expected a map, got {:?}", row[2]);
    };
    assert_eq!(m["k"], bytes(&[0, 1, 255]));
    assert_eq!(row[3], Value::Bool(true));
    assert_eq!(
        row[4],
        Value::String("LIST<INTEGER NOT NULL> NOT NULL".into())
    );
    assert_eq!(row[5], Value::Bool(true));
    assert_eq!(r.header.column_types[0], "BINARY");
}

#[test]
fn distinct_and_order_by_key_byte_wise() {
    let mut conn = connection();
    let r = conn
        .query_with_params(
            "UNWIND $xs AS x RETURN DISTINCT x ORDER BY x",
            params(&[(
                "xs",
                Value::List(vec![bytes(&[2]), bytes(&[1, 9]), bytes(&[2]), bytes(&[1])].into()),
            )]),
        )
        .unwrap();
    let column: Vec<Value> = r.rows.into_iter().map(|mut row| row.remove(0)).collect();
    assert_eq!(column, vec![bytes(&[1]), bytes(&[1, 9]), bytes(&[2])]);
}

#[test]
fn storing_a_byte_array_is_a_typed_type_error_and_writes_nothing() {
    let mut conn = connection();
    conn.query("CREATE (:Seed {k: 1})-[:R]->(:Seed {k: 2})")
        .unwrap();
    for statement in [
        "CREATE (:T {b: $b})",
        "CREATE (:T {bs: [$b]})",
        "CREATE (:T {bs: [[$b]]})",
        "MERGE (:T {b: $b})",
        "MATCH (n:Seed {k: 1}) SET n.b = $b",
        "MATCH (n:Seed {k: 1}) SET n = {b: $b}",
        "MATCH (n:Seed {k: 1}) SET n += {b: $b}",
        "MATCH ()-[r:R]->() SET r.b = $b",
        "MATCH (a:Seed {k: 1}), (b:Seed {k: 2}) CREATE (a)-[:R2 {b: $b}]->(b)",
    ] {
        let err = conn
            .query_with_params(statement, params(&[("b", bytes(&[7]))]))
            .expect_err(statement);
        let Error::TypeError(detail) = &err else {
            panic!("{statement}: expected Error::TypeError, got {err:?}");
        };
        assert!(
            detail.starts_with("byte array values cannot be stored"),
            "{statement}: {detail}"
        );
    }
    let r = conn
        .query(
            "MATCH (n) WHERE n:T OR n.b IS NOT NULL RETURN count(n) AS nodes \
             UNION ALL MATCH ()-[r]->() WHERE r.b IS NOT NULL OR type(r) = 'R2' \
             RETURN count(r) AS nodes",
        )
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(0)], vec![Value::Int(0)]]);
}
