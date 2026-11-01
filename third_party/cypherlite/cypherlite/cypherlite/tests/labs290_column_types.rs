//! LABS-290 / ADR-0224: the engine stamps every `QueryResult` with
//! `column_types` — one canonical type name per column, same length and
//! order as `columns`, derived from the FIRST row. A Null first-row cell
//! or an empty result stamps `ANY`.

mod common;

/// Scalar literals stamp their value's canonical type name.
#[test]
fn scalar_columns_stamp_their_type() {
    let mut conn = common::connection();
    for (query, expected) in [
        ("RETURN 1 AS n", "INTEGER"),
        ("RETURN 1.5 AS f", "FLOAT"),
        ("RETURN 'x' AS s", "STRING"),
        ("RETURN true AS b", "BOOLEAN"),
        ("RETURN [1, 2] AS l", "LIST"),
        ("RETURN {k: 1} AS m", "MAP"),
    ] {
        let r = common::run(&mut conn, query);
        assert_eq!(r.header.column_types, [expected], "{query}");
        assert_eq!(
            r.header.column_types.len(),
            r.header.columns.len(),
            "{query}: one type name per column"
        );
    }
}

/// A `MATCH`-bound entity stamps NODE / RELATIONSHIP.
#[test]
fn entity_columns_stamp_node_and_relationship() {
    let mut conn = common::connection();
    common::run(&mut conn, "CREATE (:P {x: 1})-[:R]->(:P)");
    let r = common::run(&mut conn, "MATCH (n:P) RETURN n");
    assert_eq!(r.header.column_types, ["NODE"]);
    let r = common::run(&mut conn, "MATCH ()-[r:R]->() RETURN r");
    assert_eq!(r.header.column_types, ["RELATIONSHIP"]);
}

/// A result with no rows stamps every column ANY.
#[test]
fn empty_result_stamps_any() {
    let mut conn = common::connection();
    let r = common::run(&mut conn, "MATCH (n:NoSuchLabel) RETURN n.name AS name");
    assert_eq!(r.header.columns, ["name"]);
    assert!(r.rows.is_empty());
    assert_eq!(r.header.column_types, ["ANY"]);
}

/// A Null first-row cell stamps ANY, even when later rows carry values.
#[test]
fn null_first_row_stamps_any() {
    let mut conn = common::connection();
    let r = common::run(&mut conn, "RETURN null AS x");
    assert_eq!(r.header.column_types, ["ANY"]);
    let r = common::run(&mut conn, "UNWIND [null, 1] AS x RETURN x");
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.header.column_types, ["ANY"]);
}

/// The stamp describes the first row only: a non-Null first row keeps its
/// type even when a later row is Null.
#[test]
fn first_row_wins_over_later_nulls() {
    let mut conn = common::connection();
    let r = common::run(&mut conn, "UNWIND [1, null] AS x RETURN x");
    assert_eq!(r.header.column_types, ["INTEGER"]);
}

/// Multi-column results stamp each column independently, in column order.
#[test]
fn multi_column_order_matches_columns() {
    let mut conn = common::connection();
    common::run(&mut conn, "CREATE (:P {name: 'A', x: 1})");
    let r = common::run(
        &mut conn,
        "MATCH (n:P) RETURN n.name AS s, n.x AS i, n AS node, n.missing AS absent",
    );
    assert_eq!(r.header.column_types, ["STRING", "INTEGER", "NODE", "ANY"]);
}

/// EXPLAIN and PROFILE carry the stamp through their construction paths
/// (EXPLAIN carries the plan tree with no data columns, ADR-0359; PROFILE
/// re-stamps the wrapped statement's own columns).
#[test]
fn explain_and_profile_carry_column_types() {
    let mut conn = common::connection();
    let r = common::run(&mut conn, "EXPLAIN MATCH (n:P) RETURN n");
    // ADR-0359: EXPLAIN's result has no columns and no rows — the plan
    // rides the tree.
    assert!(r.header.columns.is_empty());
    assert!(r.header.column_types.is_empty());
    assert!(r.plan.is_some(), "EXPLAIN stamps the plan tree");
    // ADR-0344/0345 phase 2: PROFILE returns the statement's own columns
    // (the deprecated `_profile_*` columns are gone); the profile facts
    // ride the stamped plan tree.
    let r = common::run(&mut conn, "PROFILE RETURN 1 AS n");
    assert_eq!(r.header.column_types, ["INTEGER"]);
    let r = common::run(&mut conn, "PROFILE CREATE (:P {x: 1})");
    assert!(
        r.header.column_types.is_empty(),
        "{:?}",
        r.header.column_types
    );
    assert!(r.plan.is_some(), "PROFILE stamps the plan tree");
}
