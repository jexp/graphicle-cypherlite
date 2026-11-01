//! Expression-form and bidirectional shortest-path searches: the miss cap,
//! the shallow hit, LABS-196's stop after a meet, and ADR-0189's pinned
//! counterexamples.

use cypherlite_core::Value;

use crate::graph::Graph;
use crate::shortest::test_support::{counting_expands, min_hops_counterexample, TernaryTree};

use super::test_support::{exec_cypher, Builder};

#[test]
fn shortest_path_miss_does_not_walk_two_max_hop_balls() {
    let g = Builder::new();
    let a = g.node("a", &["N"]);
    let b = g.node("b", &["N"]);
    for _ in 0..12 {
        let mid = g.anon(&["L"]);
        g.edge(a, mid);
        for _ in 0..12 {
            g.edge(mid, g.anon(&["L"]));
        }
    }
    for _ in 0..12 {
        let mid = g.anon(&["R"]);
        g.edge(mid, b);
        for _ in 0..12 {
            g.edge(g.anon(&["R"]), mid);
        }
    }
    let g = g.done();
    let q = "MATCH (a:N {id: 'a'}), (b:N {id: 'b'}) \
             RETURN shortestPath((a)-[:E*1..3]->(b)) AS p";
    let (rows, walks) = counting_expands(|| exec_cypher(&g, q).rows);
    assert_eq!(rows[0][0], Value::Null);
    // Uncapped bidir walks both 12+144 balls (~312). The miss cap
    // expands one 2-hop side plus the other origin ring (~168).
    assert!(walks < 250, "miss walked two 3-hop balls: walks={walks}");
    assert!(
        walks > 100,
        "expected to expand at least one 2-hop ring: walks={walks}"
    );
}

#[test]
fn shallow_three_hop_hit_does_not_walk_full_second_layer() {
    let g = Builder::new();
    let start = g.node("s", &["N"]);
    let end = g.node("e", &["N"]);
    let m2 = g.anon(&["M"]);
    g.edge(m2, end);
    for _ in 0..40 {
        let mid = g.anon(&["M"]);
        g.edge(start, mid);
        for _ in 0..15 {
            g.edge(mid, g.anon(&["D"]));
        }
    }
    let m1 = g.anon(&["M"]);
    g.chain(&[start, m1, m2]);
    let g = g.done();
    let q = "MATCH (a:N {id: 's'}), (b:N {id: 'e'}) \
             RETURN length(shortestPath((a)-[:E*1..3]->(b))) AS hops";
    let (rows, walks) = counting_expands(|| exec_cypher(&g, q).rows);
    assert_eq!(rows[0][0], Value::Int(3));
    // 1-hop sets are ~41 + 1. First-meet walks the newest start-neigh
    // (m1) only, not 40×15 decoy edges.
    assert!(walks < 120, "finished a 2-hop layer: walks={walks}");
}

/// LABS-196 shape: `s` has 500 outgoing edges into a 1,000-node cycle with
/// chords, plus the 3-hop path s→a→b→t. `t` heads a fan-in tree six
/// levels deep, so the backward ball keeps growing while it stays smaller
/// than the forward depth-1 queue.
fn labs196_graph() -> Graph {
    let g = Builder::new();
    let (s, t) = (g.node("s", &["N"]), g.node("t", &["N"]));
    let ring: Vec<_> = (0..1000).map(|_| g.anon(&["M"])).collect();
    for (i, &n) in ring.iter().enumerate() {
        g.edge(n, ring[(i + 1) % ring.len()]);
        g.edge(n, ring[(i * 31 + 7) % ring.len()]);
    }
    for &n in &ring[..500] {
        g.edge(s, n);
    }
    let (a, b) = (g.anon(&["M"]), g.anon(&["M"]));
    g.chain(&[s, a, b, t]);
    g.fan_in_tree(t, 6);
    g.done()
}

/// For a bound-endpoint `shortestPath` query with `{pattern}` in it, the
/// length it returns and the shortest-path expansions it took at `*1..4`
/// and `*1..8`.
fn labs196_depths(g: &Graph, query: &str, pattern: &str) -> [(Value, u64); 2] {
    [4, 8].map(|max| {
        let q = query.replace("{pattern}", &pattern.replace("{max}", &max.to_string()));
        let (rows, expands) = counting_expands(|| exec_cypher(g, &q).rows);
        (rows[0][0].clone(), expands)
    })
}

