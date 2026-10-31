//! LABS-149 repro matrix: slice/subscript postfix on an aggregating
//! expression (`collect(x)[..n]`, `collect(x)[0]`) must evaluate the
//! aggregate first and then apply the postfix — in `RETURN`, in `WITH`,
//! and inside per-branch `RETURN`s of a `UNION ALL` (the Neo4j Browser
//! sidebar metadata query).

mod common;

use std::collections::BTreeMap;

use cypherlite::Value;

use common::{connection, list, string};

// The full 3-way UNION ALL sidebar metadata query from LABS-149: every
// branch slices its COLLECT with the same `$itemLimit` parameter, and a
// small limit must truncate each branch to a single-element list.
#[test]
fn labs_149_sidebar_query_truncates_with_param_limit() {
    let mut c = connection();
    // Two labels, one relationship type, two property keys: every branch
    // of the UNION ALL has more rows than the limit of 1 below.
    c.query("CREATE (:Alpha {name: 'a', tag: 'x'})-[:LINKS {since: 2024}]->(:Beta {name: 'b'})")
        .unwrap();

    let query = "CALL db.labels() YIELD label \
                 RETURN COLLECT(label)[..$itemLimit] AS result \
                 UNION ALL \
                 CALL db.relationshipTypes() YIELD relationshipType \
                 RETURN COLLECT(relationshipType)[..$itemLimit] AS result \
                 UNION ALL \
                 CALL db.propertyKeys() YIELD propertyKey \
                 RETURN COLLECT(propertyKey)[..$itemLimit] AS result";

    let params = BTreeMap::from([("itemLimit".to_string(), Value::Int(1))]);
    let r = c.query_with_params(query, params).unwrap();

    // One row per branch; each branch truncated to exactly one element.
    assert_eq!(r.rows.len(), 3, "rows: {:?}", r.rows);
    for row in &r.rows {
        match &row[0] {
            Value::List(items) => assert_eq!(
                items.len(),
                1,
                "branch must truncate to the $itemLimit of 1: {items:?}"
            ),
            other => panic!("branch result must be a list, got {other:?}"),
        }
    }

    // The truncated element of each branch is a member of that branch's
    // full, unsliced result (order is not guaranteed, so membership, not
    // position, is asserted).
    let labels = [string("Alpha"), string("Beta")];
    let rel_types = [string("LINKS")];
    let prop_keys = [string("name"), string("tag"), string("since")];
    let branch_sets = [&labels[..], &rel_types[..], &prop_keys[..]];

    let mut matched = [false; 3];
    for row in &r.rows {
        let Value::List(items) = &row[0] else {
            unreachable!("checked above")
        };
        let item = &items[0];
        let idx = branch_sets
            .iter()
            .position(|set| set.contains(item))
            .unwrap_or_else(|| panic!("truncated value {item:?} is in no branch's full set"));
        assert!(
            !matched[idx],
            "two rows came from the same branch ({idx}): {:?}",
            r.rows
        );
        matched[idx] = true;
    }
    assert!(matched.iter().all(|m| *m), "rows: {:?}", r.rows);

    // With a limit above every branch's size, no truncation happens and
    // the exact full sets come back (sorted for a deterministic assert).
    let params = BTreeMap::from([("itemLimit".to_string(), Value::Int(100))]);
    let r = c.query_with_params(query, params).unwrap();
    assert_eq!(r.rows.len(), 3, "rows: {:?}", r.rows);
    let mut branches: Vec<Vec<Value>> = r
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::List(items) => {
                let mut items = items.to_vec();
                items.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
                items
            }
            other => panic!("branch result must be a list, got {other:?}"),
        })
        .collect();
    branches.sort_by_key(Vec::len);
    assert_eq!(
        branches,
        vec![
            vec![string("LINKS")],
            vec![string("Alpha"), string("Beta")],
            vec![string("name"), string("since"), string("tag")],
        ]
    );
}

// `MATCH (n) RETURN collect(n.name)[..1]` — the minimal LABS-149 repro:
// a sliced aggregate straight off a MATCH.
#[test]
fn labs_149_match_collect_slice() {
    let mut c = connection();
    c.query("CREATE (:N {name: 'a'}), (:N {name: 'b'}), (:N {name: 'c'})")
        .unwrap();

    let r = c
        .query("MATCH (n) RETURN collect(n.name)[..1] AS r")
        .unwrap();
    assert_eq!(r.rows.len(), 1, "rows: {:?}", r.rows);
    let Value::List(items) = &r.rows[0][0] else {
        panic!("expected a list, got {:?}", r.rows[0][0]);
    };
    assert_eq!(items.len(), 1, "slice must keep one element: {items:?}");
    assert!(
        [string("a"), string("b"), string("c")].contains(&items[0]),
        "sliced element must be one of the collected names: {items:?}"
    );

    // Deterministic variant: an ordered collect slices to the first name.
    let r = c
        .query("MATCH (n) WITH n ORDER BY n.name RETURN collect(n.name)[..2] AS r")
        .unwrap();
    assert_eq!(
        r.rows,
        vec![vec![list(vec![string("a"), string("b")])]],
        "rows: {:?}",
        r.rows
    );
}

// Subscript (not slice) on a collect: `collect(x)[0]`.
#[test]
fn labs_149_collect_subscript_index() {
    let mut c = connection();

    let r = c
        .query("UNWIND [1, 2, 3] AS x RETURN collect(x)[0] AS r")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);

    // Deterministic order via ORDER BY: the subscript picks the head.
    let r = c
        .query("UNWIND [3, 1, 2] AS x WITH x ORDER BY x RETURN collect(x)[0] AS r")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);

    // Out-of-range subscript on the aggregated list is null.
    let r = c
        .query("UNWIND [1, 2, 3] AS x RETURN collect(x)[5] AS r")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::Null]]);
}

// The WITH form of the sliced aggregate: the postfix rides on the
// aggregating expression inside a WITH projection.
#[test]
fn labs_149_sliced_collect_in_with_projection() {
    let mut c = connection();

    let r = c
        .query("UNWIND [1, 2, 3] AS x WITH collect(x)[..1] AS c RETURN c")
        .unwrap();
    assert_eq!(r.rows, vec![vec![list(vec![Value::Int(1)])]]);

    // ... and a collect carried through WITH slices in the next RETURN.
    let r = c
        .query("UNWIND [1, 2, 3] AS x WITH collect(x) AS c RETURN c[..2] AS r")
        .unwrap();
    assert_eq!(r.rows, vec![vec![list(vec![Value::Int(1), Value::Int(2)])]]);
}

// The aggregate-in-subscript-index shape stays rejected: the index
// position of `n[...]` is not a place an aggregate may appear.
#[test]
fn labs_149_aggregate_in_subscript_index_still_errors() {
    let mut c = connection();
    c.query("CREATE (:N {name: 'a'})").unwrap();

    let err = c.query("MATCH (n) RETURN n[count(*)] AS r").unwrap_err();
    assert!(
        err.to_string().contains("AmbiguousAggregationExpression"),
        "error should reject the aggregate in the subscript, got: {err}"
    );
}
