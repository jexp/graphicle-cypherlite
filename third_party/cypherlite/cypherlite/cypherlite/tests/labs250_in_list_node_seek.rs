//! LABS-250 regression: `WHERE v IN [row-bound nodes]` on a pattern's
//! node variable is a candidate producer (the same shape the
//! `id(n) IN [...]` seek uses) — not a full-graph enumeration the IN
//! filter rechecks per candidate. Pre-fix, pole adoc:390's
//! `MATCH (person)-[:PARTY_TO]->(crime) WHERE person IN [p1, p2, p3]`
//! enumerated all 61,521 nodes per triangle row (≈660 ms).

use std::time::{Duration, Instant};

use cypherlite::{Database, Value};

mod common;

#[test]
fn labs250_in_list_of_nodes_is_a_candidate_producer() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    // 5,000 :Big nodes, two :U anchors wired to two of them: the IN list
    // names the candidates, the pattern must not enumerate :Big.
    conn.query("UNWIND range(0,4999) AS i CREATE (:Big {i: i})")
        .unwrap();
    conn.query("CREATE (:U {name: 'u'}), (:U {name: 'v'})")
        .unwrap();
    conn.query("MATCH (u:U {name:'u'}), (b:Big {i: 7}) CREATE (u)-[:FOLLOWS]->(b)")
        .unwrap();
    conn.query("MATCH (u:U {name:'v'}), (b:Big {i: 8}) CREATE (u)-[:FOLLOWS]->(b)")
        .unwrap();

    let two = "MATCH (u:U), (v:U) WHERE (u)-[:FOLLOWS]->(:Big) AND (v)-[:FOLLOWS]->(:Big) WITH u, v MATCH (b)-[:FOLLOWS]->(t:Big) WHERE b IN [u, v] RETURN count(*)";
    let dup =
        "MATCH (u:U {name:'u'}) WITH u MATCH (b)-[:FOLLOWS]->(t:Big) WHERE b IN [u, u] RETURN count(*)";
    let mixed =
        "MATCH (u:U {name:'u'}) WITH u MATCH (b)-[:FOLLOWS]->(t:Big) WHERE b IN [u, 1, 'x'] RETURN count(*)";
    let empty =
        "MATCH (u:U {name:'u'}) WITH u MATCH (b)-[:FOLLOWS]->(t:Big) WHERE b IN [] RETURN count(*)";
    let extra = "MATCH (u:U {name:'u'}), (v:U {name:'v'}) WITH u, v MATCH (b)-[:FOLLOWS]->(t:Big) WHERE b IN [u, v] AND t.i = 8 RETURN count(*)";

    // 4 ordered (u, v) pairs; b iterates the distinct listed anchors
    // (1 edge for (u,u) and (v,v), 2 for the mixed pairs) = 6 rows
    assert_eq!(common::scalar_one(&mut conn, two), Value::Int(6));
    // duplicate list values stay one row (IN is set membership)
    assert_eq!(common::scalar_one(&mut conn, dup), Value::Int(1));
    // non-node elements contribute no candidates (the residual rechecks)
    assert_eq!(common::scalar_one(&mut conn, mixed), Value::Int(1));
    // empty list matches nothing
    assert_eq!(common::scalar_one(&mut conn, empty), Value::Int(0));
    // other conjuncts still apply
    assert_eq!(common::scalar_one(&mut conn, extra), Value::Int(1));

    // Timebound: anchored on the list, the second MATCH costs the listed
    // nodes' adjacency — ~50 ms locally, ~600 ms on a loaded shared CI
    // runner; pre-fix it enumerated 5,000 :Big nodes per anchor row, tens
    // of seconds on the same runner. 5 s separates the two in any build.
    let started = Instant::now();
    let r = common::scalar_one(&mut conn, two);
    let elapsed = started.elapsed();
    assert_eq!(r, Value::Int(6));
    assert!(
        elapsed < Duration::from_secs(5),
        "LABS-250 repro took {elapsed:?}; the per-row node enumeration is back"
    );
}
