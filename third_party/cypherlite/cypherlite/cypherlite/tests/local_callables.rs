//! CIP-231 local callables: `DEFINE PROCEDURE`/`DEFINE FUNCTION` behind
//! the ADR-0371 `local-callables` preview gate. The queries and every
//! expected table are the CIP's own "In this proposal: Basics" example
//! tables (doc-derived: the first implementation is not in a release, so
//! there is no live instance to verify against).

mod common;

use cypherlite::Value;

/// The CIP-231 preview gate is process-level (ADR-0371: `Database::open`
/// resolves `CYPHERLITE_PREVIEW` once), and an integration-test crate is
/// its own process -- set it before any store opens and keep every test
/// in this crate behind the gate.
fn gated_connection() -> cypherlite::Connection {
    std::env::set_var("CYPHERLITE_PREVIEW", "local-callables");
    common::connection()
}

/// The CIP's band fixture: The ABCDs with Alice, Barb, Cheng and Donni in
/// it (the `createPersonNode` example's own data).
fn band_store() -> cypherlite::Connection {
    let mut c = gated_connection();
    c.query(
        "CREATE (b:Band {name: \"The ABCDs\"}) \
         UNWIND [\"Alice\", \"Barb\", \"Cheng\", \"Donni\"] AS member \
         CREATE (p:Person {name: member})-[:IN]->(b)",
    )
    .unwrap();
    c
}

/// The one column of a one-column result, as strings.
fn strings(result: &cypherlite::QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::String(s) => s.to_string(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect()
}

#[test]
fn local_procedure_creates_per_row() {
    // The CIP's `createPersonNode` example table: the band, all four
    // members, in order.
    let mut c = gated_connection();
    let result = c
        .query(
            "DEFINE PROCEDURE createPersonNode(name) { \
               CREATE (p:Person {name: name}) \
               RETURN p AS person \
             } \
             CREATE (b:Band {name: \"The ABCDs\"}) \
             UNWIND [\"Alice\", \"Barb\", \"Cheng\", \"Donni\"] AS bandMemberName \
             CALL createPersonNode(bandMemberName) YIELD person AS bandMember \
             CREATE (bandMember)-[:IN]->(b) \
             RETURN b.name AS band, bandMember.name AS member",
        )
        .unwrap();
    let rows: Vec<String> = result
        .rows
        .iter()
        .map(|row| format!("{}/{}", show(&row[0]), show(&row[1])))
        .collect();
    assert_eq!(
        rows,
        vec![
            "The ABCDs/Alice",
            "The ABCDs/Barb",
            "The ABCDs/Cheng",
            "The ABCDs/Donni",
        ]
    );
}

/// The int-column extractor of the CIP table tests (the `show` family:
/// one canonical per-shape extractor, not one per test).
fn ints(result: &cypherlite::QueryResult) -> Vec<i64> {
    result
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Int(n) => *n,
            other => panic!("expected an int, got {other:?}"),
        })
        .collect()
}

fn show(value: &Value) -> String {
    match value {
        Value::String(s) => s.to_string(),
        other => panic!("expected a string, got {other:?}"),
    }
}

#[test]
fn typed_signatures_accept_the_bare_type_form() {
    // The CIP-231 implementation grammar's field signatures
    // (`symbolicNameString (typed? type)?`) make the `::`/TYPED marker
    // optional: the doc's own canonical output spelling
    // `:: (count INTEGER)` and a bare input `(band STRING)` must parse
    // exactly like the marked forms.
    let mut c = gated_connection();
    let result = c
        .query(
            "DEFINE PROCEDURE getNumberOfBandMembers(band STRING) \
               :: (count INTEGER) { \
                 MATCH (b:Band {name: band})<-[:IN]-(m) \
                 RETURN count(m) AS count \
               } \
               CREATE (:Band {name: 'Nirvana'})<-[:IN]-(:Person {name: 'Krist'}) \
               CALL getNumberOfBandMembers('Nirvana') YIELD count \
               RETURN count",
        )
        .unwrap();
    assert_eq!(ints(&result), vec![1]);
}

