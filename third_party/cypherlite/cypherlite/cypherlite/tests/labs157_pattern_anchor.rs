//! LABS-157 regression: a comma-separated MATCH pattern whose node
//! variable is bound by an earlier pattern must orient toward that node,
//! so the executor's node-candidates anchor (a row-bound variable pins
//! the candidate set) applies. Pre-fix, `rewrite_part` oriented every
//! pattern with only the previous *clauses'* bindings, so a same-MATCH
//! bound node could end up last — and the pattern expanded from the
//! unbound end instead, re-walking its whole adjacency per input row.

use std::time::{Duration, Instant};

use cypherlite::Value;

mod common;

/// 20 genres / 20,000 movies / one user rating 100 of them, one IN_GENRE
/// per movie — the recommendations join shape.
fn setup(conn: &mut cypherlite::Connection) {
    // id() seeks, not per-row label scans, so the setup itself stays fast
    // (the unindexed label scan is LABS-179, not what this file tests).
    conn.query("UNWIND range(0,19) AS i CREATE (:G {name: 'g'+i})")
        .unwrap();
    conn.query("UNWIND range(0,19999) AS j CREATE (:M {title: 'm'+j})")
        .unwrap();
    conn.query("CREATE (:U {name: 'u'})").unwrap();
    conn.query(
        "MATCH (u) WHERE id(u)=20020 UNWIND range(0,99) AS k \
         WITH u, k, (k*31)%20000 AS m MATCH (mv) WHERE id(mv)=20+m \
         CREATE (u)-[:RATED]->(mv)",
    )
    .unwrap();
    conn.query(
        "UNWIND range(0,19999) AS j MATCH (m) WHERE id(m)=20+j \
         MATCH (g) WHERE id(g)=(j*7)%20 CREATE (m)-[:IN_GENRE]->(g)",
    )
    .unwrap();
}

#[test]
fn labs157_comma_pattern_orients_toward_the_bound_node() {
    let mut conn = common::connection();
    setup(&mut conn);

    let connected = "MATCH (u:U {name:'u'})-[r:RATED]->(m:M)-[:IN_GENRE]->(g:G) RETURN count(*)";
    let comma = "MATCH (u:U {name:'u'})-[r:RATED]->(m:M), (m)-[:IN_GENRE]->(g:G) \
                 RETURN count(*)";
    // Bound node written last, too — orientation must still reach it.
    let comma_reversed =
        "MATCH (u:U {name:'u'})-[r:RATED]->(m:M), (g:G)<-[:IN_GENRE]-(m) RETURN count(*)";

    assert_eq!(common::scalar_one(&mut conn, connected), Value::Int(100));
    assert_eq!(common::scalar_one(&mut conn, comma), Value::Int(100));
    assert_eq!(
        common::scalar_one(&mut conn, comma_reversed),
        Value::Int(100)
    );

    // Timebound: the fixed comma query runs in ~5 ms (debug); pre-fix it
    // expanded from the unbound genre end per row — ~13 s in this debug
    // build, ~2 s in release at this scale. 10 s catches the regression
    // where CI runs tests (debug), with headroom for loaded runners.
    let started = Instant::now();
    let count = common::scalar_one(&mut conn, comma);
    let elapsed = started.elapsed();
    assert_eq!(count, Value::Int(100));
    assert!(
        elapsed < Duration::from_secs(10),
        "LABS-157 repro took {elapsed:?}; the unbound-end expansion is back"
    );
}

#[test]
fn labs157_binding_semantics_are_unchanged() {
    let mut conn = common::connection();
    conn.query("CREATE (a:A)-[:R]->(b:B)").unwrap();

    // A non-node binding can never match a node pattern.
    let non_node = "UNWIND [1] AS m MATCH (a:A), (m)-[:R]->(b:B) RETURN count(*)";
    assert_eq!(common::scalar_one(&mut conn, non_node), Value::Int(0));

    // A Null binding (OPTIONAL MATCH miss) matches nothing, like Neo4j.
    let null_bound = "OPTIONAL MATCH (x:Missing) WITH x \
                      MATCH (a:A), (x)-[:R]->(b:B) RETURN count(*)";
    assert_eq!(common::scalar_one(&mut conn, null_bound), Value::Int(0));

    // Three patterns, each anchored on the previous one's binding.
    conn.query("MATCH (a:A) CREATE (:C)-[:R2]->(a)").unwrap();
    let chained = "MATCH (c:C)-[:R2]->(a), (a)-[:R]->(b), (b)-[:R]->(x:B) \
                   RETURN count(*)";
    assert_eq!(common::scalar_one(&mut conn, chained), Value::Int(0)); // b has no outgoing R
    conn.query("MATCH (b:B) CREATE (b)-[:R]->(:B2)").unwrap();
    let chained2 = "MATCH (c:C)-[:R2]->(a), (a)-[:R]->(b), (b)-[:R]->(x) \
                    RETURN count(*)";
    assert_eq!(common::scalar_one(&mut conn, chained2), Value::Int(1));
}
