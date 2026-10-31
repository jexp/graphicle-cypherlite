//! Checked integer arithmetic in the evaluator
//! (memdb-merge/30-checked-int-ops).
//!
//! The evaluator's generic integer ops (Div, Mod, num_op's Sub/Mul) and
//! range()'s step accumulation used raw i64 arithmetic that unwound on
//! overflow in debug builds and wrapped (or looped forever) in release.
//! `abs(i64::MIN)` was the same class. An embedded engine must never
//! unwind into the host: overflow now surfaces as an honest
//! "integer out of range" query error — Neo4j errors on integer overflow
//! (never wraps), and node 03 established that phrasing for unary minus.
//!
//! ADR-0175 (LABS-178) carved out the two cases where Neo4j uses plain
//! Java operators with no overflow check: `i64::MIN / -1` wraps to
//! `i64::MIN` and `i64::MIN % -1` is 0 — replicated here deliberately for
//! cross-engine portability (they live in `in_range_integer_arithmetic_
//! still_works` below). range()'s size computation still errors on i64
//! overflow, but a range whose size is representable no longer errors just
//! because the *last* step would leave i64 (Neo4j returns it: e.g.
//! `range(i64::MAX, i64::MAX)` is `[i64::MAX]`).
//!
//! Every OVERFLOW_QUERIES entry panicked in a debug build before the fix.
//!
//! memdb-merge/31-align-add-overflow: add()'s (Int, Int) arm additionally
//! *saturated* on overflow (`i64::MIN + -1` → `i64::MIN`); it now shares the
//! same checked contract and honest error.

use std::panic::{catch_unwind, AssertUnwindSafe};

use cypherlite::{Database, Value};

const OVERFLOW_QUERIES: &[&str] = &[
    // Contract repros: Mul on pure integers, and Sub on the
    // temporal-derived i64::MIN route (civil duration.between months).
    r#"RETURN 4611686018427387904 * 4"#,
    r#"RETURN (duration.between(date('+768614336404564650-09-01'), date('+0000-01-01'))).months - 1"#,
    // Sub / Mul variants at the i64 edges.
    r#"RETURN -9223372036854775807 - 2"#,
    r#"RETURN (-9223372036854775807 - 1) - 1"#,
    r#"RETURN (-9223372036854775807 - 1) * -1"#,
    r#"RETURN 9223372036854775807 * 2"#,
    // Add at both i64 edges (memdb-merge/31-align-add-overflow): add()'s
    // (Int, Int) arm saturated silently; it must error like Sub/Mul/Div/Mod.
    // i64::MIN / MAX are not literal-representable, so derive MIN via Sub.
    r#"RETURN (-9223372036854775807 - 1) + -1"#,
    r#"RETURN (-9223372036854775807 - 1) + (-9223372036854775807 - 1)"#,
    r#"RETURN 9223372036854775807 + 1"#,
    r#"RETURN 9223372036854775807 + 9223372036854775807"#,
    // range() size overflow (ADR-0175): `end - start` or `diff/step + 1`
    // leaves i64 — Neo4j raises 22N28 from its subtractExact/addExact.
    r#"RETURN range(-9223372036854775807 - 1, 9223372036854775807)"#,
    r#"RETURN range(0, 9223372036854775807)"#,
    // abs(i64::MIN), again via the temporal-derived route.
    r#"RETURN abs(duration.between(date('+768614336404564650-09-01'), date('+0000-01-01')).months)"#,
];

#[test]
fn overflowing_integer_ops_error() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    for query in OVERFLOW_QUERIES {
        let err = match conn.query(query) {
            Ok(_) => panic!("query must fail with an error: {query}"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("integer out of range"),
            "query must fail with an honest range error: {query} (got: {err})"
        );
    }
}

/// No unwind may escape the public query path for those inputs. A fresh
/// database per query keeps a hypothetically poisoned engine from masking a
/// later panic.
#[test]
fn overflowing_integer_ops_do_not_unwind_through_public_api() {
    for query in OVERFLOW_QUERIES {
        let mut conn = Database::open(":memory:").unwrap().connect().unwrap();
        let outcome = catch_unwind(AssertUnwindSafe(|| conn.query(query)));
        let result = outcome.unwrap_or_else(|_| panic!("query panicked: {query}"));
        assert!(result.is_err(), "query must fail with an error: {query}");
    }
}