#[test]
fn union_arms_yield_their_own_items_not_the_head_arm_stamp() {
    // Rust round 7: the yield mapping used to stamp the head arm's RETURN
    // items onto every UNION arm (`RETURN 1 AS a UNION RETURN 2 AS a`
    // yielded `1, 1`); each arm now contributes its own expressions.
    let mut c = gated_connection();
    let result = c
        .query(
            "DEFINE PROCEDURE both() :: (a INTEGER) { \
                 RETURN 1 AS a UNION RETURN 2 AS a \
             } \
             CALL both() YIELD a \
             RETURN a",
        )
        .unwrap();
    let rows: Vec<i64> = result
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::Int(n) => *n,
            other => panic!("expected an int, got {other:?}"),
        })
        .collect();
    assert_eq!(rows, vec![1, 2]);
}

#[test]
fn union_arms_with_diverging_column_names_fail_with_a_typed_error() {
    // Rust round 8 / stellar F-7-1: the per-arm yield selection would
    // panic on arms whose terminal RETURN names diverge from the head
    // arm's (the engine-level UNION column check runs after this
    // rewrite); the define-time twin rejects them with the UNION error.
    let mut c = gated_connection();
    let err = c
        .query(
            "DEFINE PROCEDURE p() :: (a INTEGER) { \
                 RETURN 1 AS a UNION RETURN 2 AS b \
             } \
             CALL p() YIELD a \
             RETURN a",
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("all UNION branches must have the same column names"),
        "expected the UNION column error, got: {err}"
    );
}

#[test]
fn implicit_args_call_walls_pay_the_clone_budget() {
    // Security round 11: each `CALL f` site (implicit args, a >=1-param
    // procedure) deep-clones the callable body at the lookup; the charge
    // runs at clone time (the lookup site), so a wall of fall-through
    // call sites cannot accumulate un-budgeted clones.
    let mut wall = String::from("DEFINE PROCEDURE p(x) :: (v INTEGER) { RETURN x AS v } ");
    for _ in 0..40_000 {
        wall.push_str("CALL p YIELD v WITH v AS keep ");
    }
    wall.push_str("RETURN 1");
    let mut c = gated_connection();
    let err = c.query(&wall).unwrap_err().to_string();
    assert!(
        err.contains("expansion exceeds"),
        "expected the expansion-budget error, got: {err}"
    );
}

#[test]
fn wide_definition_body_hits_the_expansion_budget() {
    // Security: each rewritten call site deep-clones its definition's
    // body, so a chained-definition bomb compounds exponentially. The
    // per-compilation-unit budget turns it into a typed error instead
    // of an OOM (the preview-gated store still serves untrusted
    // queries).
    // A wide single body (no chaining -- the runtime evaluation cost of
    // chained definitions is bounded by the executor's per-statement
    // nested-evaluation budget, ADR-0395, which closed LABS-416): a
    // 60 001-element list literal (~120k AST nodes, depth 2 -- the
    // ADR-0079 depth guard stays out of the way) called twice clones
    // past the node budget. The chained shape itself lives in the
    // sibling test below.
    let wide = format!("[1{}]", ", 1".repeat(60_000));
    let bomb = format!(
        "DEFINE FUNCTION wide() = {wide} \
         RETURN wide() + wide() AS v"
    );
    let mut c = gated_connection();
    let err = c.query(&bomb).unwrap_err().to_string();
    assert!(
        err.contains("expansion exceeds"),
        "expected the expansion-budget error, got: {err}"
    );
}

