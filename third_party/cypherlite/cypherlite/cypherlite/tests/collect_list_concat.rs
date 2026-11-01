//! LABS-199 / ADR-0198: `WITH *` grouping keys and aggregates in CASE arms.
//!
//! Two dialect rulings ground-truthed against live Neo4j CE 2026.09.0:
//!
//! 1. When a `WITH`/`RETURN` projection combines `*` with aggregating
//!    expressions, every variable carried by `*` is a grouping key (one
//!    group per carried row, exactly as when the variable is projected
//!    explicitly).
//! 2. An aggregate call in any ordinary expression position of an
//!    aggregating projection — including a CASE arm — belongs to that
//!    projection.
//!
//! The agreed rejections (unprojected carried variable, same-clause alias,
//! unprojected property leaf) stay rejections with unchanged error kinds.

mod common;

use cypherlite::Value;

use common::{connection, err_text, list, run};

fn int(value: i64) -> Value {
    Value::Int(value)
}

fn seed() -> cypherlite::Connection {
    let mut c = connection();
    run(&mut c, "CREATE (a {x: 1, y: 10}), (b {x: 2, y: 10})");
    c
}

// s20: `WITH *` expands to all in-scope variables; they become the grouping
// keys, so the carried `z` inside the aggregate-containing expression
// resolves — one group per carried row.
#[test]
fn labs_199_with_star_registers_grouping_keys() {
    let mut c = seed();

    let r = run(
        &mut c,
        "WITH 99 AS z MATCH (n) WITH *, collect(n.x) + [z] AS xs RETURN xs",
    );
    let mut xs: Vec<Value> = r.rows.into_iter().map(|row| row[0].clone()).collect();
    xs.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(
        xs,
        vec![list(vec![int(1), int(99)]), list(vec![int(2), int(99)])]
    );
}

// The carried variables ride through to the next projection: `n` stays in
// scope after the aggregating `WITH *`, one row per group.
#[test]
fn labs_199_with_star_count_star_keeps_carried_variables() {
    let mut c = seed();

    let r = run(&mut c, "MATCH (n) WITH *, count(*) AS c RETURN n.x, c");
    let mut got: Vec<Vec<Value>> = r.rows;
    got.sort_by_key(|row| format!("{:?}", row[0]));
    assert_eq!(
        got,
        vec![vec![int(1), int(1)], vec![int(2), int(1)]],
        "one group per carried row"
    );
}

// A bare (non-list-wrapped) variable leaf next to the aggregate resolves
// against the `*` grouping keys too — the front-end and planner checks agree.
#[test]
fn labs_199_with_star_bare_variable_leaf() {
    let mut c = seed();

    let r = run(
        &mut c,
        "WITH 99 AS z MATCH (n) WITH *, count(n.x) + z AS v RETURN v",
    );
    assert_eq!(r.rows, vec![vec![int(100)], vec![int(100)]]);
}

// `RETURN *` with an aggregate follows the same ruling: the carried
// variables are the grouping keys and ride through to the result.
#[test]
fn labs_199_return_star_with_aggregate() {
    let mut c = seed();

    let r = run(
        &mut c,
        "WITH 99 AS z MATCH (n) RETURN *, collect(n.x) + [z] AS xs",
    );
    assert_eq!(r.header.columns, vec!["n", "xs", "z"]);
    let mut xs: Vec<Value> = r.rows.iter().map(|row| row[1].clone()).collect();
    xs.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(
        xs,
        vec![list(vec![int(1), int(99)]), list(vec![int(2), int(99)])]
    );
}

// p-case: an aggregate call inside a CASE arm belongs to the enclosing
// aggregating projection.
#[test]
fn labs_199_aggregate_in_case_arm() {
    let mut c = seed();

    let r = run(
        &mut c,
        "MATCH (n) WITH CASE WHEN true THEN collect(n.x) + [3] ELSE [] END AS xs RETURN xs",
    );
    assert_eq!(r.rows, vec![vec![list(vec![int(1), int(2), int(3)])]]);

    // The not-taken arm's aggregate still belongs to the projection.
    let r = run(
        &mut c,
        "MATCH (n) WITH CASE WHEN false THEN [] ELSE collect(n.x) + [3] END AS xs RETURN xs",
    );
    assert_eq!(r.rows, vec![vec![list(vec![int(1), int(2), int(3)])]]);
}

