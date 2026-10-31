//! CIP-250 (GQ10): the `FOR <variable> IN <expression>` clause behind the
//! ADR-0371 `gql-for` preview gate — the GQL-compliant successor spelling
//! of `UNWIND <expression> AS <variable>`, normalized onto the existing
//! UNWIND infrastructure (identical validation, planning, and execution).
//! Every expectation is doc-derived from the CIP-250 example table.
//!
//! The gate is process-level (ADR-0371: `Database::open` resolves
//! `CYPHERLITE_PREVIEW` once), and an integration-test crate is its own
//! process — set `gql-for` before any store opens and keep every test in
//! this crate behind the gate. The ungated fail-closed error is pinned by
//! the registry-driven fixture in `src/tests/preview.rs` (every row,
//! `gql-for` included).

mod common;

use cypherlite::Value;

/// The gated connection: `gql-for` set before any store opens.
fn gated_connection() -> cypherlite::Connection {
    common::set_preview_once("gql-for");
    common::connection()
}

/// The single-column `x` rows of a FOR query, in order.
fn x_rows(c: &mut cypherlite::Connection, query: &str) -> Vec<Value> {
    common::rows(c, query)
        .into_iter()
        .map(|row| row.into_iter().next().expect("one column"))
        .collect()
}

fn int(v: i64) -> Value {
    Value::Int(v)
}

#[test]
fn for_matches_unwind_on_lists_nulls_and_non_lists() {
    // The CIP-250 test-case table: FOR produces identical results to UNWIND
    // for list, null, and non-list values (a non-list is coerced to a
    // singleton list, matching UNWIND).
    let mut c = gated_connection();
    for (for_query, unwind_query, want) in [
        (
            "FOR x IN [1, 2, 3] RETURN x",
            "UNWIND [1, 2, 3] AS x RETURN x",
            vec![int(1), int(2), int(3)],
        ),
        (
            "FOR x IN null RETURN x",
            "UNWIND null AS x RETURN x",
            vec![],
        ),
        (
            "FOR x IN 42 RETURN x",
            "UNWIND 42 AS x RETURN x",
            vec![int(42)],
        ),
    ] {
        assert_eq!(x_rows(&mut c, for_query), want, "{for_query}");
        // Identical to the UNWIND spelling of the same statement.
        assert_eq!(x_rows(&mut c, unwind_query), want, "{unwind_query}");
    }
}

#[test]
fn for_chains_and_nested_lists() {
    // The CIP-250 examples: a null element in an intermediate row produces
    // no rows for it; nested lists unnest per level.
    let mut c = gated_connection();
    let got = x_rows(&mut c, "FOR x IN [1, null, 2] FOR y IN x RETURN y");
    assert_eq!(got, vec![int(1), int(2)]);
    let got = x_rows(&mut c, "FOR x IN [[1, 2], [3, 4]] FOR y IN x RETURN y");
    assert_eq!(got, vec![int(1), int(2), int(3), int(4)]);
    // Unnested one level only: the outer rows are the inner lists.
    let got = x_rows(&mut c, "FOR x IN [[1, 2], [3, 4]] RETURN x");
    assert_eq!(
        got,
        vec![
            common::list(vec![int(1), int(2)]),
            common::list(vec![int(3), int(4)]),
        ]
    );
}

#[test]
fn for_in_every_unwind_clause_position() {
    // The CIP-250 test-case table: variable scoping and interaction with
    // every clause position where UNWIND is valid — after MATCH, after
    // WITH, inside a CALL subquery, with aggregation, and inside FOREACH.
    let mut c = gated_connection();
    c.query("CREATE (:Person {name: 'Ada', friends: ['Bob', 'Cyd']})")
        .unwrap();

    let rows = common::rows(
        &mut c,
        "MATCH (n:Person) FOR friend IN n.friends RETURN friend",
    );
    assert_eq!(
        rows,
        vec![vec![common::string("Bob")], vec![common::string("Cyd")]]
    );

    let rows = common::rows(&mut c, "WITH 3 AS n FOR x IN range(1, n) RETURN x");
    assert_eq!(rows, vec![vec![int(1)], vec![int(2)], vec![int(3)]]);

    let rows = common::rows(&mut c, "WITH [1, 2, 3] AS list FOR x IN list RETURN x");
    assert_eq!(rows.len(), 3);

    let rows = common::rows(&mut c, "CALL { FOR x IN [1, 2, 3] RETURN x } RETURN x");
    assert_eq!(rows.len(), 3);

    let rows = common::rows(
        &mut c,
        "MATCH (n:Person) FOR tag IN n.friends WITH tag, count(*) AS cnt RETURN tag, cnt",
    );
    assert_eq!(rows.len(), 2);

    c.query("FOREACH (i IN [1] | FOR x IN [10, 20] CREATE (:Tagged {v: x}))")
        .unwrap();
    let rows = common::rows(&mut c, "MATCH (t:Tagged) RETURN t.v ORDER BY t.v");
    assert_eq!(rows, vec![vec![int(10)], vec![int(20)]]);
}