/// The LABS-416 residue, closed by ADR-0395: one call site per level
/// keeps the compile-side clone weight linear, so `MAX_CLONED_NODES`
/// never fires — but each level's `UNWIND` doubles the nested
/// statement-engine passes per level at execution. Depth 19 is the
/// measured case (524 288 leaf-body executions, 16.3 s in release before
/// the budget); the per-statement nested-evaluation budget turns it into
/// the typed error at the 100 000th pass, long before the fan completes.
#[test]
fn chained_definition_bombs_hit_the_nested_evaluation_budget() {
    let mut bomb = String::from("DEFINE PROCEDURE p0() { RETURN 1 AS v } ");
    for k in 1..=19 {
        bomb.push_str(&format!(
            "DEFINE PROCEDURE p{k}() {{ UNWIND [0, 1] AS u{k} \
             CALL p{}() YIELD v RETURN v }} ",
            k - 1
        ));
    }
    bomb.push_str("CALL p19() YIELD v RETURN count(v) AS c");
    let mut c = gated_connection();
    let err = c.query(&bomb).unwrap_err().to_string();
    // The one shared budget-error assertion (test-support): the exact
    // authored message, single-sourced from NESTED_EVAL_BUDGET, and the
    // kind's parse-back to the ADR-0395 row.
    cypherlite_test_support::nested_eval::assert_nested_eval_budget_error(&err);
}

#[test]
fn simple_function_matches_the_cip_table() {
    let mut c = gated_connection();
    let result = c
        .query(
            "DEFINE FUNCTION isEven(num) = \
               CASE WHEN num % 2 = 0 THEN true ELSE false END \
             UNWIND range(1,4) AS x \
             RETURN x, isEven(x) AS isEven",
        )
        .unwrap();
    let rows: Vec<(i64, bool)> = result
        .rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Int(x), Value::Bool(b)) => (*x, *b),
            other => panic!("expected (int, bool), got {other:?}"),
        })
        .collect();
    assert_eq!(rows, vec![(1, false), (2, true), (3, false), (4, true)]);
}

#[test]
fn optional_parameters_default_per_the_cip() {
    // The CIP's `isEven(num, radix = 2)` table, including the radix-3
    // call: one evaluation per argument and invocation.
    let mut c = gated_connection();
    let result = c
        .query(
            "DEFINE FUNCTION isEven(num, radix = 2) = \
               CASE WHEN num % radix = 0 THEN true ELSE false END \
             UNWIND range(1,4) AS x \
             RETURN x, isEven(x) AS even2, isEven(x, 3) AS even3",
        )
        .unwrap();
    let rows: Vec<(bool, bool)> = result
        .rows
        .iter()
        .map(|row| match (&row[1], &row[2]) {
            (Value::Bool(a), Value::Bool(b)) => (*a, *b),
            other => panic!("expected (bool, bool), got {other:?}"),
        })
        .collect();
    assert_eq!(
        rows,
        vec![(false, false), (true, false), (false, true), (true, false),]
    );
}

#[test]
fn query_body_function_nulls_on_zero_rows() {
    // The CIP's `getBandName` scalar-subquery semantics: zero rows -> null,
    // matched rows -> the first row's value.
    let mut c = band_store();
    let result = c
        .query(
            "DEFINE FUNCTION getBandName(name) { \
               OPTIONAL MATCH (p:Person {name: name})-[:IN]->(b:Band) \
               RETURN b.name LIMIT 1 \
             } \
             UNWIND [\"Billy\", \"Donni\", \"Barb\", \"Cheer\"] AS name \
             RETURN name, getBandName(name) AS band",
        )
        .unwrap();
    let rows: Vec<(String, Option<String>)> = result
        .rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::String(name), Value::Null) => (name.to_string(), None),
            (Value::String(name), Value::String(band)) => {
                (name.to_string(), Some(band.to_string()))
            }
            other => panic!("expected (string, string|null), got {other:?}"),
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("Billy".into(), None),
            ("Donni".into(), Some("The ABCDs".into())),
            ("Barb".into(), Some("The ABCDs".into())),
            ("Cheer".into(), None),
        ]
    );
}

