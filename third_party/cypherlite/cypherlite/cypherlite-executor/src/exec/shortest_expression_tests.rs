//! The expression form on the shared searches (ADR-0189): closed trails,
//! `min_hops` > 1 on enumeration, the unbound-end stop and its merge with
//! closed trails, a bound end that fails its pattern, and how start
//! candidates combine.

use std::collections::HashMap;
use std::sync::Arc;

use cypherlite_core::Value;

use crate::graph::Graph;
use crate::shortest::test_support::{counting_expands, min_hops_counterexample, TernaryTree};

use super::test_support::{exec_cypher, ints, Builder};

/// The single value of a one-row, one-column result, and the expansions
/// it took.
fn value(g: &Graph, q: &str) -> (Value, u64) {
    let (rows, expands) = counting_expands(|| exec_cypher(g, q).rows);
    assert_eq!(rows.len(), 1, "{q}");
    (rows[0][0].clone(), expands)
}

fn names(paths: &[&[&str]]) -> Value {
    let path = |ids: &&[&str]| {
        Value::List(
            ids.iter()
                .map(|&id| Value::String(id.into()))
                .collect::<Vec<_>>()
                .into(),
        )
    };
    Value::List(paths.iter().map(path).collect::<Vec<_>>().into())
}

/// `routes`, a list, in a fixed order.
fn sorted(routes: Value) -> Value {
    let Value::List(mut routes) = routes else {
        panic!("expected a list, got {routes:?}");
    };
    Arc::make_mut(&mut routes).sort_by(Value::order_by_cmp);
    Value::List(routes)
}

#[test]
fn all_shortest_closed_trails_on_an_undirected_triangle() {
    let b = Builder::new();
    let a = b.node("a", &["N"]);
    let (x, y) = (b.node("x", &[]), b.node("y", &[]));
    b.chain(&[a, x, y, a]);
    let g = b.done();
    let (lengths, expands) = value(
        &g,
        "MATCH (s:N {id: 'a'}) \
         RETURN [p IN allShortestPaths((s)-[*1..8]-(s)) | length(p)] AS ls",
    );
    assert_eq!(lengths, ints(&[3, 3]));
    assert!(expands > 0, "the closed-trail search did not run");
    let (routes, _) = value(
        &g,
        "MATCH (s:N {id: 'a'}) \
         RETURN [p IN allShortestPaths((s)-[*1..8]-(s)) | [n IN nodes(p) | n.id]] AS ps",
    );
    assert_eq!(
        sorted(routes),
        names(&[&["a", "x", "y", "a"], &["a", "y", "x", "a"]])
    );
}

#[test]
fn min_hops_above_one_runs_on_enumeration() {
    // s→p→x→y→t is the only 4-hop trail; s→p→x→t (3 hops) is below the
    // minimum and s→q1→…→t takes 5. 50 dead ends hang off s and a fan-in
    // tree under t, the counterexample of
    // `bidir_min_hops_above_one_keeps_searching_after_meet`.
    let g = min_hops_counterexample();
    let bound = "MATCH (s:N {id: 's'}), (t:N {id: 't'}) ";
    let (hops, expands) = value(
        &g,
        &format!("{bound}RETURN length(shortestPath((s)-[*4..8]->(t))) AS hops"),
    );
    assert_eq!(hops, Value::Int(4));
    assert_eq!(expands, 0, "min_hops > 1 ran a BFS");
    let (routes, _) = value(
        &g,
        &format!(
            "{bound}RETURN [p IN allShortestPaths((s)-[*4..8]->(t)) | \
             [n IN nodes(p) | n.id]] AS ps"
        ),
    );
    assert_eq!(routes, names(&[&["s", "p", "x", "y", "t"]]));
}

