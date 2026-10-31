//! LABS-273 regression (ADR-0218): a comma-joined MATCH part with no
//! bound node orients by label cardinality, not written order. Pre-fix,
//! `(h:Hub)-[:R]->(t:Target)` expanded from the 200k-node `:Hub` end once
//! per outer row; post-fix the planner reverses the part to seed from the
//! 10-node `:Target` end. The cartesian product with the independent
//! parts is mandated by the spec and stays — only the scan end changes.

use std::time::{Duration, Instant};

use cypherlite::Value;

mod common;

/// 200 000 `:Hub` nodes, each with a `-[:S]-> (:Noise)` rel; only 100 of
/// them also reach one of 10 `:Target` nodes via `:R`. So
/// `(h:Hub)-[:R]->(t:Target)` has 100 answers behind 200k wrong-end
/// expansions, and each `:Seed` row re-runs the part.
fn setup(conn: &mut cypherlite::Connection) {
    conn.query("UNWIND range(0,9) AS i CREATE (:Target {id:i})")
        .unwrap();
    conn.query("CREATE (:Noise)").unwrap();
    conn.query("UNWIND range(1,50) AS i CREATE (:Seed {id:i})")
        .unwrap();
    conn.query("UNWIND range(1,200000) AS i CREATE (:Hub {id:i})-[:S]->(:Noise)")
        .unwrap();
    conn.query(
        "UNWIND range(1,100) AS i MATCH (h:Hub {id:i}) \
         CREATE (h)-[:R]->(:Target {id:i % 10})",
    )
    .unwrap();
}

#[test]
fn labs273_unbound_part_orients_toward_the_rare_label() {
    let mut conn = common::connection();
    setup(&mut conn);

    let selective_last = "MATCH (s:Seed), (h:Hub)-[:R]->(t:Target) RETURN count(*)";
    let selective_first = "MATCH (s:Seed), (t:Target)<-[:R]-(h:Hub) RETURN count(*)";
    assert_eq!(
        common::scalar_one(&mut conn, selective_last),
        Value::Int(5000)
    );
    assert_eq!(
        common::scalar_one(&mut conn, selective_first),
        Value::Int(5000)
    );

    // Timebound: post-fix the stats-oriented plan runs in tens of ms
    // (debug); pre-fix it scanned :Hub per :Seed row — ~11 s in release
    // at this scale. 30 s catches the regression where CI runs tests
    // (debug), with headroom for loaded runners.
    let started = Instant::now();
    let count = common::scalar_one(&mut conn, selective_last);
    let elapsed = started.elapsed();
    assert_eq!(count, Value::Int(5000));
    assert!(
        elapsed < Duration::from_secs(30),
        "LABS-273 repro took {elapsed:?}; the wrong-end expansion is back"
    );
}

#[test]
fn labs273_comma_match_results_are_unchanged() {
    let mut conn = common::connection();
    conn.query("UNWIND range(0,4) AS i CREATE (:A {id:i})")
        .unwrap();
    conn.query("UNWIND range(0,2) AS i CREATE (:B {id:i})")
        .unwrap();
    conn.query("MATCH (a:A {id:0}), (b:B {id:0}) CREATE (a)-[:R]->(b)")
        .unwrap();
    conn.query("MATCH (a:A {id:1}), (b:B {id:1}) CREATE (a)-[:R]->(b)")
        .unwrap();

    // Both written orientations agree on rows, not just counts.
    let forward = conn
        .query("MATCH (a:A)-[:R]->(b:B), (s:A) RETURN a.id AS a, b.id AS b, s.id AS s ORDER BY a, b, s")
        .unwrap();
    let backward = conn
        .query("MATCH (s:A), (b:B)<-[:R]-(a:A) RETURN a.id AS a, b.id AS b, s.id AS s ORDER BY a, b, s")
        .unwrap();
    assert_eq!(forward.rows, backward.rows);
    assert_eq!(forward.rows.len(), 2 * 5);

    // The cartesian product of independent parts is preserved by spec:
    // 5 :A rows × 2 rel-walks.
    let product = "MATCH (a:A), (x:A)-[:R]->(b:B) RETURN count(*)";
    assert_eq!(common::scalar_one(&mut conn, product), Value::Int(10));

    // Parts that DO share a binding (LABS-157 ground) keep working with
    // statistics on: the middle variable anchors the second part.
    let chained = "MATCH (a:A)-[:R]->(b:B), (b)-[:R]->(c:B) RETURN count(*)";
    assert_eq!(common::scalar_one(&mut conn, chained), Value::Int(0));
    conn.query("MATCH (b:B {id:0}) CREATE (b)-[:R]->(:B {id:9})")
        .unwrap();
    assert_eq!(common::scalar_one(&mut conn, chained), Value::Int(1));
}