#[test]
fn query_body_function_aggregates() {
    // The CIP's `getNumberOfBandMembers` example (4 members of The ABCDs).
    let mut c = band_store();
    let result = c
        .query(
            "DEFINE FUNCTION getNumberOfBandMembers(band) { \
               MATCH (p:Person)-[:IN]->(b:Band {name: band}) \
               RETURN count(p) \
             } \
             RETURN getNumberOfBandMembers(\"The ABCDs\") AS num",
        )
        .unwrap();
    let rows: Vec<i64> = result
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Int(i) => i,
            ref other => panic!("expected an integer, got {other:?}"),
        })
        .collect();
    assert_eq!(rows, vec![4]);
}

#[test]
fn functions_invoke_each_other_and_nest() {
    // The CIP's `hasEvenNumberOfDigits` example (both the flat and the
    // nested-definition spellings produce the same table).
    let expected = vec![
        (-11, true),
        (-1, false),
        (0, false),
        (17, true),
        (100, false),
        (1234, true),
        (3516835, false),
    ];
    for query in [
        "DEFINE FUNCTION numberOfDigits(num) = \
           CASE WHEN num = 0 THEN 1 \
                ELSE toInteger(ceil(log10(abs(num)+1))) END \
         DEFINE FUNCTION isEven(num) = \
           CASE WHEN num % 2 = 0 THEN true ELSE false END \
         DEFINE FUNCTION hasEvenNumberOfDigits(num) = isEven(numberOfDigits(num)) \
         UNWIND [-11, -1, 0, 17, 100, 1234, 3516835] AS x \
         RETURN x, hasEvenNumberOfDigits(x) AS e",
        "DEFINE FUNCTION hasEvenNumberOfDigits(num) { \
           DEFINE FUNCTION numberOfDigits(num) = \
             CASE WHEN num = 0 THEN 1 \
                  ELSE toInteger(ceil(log10(abs(num)+1))) END \
           DEFINE FUNCTION isEven(num) = \
             CASE WHEN num % 2 = 0 THEN true ELSE false END \
           RETURN isEven(numberOfDigits(num)) LIMIT 1 \
         } \
         UNWIND [-11, -1, 0, 17, 100, 1234, 3516835] AS x \
         RETURN x, hasEvenNumberOfDigits(x) AS e",
    ] {
        let mut c = gated_connection();
        let result = c.query(query).unwrap();
        let rows: Vec<(i64, bool)> = result
            .rows
            .iter()
            .map(|row| match (&row[0], &row[1]) {
                (Value::Int(x), Value::Bool(b)) => (*x, *b),
                other => panic!("expected (int, bool), got {other:?}"),
            })
            .collect();
        assert_eq!(rows, expected);
    }
}

#[test]
fn call_by_value_evaluates_arguments_once() {
    // The CIP's `foo(a) = a + " = " + a` call-by-value pin: the argument
    // is evaluated once, so the equation holds.
    let mut c = gated_connection();
    let result = c
        .query("DEFINE FUNCTION foo(a) = a + \" = \" + a RETURN foo(rand()) AS str")
        .unwrap();
    let Value::String(str) = &result.rows[0][0] else {
        panic!("expected a string");
    };
    let (left, right) = str.split_once(" = ").expect("the equation separator");
    assert_eq!(left, right, "the argument must be evaluated exactly once");
}

#[test]
fn procedure_call_by_value_evaluates_arguments_once() {
    let mut c = gated_connection();
    let result = c
        .query(
            "DEFINE PROCEDURE foo(a) { RETURN a + \" = \" + a AS str } \
             CALL foo(rand()) YIELD str RETURN str",
        )
        .unwrap();
    let Value::String(str) = &result.rows[0][0] else {
        panic!("expected a string");
    };
    let (left, right) = str.split_once(" = ").expect("the equation separator");
    assert_eq!(left, right, "the argument must be evaluated exactly once");
}

#[test]
fn procedures_call_procedures() {
    let mut c = gated_connection();
    let result = c
        .query(
            "DEFINE PROCEDURE inner(x) { RETURN x * 2 AS d } \
             DEFINE PROCEDURE outer(x) { CALL inner(x) YIELD d RETURN d AS r } \
             CALL outer(21) YIELD r RETURN r",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], Value::Int(42));
}

