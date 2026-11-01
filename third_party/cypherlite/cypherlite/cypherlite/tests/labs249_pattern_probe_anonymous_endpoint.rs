//! LABS-249 regression: the pattern-predicate existence probe must
//! anchor on a row-bound endpoint and check a free (anonymous) endpoint
//! per reached node — never enumerate the free endpoint's candidates
//! (pre-fix that resolve did a full label scan with per-node
//! materialization per predicate row; pole: 20–87 s).

use std::time::{Duration, Instant};

use cypherlite::{Database, Value};

mod common;

#[test]
fn labs249_anonymous_endpoint_checked_per_reached_node_not_enumerated() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    // 5,000 :Big nodes, one :U wired to exactly one of them: the predicate
    // must anchor at u and check reached nodes, never enumerate :Big
    // (pre-fix that scan cost a full label pass with per-node
    // materialization per row — the pole 20–87 s regression, LABS-249).
    conn.query("UNWIND range(0,4999) AS i CREATE (:Big {i: i})")
        .unwrap();
    conn.query("CREATE (:U {name: 'u'})").unwrap();
    conn.query("MATCH (u:U {name:'u'}), (b:Big {i: 7}) CREATE (u)-[:FOLLOWS]->(b)")
        .unwrap();

    let hit = "MATCH (u:U {name:'u'}) RETURN EXISTS { MATCH (u)-[:FOLLOWS]->(:Big) }";
    let miss = "MATCH (u:U {name:'u'}) RETURN EXISTS { MATCH (u)-[:BLOCKS]->(:Big) }";
    let pinned = "MATCH (u:U {name:'u'}) RETURN EXISTS { MATCH (u)-[:FOLLOWS]->(:Big {i: 7}) }";
    let pinned_miss =
        "MATCH (u:U {name:'u'}) RETURN EXISTS { MATCH (u)-[:FOLLOWS]->(:Big {i: 8}) }";
    assert_eq!(common::scalar_one(&mut conn, hit), Value::Bool(true));
    assert_eq!(common::scalar_one(&mut conn, miss), Value::Bool(false));
    assert_eq!(common::scalar_one(&mut conn, pinned), Value::Bool(true));
    assert_eq!(
        common::scalar_one(&mut conn, pinned_miss),
        Value::Bool(false)
    );

    // Timebound: anchored at u the predicate costs u's single edge
    // (~µs); the pre-fix per-row :Big enumeration (5,000 nodes with
    // materialization) far exceeds 500 ms at this scale in any build.
    let started = Instant::now();
    let r = common::scalar_one(&mut conn, hit);
    let elapsed = started.elapsed();
    assert_eq!(r, Value::Bool(true));
    assert!(
        elapsed < Duration::from_millis(500),
        "LABS-249 repro took {elapsed:?}; the anonymous-endpoint enumeration is back"
    );
}

#[test]
fn labs249_incoming_anchor_null_binding_and_flipped_props() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    conn.query("CREATE (:Me), (:U {name:'a'}), (:U {name:'b'})")
        .unwrap();
    conn.query("MATCH (u:U {name:'a'}), (m:Me) CREATE (u)-[:FOLLOWS]->(m)")
        .unwrap();
    conn.query("MATCH (u:U {name:'b'}), (m:Me) CREATE (m)-[:LEADS]->(u)")
        .unwrap();

    // INCOMING anchored at nodes[0]: the bound endpoint's in-adjacency
    // with the free endpoint checked per reached node.
    let hit = "MATCH (m:Me) WITH m WHERE (m)<-[:FOLLOWS]-(:U) RETURN count(*)";
    assert_eq!(common::scalar_one(&mut conn, hit), Value::Int(1));
    let miss = "MATCH (m:Me) WITH m WHERE (m)<-[:LEADS]-(:U) RETURN count(*)";
    assert_eq!(common::scalar_one(&mut conn, miss), Value::Int(0));

    // A variable bound to null matches nothing: the predicate is false,
    // its negation true, and a positive pattern carrying it stops
    // expanding (zero rows) — Neo4j semantics (previously both walks
    // rebound the null to the reached node).
    let null_bound = "OPTIONAL MATCH (x:Missing) WITH x \
                      MATCH (m:Me) WHERE (m)<-[:FOLLOWS]-(x) RETURN count(*)";
    assert_eq!(common::scalar_one(&mut conn, null_bound), Value::Int(0));
    let null_not = "OPTIONAL MATCH (x:Missing) WITH x \
                    MATCH (m:Me) WHERE NOT (m)<-[:FOLLOWS]-(x) RETURN count(*)";
    assert_eq!(common::scalar_one(&mut conn, null_not), Value::Int(1));
    let null_positive = "OPTIONAL MATCH (x:Missing) WITH x \
                         MATCH (x)-[:FOLLOWS]->(:U) RETURN count(*)";
    assert_eq!(common::scalar_one(&mut conn, null_positive), Value::Int(0));

    // Flipped shape with rel properties: the guard falls back, the
    // walked path's own check order answers.
    conn.query("MATCH (u:U {name:'a'}), (m:Me) CREATE (u)-[:TRUSTS {v: 1}]->(m)")
        .unwrap();
    let flipped_rel_props = "MATCH (m:Me) WITH m WHERE (:U)-[:TRUSTS {v: 1}]->(m) RETURN count(*)";
    assert_eq!(
        common::scalar_one(&mut conn, flipped_rel_props),
        Value::Int(1)
    );
    let flipped_rel_props_miss =
        "MATCH (m:Me) WITH m WHERE (:U)-[:TRUSTS {v: 2}]->(m) RETURN count(*)";
    assert_eq!(
        common::scalar_one(&mut conn, flipped_rel_props_miss),
        Value::Int(0)
    );
}