#[test]
fn unbound_end_stops_after_the_first_matching_layer() {
    // s→t with t:T, and a fan-out tree of 1,092 nodes behind t.
    let b = Builder::new();
    let (s, t) = (b.node("s", &["N"]), b.node("t", &["T"]));
    b.edge(s, t);
    b.fan_out_tree(t, 6);
    let g = b.done();
    let start = "MATCH (s:N {id: 's'}) ";
    for (q, want) in [
        ("RETURN length(shortestPath((s)-[*]->(:T)))", Value::Int(1)),
        (
            "RETURN size(allShortestPaths((s)-[*]->(:T)))",
            Value::Int(1),
        ),
    ] {
        let (got, expands) = value(&g, &format!("{start}{q}"));
        assert_eq!(got, want, "{q}");
        assert!(expands > 0, "{q} did not run the BFS");
        assert!(
            expands < 10,
            "{q} walked the ball behind t: expands={expands}"
        );
    }
    // The same search with no matching end walks the whole ball.
    let (got, expands) = value(
        &g,
        &format!("{start}RETURN shortestPath((s)-[*]->(:Missing))"),
    );
    assert_eq!(got, Value::Null);
    assert!(expands > 1000, "expands={expands}");
}

#[test]
fn start_candidates_combine_in_candidate_order() {
    // Starts s1, s2, s3 in creation (candidate) order: s1 reaches t and t1
    // in 3 hops; s2 reaches t and t2 in 1 hop and again in 3, and t3 in 3;
    // s3 reaches t in 3.
    let b = Builder::new();
    let starts = ["s1", "s2", "s3"].map(|id| b.node(id, &["S"]));
    let t = b.node("t", &["N"]);
    let ends = ["t1", "t2", "t3"].map(|id| b.node(id, &["T"]));
    let mid = |id: &str| b.node(id, &[]);
    b.chain(&[starts[0], mid("x"), mid("y"), t]);
    b.chain(&[starts[1], t]);
    b.chain(&[starts[1], mid("e"), mid("f"), t]);
    b.chain(&[starts[2], mid("u"), mid("v"), t]);
    b.chain(&[starts[0], mid("a"), mid("b"), ends[0]]);
    b.chain(&[starts[1], ends[1]]);
    b.chain(&[starts[1], mid("g"), mid("h"), ends[1]]);
    b.chain(&[starts[1], mid("c"), mid("d"), ends[2]]);
    let g = b.done();
    let ids = "[p IN allShortestPaths({pattern}) | [n IN nodes(p) | n.id]]";
    for (bind, pattern, all) in [
        (
            "MATCH (t:N {id: 't'}) ",
            "(:S)-[*]->(t)",
            names(&[&["s1", "x", "y", "t"], &["s3", "u", "v", "t"]]),
        ),
        (
            "",
            "(:S)-[*]->(:T)",
            names(&[&["s1", "a", "b", "t1"], &["s2", "c", "d", "t3"]]),
        ),
    ] {
        // shortestPath: the first start with any trail, not the shortest
        // over all starts (s2's 1 hop).
        let (hops, _) = value(&g, &format!("{bind}RETURN length(shortestPath({pattern}))"));
        assert_eq!(hops, Value::Int(3), "{pattern}");
        // allShortestPaths: s1 sets the length, 3. Later starts add their
        // shortest trails of exactly that length. s2's 1-hop trails are
        // dropped, and so are its 3-hop trails to the same ends, which are
        // not shortest (s2→e→f→t, s2→g→h→t2; ADR-0189 Consequences).
        let q = format!("{bind}RETURN {}", ids.replace("{pattern}", pattern));
        assert_eq!(value(&g, &q).0, all, "{q}");
    }
}

#[test]
fn bound_end_failing_its_pattern_returns_null() {
    // a→m→b: b is reachable, but neither `:Missing` nor `{id: 'z'}`.
    let b = Builder::new();
    let (a, end) = (b.node("a", &["N"]), b.node("b", &["N"]));
    b.chain(&[a, b.node("m", &[]), end]);
    let g = b.done();
    for (end, want) in [
        ("(b:N)", [Value::Int(2), Value::Int(1)]),
        ("(b {id: 'b'})", [Value::Int(2), Value::Int(1)]),
        ("(b:Missing)", [Value::Null, Value::Null]),
        ("(b {id: 'z'})", [Value::Null, Value::Null]),
    ] {
        let q = format!(
            "MATCH (a:N {{id: 'a'}}), (b:N {{id: 'b'}}) \
             RETURN length(shortestPath((a)-[*]->{end})), \
             size(allShortestPaths((a)-[*]->{end}))"
        );
        let (rows, expands) = counting_expands(|| exec_cypher(&g, &q).rows);
        assert_eq!(rows, vec![want.to_vec()], "{q}");
        if want[0] == Value::Null {
            assert_eq!(expands, 0, "{q} searched for an end that fails its pattern");
        } else {
            assert!(expands > 0, "{q} did not run the BFS");
        }
    }
}

