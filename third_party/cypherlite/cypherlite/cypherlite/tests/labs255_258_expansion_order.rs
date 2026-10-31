//! LABS-255 / LABS-258 (ADR-0203): expansion-order parity with the Neo4j
//! reference.
//!
//! Neo4j inserts a new relationship at the head of a node's relationship
//! chain, so a node's expansion walks its relationships newest-first
//! (descending relationship id within a type). That walk order decides two
//! otherwise-unspecified outcomes:
//!
//! - `shortestPath` picks the equal-hop path discovered first by the BFS —
//!   the one reached through the newest relationships (LABS-255, the
//!   openstreetmap walkthrough: the reference's clock→Zoo School walk sums
//!   to 212.19, which is the path through the *newer* of two parallel
//!   ROUTE relationships, not the older one cypherlite used to pick).
//! - Aggregating projections iterate groups in first-encountered (row)
//!   order and `ORDER BY` is a stable sort, so rows tied on the sort keys
//!   keep the order the MATCH rows arrived in — which the expansion order
//!   drives (LABS-258, the legis-graph / twitter-trolls tied `count`s).

mod common;

use cypherlite::Value;

use common::{connection, list, rows, run, string};

/// Two parallel relationships `a→m` (`old` created first, `new` second)
/// and one `m→z`: the only shortest `a`→`z` trail takes one of the
/// parallel pair, and the reference takes the NEWER one.
#[test]
fn labs255_shortest_path_takes_the_newest_parallel_relationship() {
    let mut c = connection();
    run(
        &mut c,
        "CREATE (a:A {name: 'a'})-[:R {tag: 'old'}]->(m:M), \
                (a)-[:R {tag: 'new'}]->(m), \
                (m)-[:R {tag: 'mz'}]->(z:Z)",
    );
    for pattern in [
        "(a:A)-[:R*]-(z:Z)",      // undirected, bidirectional BFS
        "(a:A)-[:R*]->(z:Z)",     // directed, bidirectional BFS
        "(a:A)-[:R*1..3]->(z:Z)", // directed, shallow intersection
    ] {
        let query = format!(
            "MATCH p = shortestPath({pattern}) RETURN [r IN relationships(p) | r.tag] AS tags"
        );
        assert_eq!(
            common::scalar_one(&mut c, &query),
            list(vec![string("new"), string("mz")]),
            "{pattern}"
        );
    }
}

/// Two equal-hop paths `a→m1→z` and `a→m2→z` where every relationship of
/// the `m2` path was created after the `m1` path's: the BFS reaches `m2`
/// first (newest-first expansion), so the path through `m2` wins.
#[test]
fn labs255_shortest_path_picks_the_path_reached_via_the_newest_relationship() {
    let mut c = connection();
    run(
        &mut c,
        "CREATE (a:A), (m1:M {name: 'm1'}), (m2:M {name: 'm2'}), (z:Z)",
    );
    run(
        &mut c,
        "MATCH (a:A), (m1:M {name: 'm1'}) CREATE (a)-[:R]->(m1)",
    );
    run(
        &mut c,
        "MATCH (m1:M {name: 'm1'}), (z:Z) CREATE (m1)-[:R]->(z)",
    );
    run(
        &mut c,
        "MATCH (a:A), (m2:M {name: 'm2'}) CREATE (a)-[:R]->(m2)",
    );
    run(
        &mut c,
        "MATCH (m2:M {name: 'm2'}), (z:Z) CREATE (m2)-[:R]->(z)",
    );
    let got = common::scalar_one(
        &mut c,
        "MATCH p = shortestPath((a:A)-[:R*]-(z:Z)) \
         RETURN [n IN nodes(p) | coalesce(n.name, '?')] AS names",
    );
    assert_eq!(got, list(vec![string("?"), string("m2"), string("?")]),);
}

/// A plain expansion walks the anchor's relationships newest-first; with
/// no `ORDER BY` the row order is that walk order.
#[test]
fn labs255_expansion_walks_newest_relationship_first() {
    let mut c = connection();
    run(&mut c, "CREATE (:S {name: 's'})");
    run(
        &mut c,
        "MATCH (s:S) CREATE (s)-[:E]->(:X {name: 'first-created'})",
    );
    run(
        &mut c,
        "MATCH (s:S) CREATE (s)-[:E]->(:X {name: 'second-created'})",
    );
    let got = rows(&mut c, "MATCH (:S)-[:E]->(x:X) RETURN x.name");
    assert_eq!(
        got,
        vec![
            vec![string("second-created")],
            vec![string("first-created")],
        ]
    );
}

/// Tied `count(*)` groups: groups iterate in first-encountered order and
/// `ORDER BY num DESC` is stable, so the tie row order is the expansion
/// order — the group reached through the newest relationship comes first
/// (the legis-graph:69 / twitter-trolls:226 shape). `collect` inside a
/// group keeps the same arrival order.
#[test]
fn labs258_aggregation_ties_follow_first_encountered_group_order() {
    let mut c = connection();
    run(&mut c, "CREATE (:S {name: 's'})");
    // Older relationship first; both groups tie at count 1.
    run(
        &mut c,
        "MATCH (s:S) CREATE (s)-[:E]->(:X {g: 'older', n: 'old-row'})",
    );
    run(
        &mut c,
        "MATCH (s:S) CREATE (s)-[:E]->(:X {g: 'newer', n: 'new-row'})",
    );
    let got = rows(
        &mut c,
        "MATCH (:S)-[:E]->(x:X) \
         RETURN x.g AS g, collect(x.n) AS ns, count(*) AS num ORDER BY num DESC",
    );
    assert_eq!(
        got,
        vec![
            vec![
                string("newer"),
                list(vec![string("new-row")]),
                Value::Int(1)
            ],
            vec![
                string("older"),
                list(vec![string("old-row")]),
                Value::Int(1)
            ],
        ]
    );

    // Same tie through a WITH … ORDER BY (the twitter-trolls shape): rows
    // entering the second aggregation are stably sorted, ties keep the
    // first aggregation's encounter order, and COLLECT preserves it.
    let got = rows(
        &mut c,
        "MATCH (:S)-[:E]->(x:X) \
         WITH x.g AS g, count(*) AS num ORDER BY num DESC \
         RETURN collect(g) AS gs",
    );
    assert_eq!(
        got,
        vec![vec![list(vec![string("newer"), string("older")])]]
    );
}
