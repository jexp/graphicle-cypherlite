//! LABS-193 regression: a pattern-predicate existence check with at
//! least one row-bound endpoint must anchor on the bound side (the
//! smaller side when both are bound) and probe its adjacency with an
//! early exit — not walk the first-written endpoint's whole adjacency
//! materializing a row per edge. (Free-endpoint checking is LABS-249,
//! tested in its own file.)

use std::time::{Duration, Instant};

use cypherlite::{Database, Value};

mod common;

#[test]
fn labs193_probe_results_match_the_walked_form() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    conn.query(
        "CREATE (:Me {name: 'me'}), \
         (:U {name: 'a'}), (:U {name: 'b'}), (:U {name: 'c'}), \
         (:X {name: 'x'})",
    )
    .unwrap();
    conn.query("MATCH (a:U {name:'a'}), (m:Me) CREATE (a)-[:FOLLOWS {since: 2020}]->(m)")
        .unwrap();
    conn.query("MATCH (b:U {name:'b'}), (m:Me) CREATE (b)-[:FOLLOWS {since: 2021}]->(m)")
        .unwrap();
    conn.query("MATCH (c:U {name:'c'}), (x:X) CREATE (c)-[:BLOCKS]->(x)")
        .unwrap();

    let q = |conn: &mut cypherlite::Connection, stmt: &str| common::scalar_one(conn, stmt);

    // bound-bound, both spellings, negated
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}), (m:Me) WITH a, m WHERE (a)-[:FOLLOWS]->(m) RETURN count(*)"
        ),
        Value::Int(1)
    );
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}), (m:Me) WITH a, m WHERE (m)<-[:FOLLOWS]-(a) RETURN count(*)"
        ),
        Value::Int(1)
    );
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}), (m:Me) WITH a, m WHERE (a)-[:BLOCKS]->(m) RETURN count(*)"
        ),
        Value::Int(0)
    );
    assert_eq!(q(&mut conn, "MATCH (a:U {name:'a'}), (m:Me) WITH a, m WHERE NOT (a)-[:FOLLOWS]->(m) RETURN count(*)"), Value::Int(0));

    // rel types and properties
    assert_eq!(q(&mut conn, "MATCH (a:U {name:'a'}), (m:Me) WITH a, m WHERE (a)-[:FOLLOWS {since: 2020}]->(m) RETURN count(*)"), Value::Int(1));
    assert_eq!(q(&mut conn, "MATCH (a:U {name:'a'}), (m:Me) WITH a, m WHERE (a)-[:FOLLOWS {since: 2021}]->(m) RETURN count(*)"), Value::Int(0));
    assert_eq!(q(&mut conn, "MATCH (a:U {name:'a'}), (m:Me) WITH a, m WHERE (a)-[:FOLLOWS|BLOCKS]->(m) RETURN count(*)"), Value::Int(1));

    // anonymous endpoint pinned by its label
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}) WHERE (a)-[:FOLLOWS]->(:Me) RETURN count(*)"
        ),
        Value::Int(1)
    );
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}) WHERE (:Me)<-[:FOLLOWS]-(a) RETURN count(*)"
        ),
        Value::Int(1)
    );
    // anonymous multi-candidate endpoint: checked per reached node from
    // the bound side (LABS-249), never enumerated
    assert_eq!(
        q(
            &mut conn,
            "MATCH (m:Me) WHERE (m)<-[:FOLLOWS]-(:U) RETURN count(*)"
        ),
        Value::Int(1)
    );

    // node constraints on resolved endpoints
    assert_eq!(q(&mut conn, "MATCH (a:U {name:'a'}), (m:Me) WITH a, m WHERE (a:U {name:'a'})-[:FOLLOWS]->(m) RETURN count(*)"), Value::Int(1));
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}), (m:Me) WITH a, m WHERE (a:X)-[:FOLLOWS]->(m) RETURN count(*)"
        ),
        Value::Int(0)
    );

    // undirected spelling — the only one exercising the Both side-choice
    // and hop() with Direction::Both
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}), (m:Me) WITH a, m WHERE (a)-[:FOLLOWS]-(m) RETURN count(*)"
        ),
        Value::Int(1)
    );
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}), (m:Me) WITH a, m WHERE (a)-[:BLOCKS]-(m) RETURN count(*)"
        ),
        Value::Int(0)
    );
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}), (m:Me) WITH a, m RETURN EXISTS { (a)-[:FOLLOWS]-(m) }"
        ),
        Value::Bool(true)
    );

    // both endpoints anonymous but each pinned by its label
    assert_eq!(
        q(
            &mut conn,
            "MATCH (ou:U {name:'a'}) WHERE (:Me)<-[:FOLLOWS]-(:U) RETURN count(*)"
        ),
        Value::Int(1)
    );

    // the same variable on both ends
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}) WHERE (a)-[:FOLLOWS]->(a) RETURN count(*)"
        ),
        Value::Int(0)
    );

    // EXISTS { } form of the same shapes
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}), (m:Me) WITH a, m RETURN EXISTS { (a)-[:FOLLOWS]->(m) }"
        ),
        Value::Bool(true)
    );
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}), (m:Me) WITH a, m RETURN EXISTS { (a)-[:BLOCKS]->(m) }"
        ),
        Value::Bool(false)
    );
    assert_eq!(
        q(
            &mut conn,
            "MATCH (a:U {name:'a'}) RETURN EXISTS { (a)-[:FOLLOWS]->(:Me) }"
        ),
        Value::Bool(true)
    );
}

