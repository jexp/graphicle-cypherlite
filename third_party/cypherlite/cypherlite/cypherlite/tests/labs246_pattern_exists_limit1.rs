//! LABS-246 regression: a pattern predicate whose shape the anchored
//! probe rejects (multi-hop, var-length, no row-bound endpoint) must
//! still answer existence with a limit-1 walk — stop at the first full
//! match — not collect every matching row and test non-emptiness.
//! (Bound-endpoint 1-hop shapes are the anchored probe, tested in
//! labs193_pattern_exists_probe.rs / labs249_pattern_probe_anonymous_endpoint.rs.)

use std::time::{Duration, Instant};

use cypherlite::{Database, Value};

mod common;

#[test]
fn labs246_ineligible_shapes_answer_existence_correctly() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    conn.query(
        "CREATE (:A {name: 'a'})-[:R]->(:M)-[:R]->(:B {name: 'b'}), \
         (:A {name: 'lone'}), (:B {name: 'unreachable'})",
    )
    .unwrap();

    let q = |conn: &mut cypherlite::Connection, stmt: &str| common::scalar_one(conn, stmt);

    // two hops between row-bound endpoints — the probe only plans one rel
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:A {name:'a'}), (b:B {name:'b'}) \
             WITH a, b WHERE (a)-[:R]->()-[:R]->(b) RETURN count(*)"
        ),
        Value::Int(1)
    );
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:A {name:'a'}), (b:B {name:'unreachable'}) \
             WITH a, b WHERE (a)-[:R]->()-[:R]->(b) RETURN count(*)"
        ),
        Value::Int(0)
    );
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:A {name:'a'}), (b:B {name:'unreachable'}) \
             WITH a, b WHERE NOT (a)-[:R]->()-[:R]->(b) RETURN count(*)"
        ),
        Value::Int(1)
    );
    // var-length: also probe-ineligible
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:A {name:'a'}), (b:B {name:'b'}) \
             WITH a, b WHERE (a)-[:R*1..2]->(b) RETURN count(*)"
        ),
        Value::Int(1)
    );
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:A {name:'a'}), (b:B {name:'b'}) \
             WITH a, b WHERE (a)-[:R*3..4]->(b) RETURN count(*)"
        ),
        Value::Int(0)
    );
    // both endpoints anonymous (no row-bound endpoint at all)
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:A {name:'lone'}) WHERE (:A)-[:R]->()-[:R]->(:B) RETURN count(*)"
        ),
        Value::Int(1)
    );
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:A {name:'lone'}) WHERE (:A)-[:R]->()-[:R]->(:A) RETURN count(*)"
        ),
        Value::Int(0)
    );
    // EXISTS { } spelling of a probe-ineligible body
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:A {name:'a'}), (b:B {name:'b'}) \
             RETURN EXISTS { (a)-[:R]->()-[:R]->(b) }"
        ),
        Value::Bool(true)
    );
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:A {name:'a'}), (b:B {name:'unreachable'}) \
             RETURN EXISTS { (a)-[:R]->()-[:R]->(b) }"
        ),
        Value::Bool(false)
    );
}

#[test]
fn labs246_limit_does_not_leak_into_row_collecting_consumers() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    conn.query("CREATE (:H {name: 'hub'})").unwrap();
    conn.query(
        "UNWIND range(0,9) AS i MATCH (h:H) \
         CREATE (:U {name: 'u'+i})-[:FOLLOWS]->(h)",
    )
    .unwrap();

    // Pattern comprehensions collect every match (no limit): the limit-1
    // short-circuit belongs to boolean existence only.
    assert_eq!(
        common::scalar_one(
            &mut conn,
            "MATCH (h:H) RETURN size([(u)-[:FOLLOWS]->(h) | u])"
        ),
        Value::Int(10)
    );
}

#[test]
fn labs246_hub_existence_does_not_collect_all_matches() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    // 20,000 users all following one hub: the two-hop predicate
    // (u)-[:FOLLOWS]->(:Me)<-[:FOLLOWS]-(:U) has 20,000 matches per outer
    // row; existence needs only the first.
    conn.query("CREATE (:Me)").unwrap();
    conn.query(
        "UNWIND range(0,19999) AS i MATCH (m:Me) WHERE id(m)=0 \
         CREATE (:U {name: 'u'+i})-[:FOLLOWS]->(m)",
    )
    .unwrap();

    // Timebound: the limit-1 walk probes ~2 edges per row (20,000 rows,
    // well under a second); collecting all 20,000 matches per row is
    // 4×10^8 materialized rows — minutes. 20 s holds the fixed path with
    // headroom on loaded CI runners.
    let query = "MATCH (u:U) \
                 WHERE (u)-[:FOLLOWS]->(:Me)<-[:FOLLOWS]-(:U) \
                 RETURN count(*)";
    let started = Instant::now();
    let count = common::scalar_one(&mut conn, query);
    let elapsed = started.elapsed();
    assert_eq!(count, Value::Int(20000));
    assert!(
        elapsed < Duration::from_secs(20),
        "LABS-246 repro took {elapsed:?}; the collect-all fallback is back"
    );

    // The same shape with no match must walk and answer false, fast.
    let started = Instant::now();
    let count = common::scalar_one(
        &mut conn,
        "MATCH (u:U) WHERE (u)-[:FOLLOWS]->(:Me)<-[:BLOCKS]-(:U) RETURN count(*)",
    );
    let elapsed = started.elapsed();
    assert_eq!(count, Value::Int(0));
    assert!(
        elapsed < Duration::from_secs(20),
        "LABS-246 negative probe took {elapsed:?}"
    );
}