// `WITH *` must not make an unprojected property leaf legal: `*` projects
// the variables, not their property expressions (ADR-0198 keeps the i03
// rejection).
#[test]
fn labs_199_with_star_still_rejects_property_leaf() {
    let mut c = seed();

    let err =
        err_text(c.query("WITH 99 AS z MATCH (n) WITH *, collect(n.x) + [n.y] AS xs RETURN xs"));
    assert!(
        err.contains("AmbiguousAggregationExpression"),
        "property leaf must stay rejected, got: {err}"
    );
}

// The agreed rejections keep their semantics: an unprojected carried
// variable (i01), a same-clause alias reference (i02), and an unprojected
// property leaf (i03) all stay errors.
#[test]
fn labs_199_agreed_rejections_unchanged() {
    let mut c = seed();

    let err = err_text(c.query("WITH 10 AS y MATCH (n) WITH collect(n.x) + [y] AS xs RETURN xs"));
    assert!(
        err.contains("AmbiguousAggregationExpression"),
        "i01 must stay rejected, got: {err}"
    );

    let err = err_text(c.query("MATCH (n) WITH n.y AS y, collect(n.x) + [y] AS xs RETURN xs"));
    assert!(
        err.contains("AmbiguousAggregationExpression") || err.contains("Variable"),
        "i02 must stay rejected, got: {err}"
    );

    let mut c = seed();
    let err = err_text(c.query("MATCH (n) WITH collect(n.x) + [n.y] AS xs RETURN xs"));
    assert!(
        err.contains("AmbiguousAggregationExpression"),
        "i03 must stay rejected, got: {err}"
    );
}

// Stellar F-1-11: the Comparison arm of the grouping-key ambiguity check
// applies the same both-sides-aggregate recursion as the Binary arm. The
// missing recursion was a copy-paste divergence inherited from the original
// import, not a ruling — the planner's IR lowers comparisons to the same
// `Expr::Binary` node and checks them identically. A complex non-aggregate
// part hiding in a mixed comparison side is ambiguous; simple key parts
// stay valid.
#[test]
fn labs_199_comparison_mixed_sides_follow_binary_rules() {
    let mut c = seed();

    // `n.x + 1` is a complex non-aggregate part: rejected under a
    // both-aggregating comparison, exactly as the Binary arm has always
    // rejected the same shape under `+`.
    let err = err_text(
        c.query("MATCH (n) WITH n.x AS x, count(*) > (sum(n.y) + (n.x + 1)) AS c RETURN c"),
    );
    assert!(
        err.contains("AmbiguousAggregationExpression"),
        "complex non-key part in a mixed comparison side must be ambiguous, got: {err}"
    );
    let err = err_text(
        c.query("MATCH (n) WITH n.x AS x, count(*) + (sum(n.y) + (n.x + 1)) AS c RETURN c"),
    );
    assert!(
        err.contains("AmbiguousAggregationExpression"),
        "Binary arm parity, got: {err}"
    );

    // A simple grouping-key part in a mixed comparison side stays valid
    // (one group per `n.x`; every count is 1, every sum + key exceeds it).
    let r = run(
        &mut c,
        "MATCH (n) WITH n.x AS x, count(*) > (sum(n.y) + n.x) AS c RETURN c",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Bool(false)], vec![Value::Bool(false)]]
    );

    // A pure comparison of two aggregates was and stays valid.
    let r = run(
        &mut c,
        "MATCH (n) WITH n.x AS x, count(*) > sum(n.y) AS c RETURN c",
    );
    assert_eq!(
        r.rows,
        vec![vec![Value::Bool(false)], vec![Value::Bool(false)]]
    );
}
