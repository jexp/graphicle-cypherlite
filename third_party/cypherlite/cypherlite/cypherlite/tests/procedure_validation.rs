//! Compile-time `CALL` validation must cover `NEXT` stages: a procedure
//! call in a later stage is checked against the canonical signature
//! registry exactly like a first-stage call. Regression tests for the bug
//! where `validate_calls` iterated only `logical.parts` without recursing
//! into `plan.next`, so a `NEXT`-stage `CALL … YIELD <undeclared>` silently
//! bound null instead of failing with `UnknownProcedureOutput`.

mod common;

use common::connection;

#[test]
fn next_stage_yield_of_undeclared_column_fails_validation() {
    let mut c = connection();
    let error = c
        .query("CREATE (:L) RETURN 1 AS x NEXT CALL db.labels() YIELD nope RETURN nope")
        .unwrap_err();
    assert!(
        error.to_string().contains("UnknownProcedureOutput"),
        "NEXT-stage undeclared YIELD must fail at compile time, got: {error}"
    );
    assert!(
        error.to_string().contains("nope"),
        "error should name the unknown column, got: {error}"
    );
}

#[test]
fn next_stage_valid_call_runs() {
    let mut c = connection();
    c.query("CREATE (:L) RETURN 1 AS x NEXT CALL db.labels() YIELD label RETURN label")
        .unwrap();
    let result = c
        .query("CREATE (:L2) RETURN 1 AS x NEXT CALL db.labels() YIELD label RETURN label")
        .unwrap();
    assert_eq!(result.header.columns, ["label"]);
    // The created labels are visible to the NEXT-stage call.
    let labels: Vec<&str> = result
        .rows
        .iter()
        .filter_map(|row| row[0].as_string())
        .collect();
    assert!(labels.contains(&"L"), "labels: {labels:?}");
    assert!(labels.contains(&"L2"), "labels: {labels:?}");
}

#[test]
fn next_stage_call_with_bad_arity_fails_validation() {
    let mut c = connection();
    let error = c.query("RETURN 1 AS x NEXT CALL db.labels(1)").unwrap_err();
    assert!(
        error.to_string().contains("InvalidNumberOfArguments"),
        "NEXT-stage arity violation must fail at compile time, got: {error}"
    );
}

#[test]
fn nested_call_yield_of_undeclared_column_fails_validation() {
    // Regression: `validate_calls` walks the full clause tree — a `CALL`
    // nested in a `CALL { … }` subquery is checked against the signature
    // registry exactly like a top-level call. Before the full descent a
    // nested `CALL … YIELD <undeclared>` silently bound null instead of
    // failing with `UnknownProcedureOutput`.
    let mut c = connection();
    let error = c
        .query("CALL { CALL db.labels() YIELD nope RETURN nope } RETURN 1 AS x")
        .unwrap_err();
    assert!(
        error.to_string().contains("UnknownProcedureOutput"),
        "nested undeclared YIELD must fail at compile time, got: {error}"
    );
    assert!(
        error.to_string().contains("nope"),
        "error should name the unknown column, got: {error}"
    );
}

#[test]
fn nested_valid_call_runs() {
    let mut c = connection();
    c.query("CREATE (:L)").unwrap();
    let result = c
        .query("CALL { CALL db.labels() YIELD label RETURN label } RETURN 1 AS x")
        .unwrap();
    assert_eq!(result.header.columns, ["x"]);
}