#[test]
fn void_procedure_needs_no_yield() {
    let mut c = gated_connection();
    c.query(
        "DEFINE PROCEDURE addNode(name) { CREATE (p:P {name: name}) FINISH } \
         CALL addNode(\"x\") RETURN count { (p:P) } AS c",
    )
    .unwrap();
    let result = c.query("MATCH (p:P) RETURN p.name AS n").unwrap();
    assert_eq!(strings(&result), vec!["x".to_string()]);
}

#[test]
fn procedure_and_function_definitions_inside_a_subquery() {
    // The CIP's "Local procedure and local function definition in
    // subquery" example: evens/odds split driven by a local function and
    // a local procedure defined inside the scoped CALL body.
    let mut c = gated_connection();
    let result = c
        .query(
            "CREATE (a:Person {name: \"Alice\"}), (b:Person {name: \"Barb\"}) \
             WITH a, b \
             CALL (a, b) { \
               DEFINE FUNCTION isEven(num) = \
                 CASE WHEN num % 2 = 0 THEN true ELSE false END \
               DEFINE PROCEDURE tag(n, t) { CREATE (p:Tag {name: n.name + t}) FINISH } \
               UNWIND [1, 2] AS i \
             CALL (i, a, b) { \
               WHEN isEven(i) THEN { CALL tag(a, \"-even\") RETURN \"even\" AS arm } \
               ELSE { CALL tag(b, \"-odd\") RETURN \"odd\" AS arm } \
             } \
             RETURN arm } \
             RETURN arm",
        )
        .unwrap();
    assert_eq!(
        strings(&result),
        vec!["odd".to_string(), "even".to_string()]
    );
}

#[test]
fn definitions_do_not_escape_their_block() {
    // The CIP's "not visible" rule: a definition inside one NEXT stage /
    // UNION arm is invisible outside it; the call falls through to the
    // engine's unknown-procedure error.
    let mut c = gated_connection();
    let error = c
        .query(
            "{ DEFINE PROCEDURE foo() { RETURN 1 AS a } CALL foo() YIELD a RETURN a } \
             NEXT CALL foo() YIELD a AS b RETURN b",
        )
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("ProcedureNotFound") || error.contains("not found"),
        "{error}"
    );
}

#[test]
fn no_recursion() {
    let mut c = gated_connection();
    let error = c
        .query("DEFINE FUNCTION f() { RETURN f() AS x LIMIT 1 } RETURN f()")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("recursive calls and calls of parent definitions"),
        "{error}"
    );
}

#[test]
fn use_inside_bodies_is_rejected() {
    let mut c = gated_connection();
    let error = c
        .query("DEFINE PROCEDURE p() { USE neo4j RETURN 1 AS a } CALL p() YIELD a RETURN a")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("USE clause is not supported in local procedure definitions."),
        "{error}"
    );
}

#[test]
fn in_transactions_combination_is_rejected() {
    let mut c = gated_connection();
    let error = c
        .query("DEFINE FUNCTION f() = 1 CALL { RETURN 1 AS a } IN TRANSACTIONS RETURN f()")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(
            "'CALL { ... } IN TRANSACTIONS' is not supported in combination with 'DEFINE'."
        ),
        "{error}"
    );
}

#[test]
fn local_callable_shadows_the_built_in_name() {
    // SHADOW-BUILT-IN-CALLABLE-PER-COMPILATION-UNIT: inside the unit, the
    // local definition wins.
    let mut c = gated_connection();
    let result = c
        .query("DEFINE FUNCTION abs(x) = x * -1 UNWIND [1, 2] AS n RETURN abs(n) AS a")
        .unwrap();
    let rows: Vec<i64> = result
        .rows
        .iter()
        .map(|row| match row[0] {
            Value::Int(i) => i,
            ref other => panic!("expected an integer, got {other:?}"),
        })
        .collect();
    assert_eq!(rows, vec![-1, -2]);
}