#[test]
fn for_forwards_variables_into_subqueries() {
    // The CIP-250 test-case table: variables introduced by FOR are
    // correctly forwarded into subqueries (the CALL-scope import rule).
    let mut c = gated_connection();
    let rows = common::rows(
        &mut c,
        "FOR x IN [1, 2] CALL { RETURN x AS inner } RETURN inner",
    );
    assert_eq!(rows, vec![vec![int(1)], vec![int(2)]]);
    // The FOR variable is visible inside an importing subquery body too.
    let rows = common::rows(
        &mut c,
        "FOR x IN [1, 2] CALL { WITH x AS carried RETURN carried } RETURN carried",
    );
    assert_eq!(rows, vec![vec![int(1)], vec![int(2)]]);
}

#[test]
fn for_shows_the_unwind_plan_operator() {
    // The CIP-250 interaction table: the query plan shows the Unwind
    // operator, identical to UNWIND — no new operator exists.
    let mut c = gated_connection();
    let result = c.query("EXPLAIN FOR x IN [1, 2, 3] RETURN x").unwrap();
    let unwind = c.query("EXPLAIN UNWIND [1, 2, 3] AS x RETURN x").unwrap();
    // The operator shape is identical (the plan's `statement_echo` field
    // alone carries the differing query text).
    let for_unwind = common::find_node(common::plan(&result), "Unwind")
        .expect("the FOR plan shows the Unwind operator");
    assert!(common::find_node(common::plan(&unwind), "Unwind").is_some());
    assert_eq!(
        format!("{for_unwind:?}"),
        format!(
            "{:?}",
            common::find_node(common::plan(&unwind), "Unwind").expect("the UNWIND operator")
        )
    );
}

#[test]
fn for_variable_binding_follows_the_unwind_rules() {
    // The CIP-250 interaction table: the binding variable follows the same
    // scoping rules as UNWIND — whatever the enclosing scope already bound,
    // the two spellings behave identically (this engine, like Neo4j, lets a
    // UNWIND-family clause rebind an incoming name to the element).
    let mut c = gated_connection();
    let for_rows = x_rows(&mut c, "WITH 1 AS x FOR x IN [1, 2] RETURN x");
    let unwind_rows = x_rows(&mut c, "WITH 1 AS x UNWIND [1, 2] AS x RETURN x");
    assert_eq!(for_rows, unwind_rows);
    // A genuinely unknown variable after the clause is the same error
    // either way.
    let for_error = common::err_text(c.query("FOR x IN [1, 2] RETURN y"));
    let unwind_error = common::err_text(c.query("UNWIND [1, 2] AS x RETURN y"));
    assert_eq!(for_error, unwind_error);
    assert!(for_error.contains("Variable"), "wrong error: {for_error}");
}

#[test]
fn for_error_shapes_are_parse_errors() {
    // The CIP-250 example table's error shapes: missing IN, missing
    // variable, and the UNWIND-shaped AS form all fail the parse under the
    // opt-in too (the gate admits the grammar, not malformed input).
    let mut c = gated_connection();
    for query in [
        "FOR x [1, 2, 3] RETURN x",
        "FOR IN [1, 2, 3] RETURN x",
        "FOR [1, 2, 3] AS x RETURN x",
    ] {
        let error = common::err_text(c.query(query));
        assert!(!error.is_empty(), "{query} parsed");
    }
}