#[test]
fn labs193_destination_checks_run_with_the_rel_bound() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    // r.v = 1, m.v = 2 on the first edge; equal on the second.
    conn.query("CREATE (:A {name:'a'})-[:R {v: 1}]->(:M {v: 2})")
        .unwrap();
    conn.query("CREATE (:B {name:'b'})-[:R2 {v: 5}]->(:N {v: 5})")
        .unwrap();

    // A destination property referencing the rel variable evaluates with
    // the rel bound (bind_trail runs before the destination's node checks
    // in the walked path) — pre-fix the probe dropped it as null and
    // answered true.
    let mismatch = "MATCH (a:A {name:'a'}) \
                    RETURN EXISTS { MATCH (a)-[r:R]->(m:M {v: r.v}) }";
    assert_eq!(
        common::scalar_one(&mut conn, mismatch),
        Value::Bool(false),
        "r.v=1 != m.v=2: the destination constraint must reject the edge"
    );
    let matched = "MATCH (b:B {name:'b'}) \
                    RETURN EXISTS { MATCH (b)-[r:R2]->(n:N {v: r.v}) }";
    assert_eq!(common::scalar_one(&mut conn, matched), Value::Bool(true));
}

#[test]
fn labs193_hub_side_predicate_is_not_quadratic() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    // 20,000 users all following one hub: a predicate written from the
    // hub's side must not walk the hub's 20,000-edge adjacency per row.
    conn.query("CREATE (:Me)").unwrap();
    conn.query(
        "UNWIND range(0,19999) AS i MATCH (m:Me) WHERE id(m)=0 \
         CREATE (:U {name: 'u'+i})-[:FOLLOWS]->(m)",
    )
    .unwrap();

    // Timebound: the fixed query probes ou's single edge per row (~0.2 s
    // debug); pre-fix it row-walked the hub's 20,000 in-adjacency per row
    // (~4.5 min release, far beyond any bound). 20 s holds the fixed path
    // with headroom on loaded CI runners.
    let query = "MATCH (ou:U) \
                 WHERE (ou)-[:FOLLOWS]->(:Me) AND NOT (:Me)<-[:FOLLOWS]-(ou) \
                 RETURN count(*)";
    let started = Instant::now();
    let count = common::scalar_one(&mut conn, query);
    let elapsed = started.elapsed();
    assert_eq!(count, Value::Int(0));
    assert!(
        elapsed < Duration::from_secs(20),
        "LABS-193 repro took {elapsed:?}; the per-row adjacency walk is back"
    );
}