/// In-range controls: boundary-adjacent values whose results are
/// representable keep working, with exact results.
#[test]
fn in_range_integer_arithmetic_still_works() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let controls: &[(&str, i64)] = &[
        ("RETURN 7 / 2", 3),
        ("RETURN -7 / 2", -3),
        ("RETURN (-9223372036854775807 - 1) / 1", i64::MIN),
        // ADR-0175: Neo4j's plain Java `/` and `%` carry no overflow check,
        // so MIN / -1 wraps to MIN and MIN % -1 is 0 (a deliberate,
        // mathematically wrong wrap replicated for cross-engine parity).
        ("RETURN (-9223372036854775807 - 1) / -1", i64::MIN),
        ("RETURN (-9223372036854775807 - 1) % -1", 0),
        ("RETURN 7 % 3", 1),
        ("RETURN (-9223372036854775807 - 1) % 1", 0),
        ("RETURN 1000000007 * 1000000009", 1000000016000000063),
        ("RETURN 4611686018427387904 * 1", 4611686018427387904),
        ("RETURN -9223372036854775807 - 0", -9223372036854775807),
        (
            "RETURN (-9223372036854775807 - 1) + 1",
            -9223372036854775807,
        ),
        ("RETURN 9223372036854775807 + 0", 9223372036854775807),
        ("RETURN (-9223372036854775807 - 1) + 0", i64::MIN),
        ("RETURN 9223372036854775806 + 1", 9223372036854775807),
        ("RETURN 9223372036854775807 + -9223372036854775807", 0),
        ("RETURN 9223372036854775807 - 9223372036854775807", 0),
        ("RETURN abs(-9223372036854775807)", 9223372036854775807),
        ("RETURN abs(0 - 9223372036854775807)", 9223372036854775807),
    ];
    for &(query, expected) in controls {
        let result = conn
            .query(query)
            .unwrap_or_else(|e| panic!("{query} must succeed: {e}"));
        assert_eq!(result.rows[0][0].as_int(), Some(expected), "for {query}");
    }
}

/// range() controls near the boundary whose step accumulation stays in i64.
#[test]
fn in_range_range_controls_still_work() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let controls: &[(&str, &[i64])] = &[
        ("RETURN range(5, -5, -3)", &[5, 2, -1, -4]),
        ("RETURN range(0, 10, 9223372036854775807)", &[0]),
        (
            "RETURN range(-9223372036854775807, -9223372036854775807)",
            &[-9223372036854775807],
        ),
        (
            "RETURN range(9223372036854775806, 9223372036854775806)",
            &[9223372036854775806],
        ),
        // ADR-0175: a range whose size is representable no longer errors
        // just because the next step would leave i64 (Neo4j returns it).
        (
            "RETURN range(9223372036854775807, 9223372036854775807)",
            &[9223372036854775807],
        ),
        (
            "RETURN range(9223372036854775800, 9223372036854775807, 9223372036854775807)",
            &[9223372036854775800],
        ),
        (
            "RETURN range(-9223372036854775807 - 1, -9223372036854775807 - 1, -9223372036854775807)",
            &[i64::MIN],
        ),
        ("RETURN range(3, 1)", &[]),
    ];
    for &(query, expected) in controls {
        let result = conn
            .query(query)
            .unwrap_or_else(|e| panic!("{query} must succeed: {e}"));
        let Value::List(items) = &result.rows[0][0] else {
            panic!("expected a list for {query}, got {:?}", result.rows[0][0]);
        };
        let ints: Vec<i64> = items.iter().map(|v| v.as_int().unwrap()).collect();
        assert_eq!(ints, expected, "for {query}");
    }
}

/// Divide/modulo by zero keep their distinct pre-existing errors.
#[test]
fn division_and_modulo_by_zero_still_error() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    for query in ["RETURN 1 / 0", "RETURN 1 % 0"] {
        assert!(conn.query(query).is_err(), "{query} must fail");
    }
}