/// A graph of `E` relationships between nodes created on first mention,
/// carrying their name as `id`; `s` and `t` are `:N`.
fn named_graph(edges: &[(&str, &str)]) -> Graph {
    let b = Builder::new();
    let mut ids = HashMap::new();
    for &(from, to) in edges {
        let [from, to] = [from, to].map(|id| {
            *ids.entry(id).or_insert_with(|| {
                let labels: &[&str] = if matches!(id, "s" | "t") { &["N"] } else { &[] };
                b.node(id, labels)
            })
        });
        b.edge(from, to);
    }
    b.done()
}

/// On the graph of `edges`, `allShortestPaths((s){rel}(:N))` from `s`
/// returns `routes` (in any order) and `shortestPath` their length.
fn assert_unbound_end_routes(edges: &[(&str, &str)], rel: &str, routes: &[&[&str]]) {
    let g = named_graph(edges);
    let pattern = format!("(s){rel}(:N)");
    let start = "MATCH (s:N {id: 's'}) ";
    let (got, expands) = value(
        &g,
        &format!(
            "{start}RETURN [p IN allShortestPaths({pattern}) | \
             [n IN nodes(p) | n.id]]"
        ),
    );
    assert_eq!(sorted(got), sorted(names(routes)), "{edges:?} {pattern}");
    assert!(expands > 0, "{edges:?} {pattern} did not run the BFS");
    let (hops, _) = value(
        &g,
        &format!("{start}RETURN length(shortestPath({pattern}))"),
    );
    let want = routes[0].len() as i64 - 1;
    assert_eq!(hops, Value::Int(want), "{edges:?} {pattern}");
}

#[test]
fn start_matching_the_unbound_end_merges_its_closed_trails() {
    // The end pattern (:N) matches the start s, and t where there is one.
    // An undirected triangle with only s :N: the closed trails alone.
    assert_unbound_end_routes(
        &[("s", "x"), ("x", "y"), ("y", "s")],
        "-[*]-",
        &[&["s", "x", "y", "s"], &["s", "y", "x", "s"]],
    );
    // The shorter closed trail wins.
    assert_unbound_end_routes(
        &[("s", "x"), ("x", "s"), ("s", "a"), ("a", "b"), ("b", "t")],
        "-[*]->",
        &[&["s", "x", "s"]],
    );
    // A closed and an open trail of one length merge.
    assert_unbound_end_routes(
        &[("s", "x"), ("x", "s"), ("s", "a"), ("a", "t")],
        "-[*]->",
        &[&["s", "a", "t"], &["s", "x", "s"]],
    );
    // The shorter open trail wins.
    assert_unbound_end_routes(
        &[("s", "x"), ("x", "s"), ("s", "t")],
        "-[*]->",
        &[&["s", "t"]],
    );
}

#[test]
fn relationship_property_map_runs_on_enumeration() {
    // a→b with k: 0, and a→m→b with k: 1.
    let b = Builder::new();
    let (a, end, m) = (b.node("a", &["N"]), b.node("b", &["N"]), b.node("m", &[]));
    for (from, to, k) in [(a, end, 0), (a, m, 1), (m, end, 1)] {
        b.rel(from, to, "E", &[("k", Value::Int(k))]);
    }
    let g = b.done();
    let (hops, expands) = value(
        &g,
        "MATCH (a:N {id: 'a'}), (b:N {id: 'b'}) \
         RETURN length(shortestPath((a)-[*1..3 {k: 1}]->(b)))",
    );
    assert_eq!(hops, Value::Int(2));
    assert_eq!(expands, 0, "the BFS ignores relationship property maps");
}
