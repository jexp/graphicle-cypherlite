//! Property-index keys share the normalized numeric equivalence of
//! `GroupKey` (memdb-merge node 37): a stored `Int(1)` and a stored
//! `Float(1.0)` occupy one index bucket, so an equality seek for `1.0`
//! finds `Int(1)` and vice versa — with an index present. The old
//! `to_cypher_literal()` string keys split the bucket (`"1"` vs `"1.0"`)
//! and the seek missed.
//!
//! Node 41 extended the same exact Int/Float rule to `=`/`<` evaluation
//! itself (`Value::eq_non_null`/`compare` route through
//! `cypherlite_model::canonical_key`'s `eq_i64_f64`/`cmp_i64_f64`), so a
//! `WHERE` residual, a pattern post-filter, `IN`, MERGE, DISTINCT, and an
//! index seek can never disagree — not even past `2^53`, where the old
//! `(i as f64) == f` cast rounded away low bits.

mod common;

use std::collections::BTreeMap;

use cypherlite::Value;
use cypherlite_core::VectorCoordKind;

use common::connection;

fn setup_with_index(c: &mut cypherlite::Connection) {
    c.query("CREATE INDEX n_x FOR (n:N) ON (n.x)").unwrap();
}

#[test]
fn stored_int_found_by_float_equality_seek() {
    let mut c = connection();
    setup_with_index(&mut c);
    c.query("CREATE (n:N {x: 1})").unwrap();
    let r = c.query("MATCH (n:N {x: 1.0}) RETURN n.x AS x").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
    // Same through a parameter (evaluated pattern map).
    let r = c
        .query_with_params(
            "MATCH (n:N {x: $p}) RETURN n.x AS x",
            std::collections::BTreeMap::from([("p".into(), Value::Float(1.0))]),
        )
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn stored_float_found_by_int_equality_seek() {
    let mut c = connection();
    setup_with_index(&mut c);
    c.query("CREATE (n:N {x: 1.0})").unwrap();
    let r = c.query("MATCH (n:N {x: 1}) RETURN n.x AS x").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Float(1.0)]]);
}

#[test]
fn range_seek_keeps_numeric_ordering_with_index_present() {
    let mut c = connection();
    setup_with_index(&mut c);
    c.query("CREATE (:N {x: 1}), (:N {x: 2}), (:N {x: 2.5})")
        .unwrap();
    // > 1.5 must not return Int(1) and must not miss Int(2) or Float(2.5).
    let r = c
        .query("MATCH (n:N) WHERE n.x > 1.5 RETURN n.x AS x ORDER BY x")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(2)], vec![Value::Float(2.5)]]);
    // >= 1.0 matches the Int(1) row too (cross-type lower bound).
    let r = c
        .query("MATCH (n:N) WHERE n.x >= 1.0 RETURN n.x AS x ORDER BY x")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Float(2.5)]
        ]
    );
    // Vice versa: a stored Float(1.0) is inside an integer upper bound.
    let mut c = connection();
    setup_with_index(&mut c);
    c.query("CREATE (:N {x: 1.0}), (:N {x: 3})").unwrap();
    let r = c
        .query("MATCH (n:N) WHERE n.x < 2 RETURN n.x AS x ORDER BY x")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Float(1.0)]]);
}

#[test]
fn merge_with_float_matches_stored_int_with_index_present() {
    let mut c = connection();
    setup_with_index(&mut c);
    c.query("CREATE (n:N {x: 1})").unwrap();
    // MERGE must see the Int(1) row through a Float(1.0) pattern — not
    // create a duplicate.
    c.query("MERGE (n:N {x: 1.0})").unwrap();
    let r = c.query("MATCH (n:N) RETURN count(n) AS c").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn int_float_equality_and_ordering_are_exact_past_2_pow_53() {
    let mut c = connection();
    // 9007199254740993 (= 2^53 + 1) is an exact i64 but rounds to
    // 9007199254740992.0 (= 2^53) under `as f64`; the old lossy cast in
    // `Value::eq_non_null` called them EQUAL, and the old lossy `compare`
    // cast called them Equal — both assertions below fail on that code.
    let r = c
        .query("RETURN 9007199254740993 = 9007199254740992.0 AS eq")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Bool(false)]]);
    // The integer is mathematically GREATER than the float 2^53.
    let r = c
        .query("RETURN 9007199254740993 > 9007199254740992.0 AS gt")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Bool(true)]]);
    let r = c
        .query("RETURN 9007199254740993 < 9007199254740992.0 AS lt")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Bool(false)]]);
    // The float DOES equal the integer it exactly denotes.
    let r = c
        .query("RETURN 9007199254740992 = 9007199254740992.0 AS eq")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Bool(true)]]);
    // `IN` routes through the same equality.
    let r = c
        .query("RETURN 9007199254740993 IN [9007199254740992.0] AS member")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Bool(false)]]);
    let r = c
        .query("RETURN 9007199254740992 IN [9007199254740992.0] AS member")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Bool(true)]]);
}

