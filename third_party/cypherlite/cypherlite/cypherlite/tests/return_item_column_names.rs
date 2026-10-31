//! Unaliased `RETURN` items use the verbatim source text as the column
//! name — matching Neo4j — instead of falling back to `column_<idx>` for
//! expression shapes the engine had no dedicated label for (`count{...}`,
//! `EXISTS { ... }`, list/map literals, `CASE`, parameters, subscripts, …).
//!
//! Reported via GraphAcademy (Adam Cowley). The fix threads the parser's
//! `source_text` through `Projection` and consults it in
//! `Projection::output_name` (see `cypherlite-ir/src/ast.rs`).

mod common;

use common::{connection, params};

fn columns(q: &str) -> Vec<String> {
    connection().query(q).unwrap().header.columns
}

#[test]
fn count_subquery_exists_and_literals_get_source_names() {
    assert_eq!(
        columns("RETURN count{()}, true, 123"),
        vec!["count{()}", "true", "123"],
    );
    assert_eq!(
        columns("RETURN EXISTS { MATCH () }"),
        vec!["EXISTS { MATCH () }"],
    );
    assert_eq!(
        columns("RETURN COLLECT { MATCH (n) RETURN n }"),
        vec!["COLLECT { MATCH (n) RETURN n }"],
    );
}

#[test]
fn list_map_case_and_parameter_get_source_names() {
    assert_eq!(columns("RETURN [1, 2, 3]"), vec!["[1, 2, 3]"]);
    assert_eq!(columns("RETURN {a: 1, b: 2}"), vec!["{a: 1, b: 2}"]);
    assert_eq!(
        columns("RETURN CASE WHEN true THEN 1 ELSE 0 END"),
        vec!["CASE WHEN true THEN 1 ELSE 0 END"],
    );
    let result = connection()
        .query_with_params(
            "RETURN $param",
            params(&[("param", cypherlite::Value::Null)]),
        )
        .unwrap();
    assert_eq!(result.header.columns, vec!["$param"]);
}

#[test]
fn operators_and_function_calls_get_source_names() {
    assert_eq!(columns("RETURN 1 + 2"), vec!["1 + 2"]);
    assert_eq!(columns("RETURN NOT true"), vec!["NOT true"]);
    assert_eq!(columns("RETURN size([1, 2])"), vec!["size([1, 2])"]);
    assert_eq!(columns("RETURN count(*)"), vec!["count(*)"]);
}

#[test]
fn variable_property_and_star_keep_their_canonical_names() {
    assert_eq!(
        columns("UNWIND [1] AS n RETURN n, n + 1"),
        vec!["n", "n + 1"],
    );
    assert_eq!(columns("UNWIND [{a: 1}] AS x RETURN x.a"), vec!["x.a"],);
    assert_eq!(columns("UNWIND [1] AS n WITH n RETURN *"), vec!["n"],);
}

#[test]
fn aliases_still_win_over_source_text() {
    assert_eq!(
        columns("RETURN count{()} AS total, 1 + 2 AS sum"),
        vec!["total", "sum"],
    );
}

/// A parenthesized function call unwraps during lowering, so the stored
/// `source_text` starts with the outer `(` — the column name must still
/// be the canonical `size([1, 2])`, not `size(size([1, 2]))`.
#[test]
fn parenthesized_function_call_keeps_canonical_name() {
    assert_eq!(columns("RETURN (size([1, 2]))"), vec!["size([1, 2])"]);
}

/// Non-call parenthesized expressions keep their parentheses in the
/// column name — the paren-strip only exists to recover the canonical
/// function name when the call is wrapped, not to normalize text.
#[test]
fn parenthesized_non_call_keeps_parens_in_column_name() {
    assert_eq!(columns("RETURN (1 + 2)"), vec!["(1 + 2)"]);
}

/// Two distinct expressions keep distinct, source-faithful column names,
/// so the second row value never silently overwrites the first — the
/// collision risk that fell out of the previous "placeholder label" fix
/// (reported in Stellar review round 5).
#[test]
fn distinct_case_expressions_keep_distinct_column_names() {
    let r = connection()
        .query("RETURN CASE WHEN true THEN 1 ELSE 0 END, CASE WHEN false THEN 2 ELSE 3 END")
        .unwrap();
    assert_eq!(
        r.header.columns,
        vec![
            "CASE WHEN true THEN 1 ELSE 0 END",
            "CASE WHEN false THEN 2 ELSE 3 END",
        ],
    );
    let [row] = &r.rows[..] else {
        panic!("expected one row, got {:?}", r.rows)
    };
    assert_eq!(row[0], cypherlite::Value::Int(1));
    assert_eq!(row[1], cypherlite::Value::Int(3));
}
