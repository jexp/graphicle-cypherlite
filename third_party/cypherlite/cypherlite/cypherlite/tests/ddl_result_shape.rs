//! Schema and administration DDL returns zero rows and zero columns —
//! the "omitted result" shape of a statement without `RETURN` (Neo4j
//! parity; CIP-161 scenarios [15]-[17]). Only row-producing catalog
//! commands (the SHOW family and `TERMINATE TRANSACTIONS`) project catalog
//! rows into the statement result.

mod common;

use std::collections::BTreeMap;

use cypherlite::{Connection, Value};

use common::connection;

/// Run `statement` and assert the result is empty: no rows, no columns.
fn assert_omitted(conn: &mut Connection, statement: &str, params: BTreeMap<String, Value>) {
    let result = conn
        .query_with_params(statement, params)
        .unwrap_or_else(|error| panic!("`{statement}` failed: {error}"));
    assert!(
        result.rows.is_empty(),
        "`{statement}`: expected zero rows, got {:?}",
        result.rows
    );
    assert!(
        result.header.columns.is_empty(),
        "`{statement}`: expected zero columns, got {:?}",
        result.header.columns
    );
}

#[test]
fn schema_ddl_returns_zero_rows_and_zero_columns() {
    let mut conn = connection();

    // Seed the objects the IF NOT EXISTS / DROP forms below act on. Every
    // object targets its own property so the DDL never conflicts with an
    // earlier one.
    conn.query("CREATE CONSTRAINT c_seed FOR (n:Drug) REQUIRE n.seed IS UNIQUE")
        .unwrap();

    // CREATE CONSTRAINT: named, unnamed, and IF NOT EXISTS on an existing
    // name (an idempotent no-op).
    assert_omitted(
        &mut conn,
        "CREATE CONSTRAINT c_named FOR (n:Drug) REQUIRE n.named IS UNIQUE",
        BTreeMap::new(),
    );
    assert_omitted(
        &mut conn,
        "CREATE CONSTRAINT FOR (n:Drug) REQUIRE n.strength IS UNIQUE",
        BTreeMap::new(),
    );
    assert_omitted(
        &mut conn,
        "CREATE CONSTRAINT c_seed IF NOT EXISTS FOR (n:Drug) REQUIRE n.seed IS UNIQUE",
        BTreeMap::new(),
    );

    // DROP CONSTRAINT: an existing name, and IF EXISTS as a no-op on a
    // missing name.
    assert_omitted(&mut conn, "DROP CONSTRAINT c_named", BTreeMap::new());
    assert_omitted(
        &mut conn,
        "DROP CONSTRAINT c_missing IF EXISTS",
        BTreeMap::new(),
    );

    // CREATE INDEX: single-property RANGE, TEXT, composite, and a `$param`
    // name (the parameter form routes through the catalog path).
    assert_omitted(
        &mut conn,
        "CREATE RANGE INDEX ri FOR (n:Drug) ON (n.ri)",
        BTreeMap::new(),
    );
    assert_omitted(
        &mut conn,
        "CREATE TEXT INDEX ti FOR (n:Drug) ON (n.ti)",
        BTreeMap::new(),
    );
    assert_omitted(
        &mut conn,
        "CREATE INDEX ci FOR (n:Drug) ON (n.ci1, n.ci2)",
        BTreeMap::new(),
    );
    assert_omitted(
        &mut conn,
        "CREATE INDEX $idx FOR (n:Drug) ON (n.param)",
        BTreeMap::from([("idx".to_string(), Value::String("param_idx".into()))]),
    );

    // DROP INDEX: a literal name, and a `$param` name.
    assert_omitted(&mut conn, "DROP INDEX ci", BTreeMap::new());
    assert_omitted(
        &mut conn,
        "DROP INDEX $idx",
        BTreeMap::from([("idx".to_string(), Value::String("param_idx".into()))]),
    );

    // Database administration and graph-type DDL.
    assert_omitted(
        &mut conn,
        "ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5",
        BTreeMap::new(),
    );
    assert_omitted(
        &mut conn,
        "CREATE GRAPH TYPE social AS { (:Person => {name :: STRING}) }",
        BTreeMap::new(),
    );
    assert_omitted(
        &mut conn,
        "ALTER CURRENT GRAPH TYPE SET { (:Private => {v :: STRING}) }",
        BTreeMap::new(),
    );
}

/// The SHOW path is unchanged: a row-producing catalog command still
/// projects its rows.
#[test]
fn show_constraints_still_projects_its_rows() {
    let mut conn = connection();
    conn.query("CREATE CONSTRAINT c_show FOR (n:Drug) REQUIRE n.name IS UNIQUE")
        .unwrap();

    let result = conn.query("SHOW CONSTRAINTS YIELD name").unwrap();
    assert_eq!(
        result.header.columns,
        vec!["name".to_string()],
        "SHOW CONSTRAINTS must keep its projected columns"
    );
    assert_eq!(
        result.rows.len(),
        1,
        "expected exactly one constraint row, got: {:?}",
        result.rows
    );
}

/// A schema/admin DDL stage seeds the next `NEXT` stage with one unit row,
/// exactly like a data write does (ADR-0287): the chained `MATCH` sees the
/// graph. Covers both DDL lowering paths — the catalog path (constraint,
/// text index) and the IR-native path (single-property range index). Each
/// statement runs on a fresh store: the unique constraint's backing index
/// would otherwise conflict with the range index on the same property.
#[test]
fn ddl_stage_seeds_next_chain() {
    for statement in [
        "CREATE CONSTRAINT c FOR (x:N) REQUIRE x.p IS UNIQUE NEXT MATCH (n) RETURN n",
        "CREATE TEXT INDEX t FOR (x:N) ON (x.q) NEXT MATCH (n) RETURN n",
        "CREATE INDEX i FOR (x:N) ON (x.p) NEXT MATCH (n) RETURN n",
    ] {
        let mut conn = connection();
        conn.query("CREATE (n:N {p: 1})").unwrap();
        let result = conn
            .query(statement)
            .unwrap_or_else(|error| panic!("`{statement}` failed: {error}"));
        assert_eq!(
            result.rows.len(),
            1,
            "`{statement}`: expected the seeded node, got {:?}",
            result.rows
        );
        let Value::Node(node) = &result.rows[0][0] else {
            panic!("`{statement}`: expected a node, got {:?}", result.rows[0]);
        };
        assert_eq!(
            node.labels.as_slice(),
            ["N".to_string()].as_slice(),
            "`{statement}`"
        );
        assert_eq!(node.properties["p"], Value::Int(1), "`{statement}`");
    }
}

/// A row-producing catalog stage keeps its row semantics in a `NEXT` chain
/// (ADR-0287): it seeds the next stage from its projected rows — exactly
/// once, with no extra unit row.
#[test]
fn show_stage_seeds_next_chain_from_its_rows() {
    let mut conn = connection();
    conn.query("CREATE (n:N {p: 1})").unwrap();
    conn.query("CREATE CONSTRAINT c_show FOR (x:N) REQUIRE x.p IS UNIQUE")
        .unwrap();

    let result = conn
        .query("SHOW CONSTRAINTS YIELD name NEXT MATCH (n) RETURN n, name")
        .unwrap();
    assert_eq!(
        result.rows.len(),
        1,
        "one constraint row seeds one next-stage row, got {:?}",
        result.rows
    );
    assert_eq!(result.rows[0][1], Value::String("c_show".into()));
}