#[test]
fn where_equality_agrees_with_index_past_2_pow_53() {
    // A stored Int(2^53 + 1) must NOT match an equality predicate for the
    // nearby float 2^53 — with the index usable and on the pure scan path.
    for with_index in [true, false] {
        let mut c = connection();
        if with_index {
            setup_with_index(&mut c);
        }
        c.query("CREATE (:N {x: 9007199254740993})").unwrap();
        // Index-seek forms (the planner absorbs these into a GroupKey
        // property seek, exact since node 37).
        for q in [
            "MATCH (n:N) WHERE n.x = 9007199254740992.0 RETURN n.x AS x",
            "MATCH (n:N {x: 9007199254740992.0}) RETURN n.x AS x",
        ] {
            let r = c.query(q).unwrap();
            assert_eq!(
                r.rows,
                Vec::<Vec<Value>>::new(),
                "{q} (with_index={with_index})"
            );
        }
        // Pure scan/residual forms (not absorbable into a property seek, so
        // they evaluate through `Value::eq_non_null`): the old lossy
        // `(a as f64) == b` cast wrongly MATCHED the stored row here, which
        // is exactly the index/scan divergence this pins.
        for q in [
            "MATCH (n:N) WHERE n.x = 9007199254740992.0 OR n.x = -1 RETURN n.x AS x",
            "MATCH (n:N) WHERE n.x IN [9007199254740992.0] RETURN n.x AS x",
            "WITH 9007199254740992.0 AS f MATCH (n:N) WHERE n.x = f RETURN n.x AS x",
        ] {
            let r = c.query(q).unwrap();
            assert_eq!(
                r.rows,
                Vec::<Vec<Value>>::new(),
                "{q} (with_index={with_index})"
            );
        }
        // Positive control: the float 2^53 DOES match a stored Int(2^53),
        // through both the seek and the residual.
        c.query("CREATE (:N {x: 9007199254740992})").unwrap();
        for q in [
            "MATCH (n:N) WHERE n.x = 9007199254740992.0 RETURN n.x AS x",
            "MATCH (n:N) WHERE n.x IN [9007199254740992.0] RETURN n.x AS x",
        ] {
            let r = c.query(q).unwrap();
            assert_eq!(
                r.rows,
                vec![vec![Value::Int(9_007_199_254_740_992)]],
                "{q} (with_index={with_index})"
            );
        }
    }
}

#[test]
fn distinct_agrees_with_equality_past_2_pow_53() {
    let mut c = connection();
    // The pair `=` calls different lands in two DISTINCT buckets...
    let r = c
        .query("UNWIND [9007199254740993, 9007199254740992.0] AS x RETURN count(DISTINCT x) AS c")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
    // ...and the pair `=` calls equal lands in one.
    let r = c
        .query("UNWIND [9007199254740992, 9007199254740992.0] AS x RETURN count(DISTINCT x) AS c")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn vector_equality_and_distinct_share_the_elementwise_exact_rule() {
    // Vectors whose elements coincide numerically but not by variant (an
    // Int element vs the Float it denotes) can arrive via parameters; the
    // old derived-`PartialEq` vector arm in `eq_non_null` called them
    // unequal while DISTINCT/GROUP BY normalized the elements numerically.
    let int_backed = Value::Vector {
        coord: VectorCoordKind::Float64,
        values: vec![Value::Int(1)].into(),
    };
    let float_backed = Value::Vector {
        coord: VectorCoordKind::Float64,
        values: vec![Value::Float(1.0)].into(),
    };
    let two = Value::Vector {
        coord: VectorCoordKind::Float64,
        values: vec![Value::Float(2.0)].into(),
    };

    let mut c = connection();
    // `=` direction one: numerically equal elements -> true...
    let r = c
        .query_with_params(
            "RETURN $a = $b AS eq",
            BTreeMap::from([
                ("a".into(), int_backed.clone()),
                ("b".into(), float_backed.clone()),
            ]),
        )
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Bool(true)]]);
    // ...and DISTINCT agrees: one row, first-seen representation.
    let r = c
        .query_with_params(
            "UNWIND [$a, $b] AS v RETURN DISTINCT v",
            BTreeMap::from([("a".into(), int_backed.clone()), ("b".into(), float_backed)]),
        )
        .unwrap();
    assert_eq!(r.rows, vec![vec![int_backed.clone()]]);

    // Direction two: different element values -> `=` false...
    let r = c
        .query_with_params(
            "RETURN $a = $b AS eq",
            BTreeMap::from([("a".into(), int_backed.clone()), ("b".into(), two.clone())]),
        )
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Bool(false)]]);
    // ...and DISTINCT agrees: two buckets.
    let r = c
        .query_with_params(
            "UNWIND [$a, $b] AS v RETURN count(DISTINCT v) AS c",
            BTreeMap::from([("a".into(), int_backed), ("b".into(), two)]),
        )
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
}