/// LABS-196: once a 3-hop meet is found, raising `max_hops` from 4 to 8
/// must not keep the bidirectional search walking (at most 10% more work).
fn assert_labs196_stops_after_meet(query: &str, pattern: &str) {
    let g = labs196_graph();
    let [(len4, walks4), (len8, walks8)] = labs196_depths(&g, query, pattern);
    assert_eq!(len4, Value::Int(3), "{pattern} *1..4 length");
    assert_eq!(len8, Value::Int(3), "{pattern} *1..8 length");
    assert!(walks4 > 0, "{query} with {pattern} did not run the BFS");
    assert!(
        walks8 * 10 <= walks4 * 11,
        "{query} with {pattern}: *1..8 walked {walks8}, *1..4 walked {walks4}"
    );
}

const LABS196_EXPRESSION: &str = "MATCH (s:N {id: 's'}), (t:N {id: 't'}) \
     RETURN length(shortestPath({pattern})) AS hops";
const LABS196_MATCH: &str = "MATCH (s:N {id: 's'}), (t:N {id: 't'}) \
     MATCH p = shortestPath({pattern}) RETURN length(p) AS hops";

#[test]
fn labs196_expression_shortest_path_stops_after_meet() {
    assert_labs196_stops_after_meet(LABS196_EXPRESSION, "(s)-[*1..{max}]->(t)");
}

#[test]
fn labs196_match_shortest_path_stops_after_meet() {
    assert_labs196_stops_after_meet(LABS196_MATCH, "(s)-[*1..{max}]->(t)");
}

#[test]
fn labs196_undirected_shortest_path_stops_after_meet() {
    for query in [LABS196_EXPRESSION, LABS196_MATCH] {
        assert_labs196_stops_after_meet(query, "(s)-[*1..{max}]-(t)");
    }
}

/// The ADR-0189 counterexample. With min_hops > 1 the expression form runs
/// on enumeration, which finds the only 4-hop trail s→p→x→y→t. A
/// bidirectional search that stops at df + db + 1 ≥ L returns 5: it meets
/// on s→q1→q2→q3→q4→t while the front (s plus 50 dead ends) is still at
/// depth 1, since s→p→x→t splits at 3 hops, below min_hops.
#[test]
fn bidir_min_hops_above_one_keeps_searching_after_meet() {
    let g = min_hops_counterexample();
    let bound = "MATCH (s:N {id: 's'}), (t:N {id: 't'}) ";
    let every = exec_cypher(
        &g,
        &format!("{bound}MATCH p = (s)-[*4..8]->(t) RETURN min(length(p)) AS hops"),
    );
    assert_eq!(every.rows[0][0], Value::Int(4));
    let shortest = exec_cypher(
        &g,
        &format!("{bound}RETURN length(shortestPath((s)-[*4..8]->(t))) AS hops"),
    );
    assert_eq!(shortest.rows[0][0], Value::Int(4));
}

/// Closed shortest paths (start = end) run on the closed-trail search
/// (ADR-0189): one search back to the start per first relationship, never
/// over that relationship, so an undirected edge walked back and forth is
/// no trail. Pins the lengths returned before LABS-196.
#[test]
fn bidir_closed_shortest_path_keeps_per_side_stop() {
    let closed = |edges: &[(usize, usize)], pattern: &str| {
        let g = Builder::new();
        let nodes = [g.node("a", &["N"]), g.anon(&["M"]), g.anon(&["M"])];
        for &(from, to) in edges {
            g.edge(nodes[from], nodes[to]);
        }
        let g = g.done();
        let q = format!("MATCH (s:N {{id: 'a'}}) RETURN length(shortestPath({pattern})) AS hops");
        exec_cypher(&g, &q).rows[0][0].clone()
    };
    let triangle = [(0, 1), (1, 2), (2, 0)];
    assert_eq!(closed(&triangle, "(s)-[*1..8]-(s)"), Value::Int(3));
    assert_eq!(closed(&[(0, 1), (1, 0)], "(s)-[*1..8]->(s)"), Value::Int(2));
    assert_eq!(closed(&[(0, 1)], "(s)-[*1..8]-(s)"), Value::Null);
}
