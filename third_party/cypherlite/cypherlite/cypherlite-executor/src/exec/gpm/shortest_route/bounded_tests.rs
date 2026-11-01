//! ADR-0186 regression block: `MATCH`-clause shortest paths stay on BFS
//! (LABS-192). A pattern that falls back to GPM enumerate-then-select makes
//! no route run and no BFS expansion, so every test asserts both counters
//! are above zero as well as the expansions under their bound.

use std::collections::HashMap;
use std::time::Duration;

use cypherlite_core::Value::{self, Int};

use crate::exec::test_support::{build, exec_cypher, Builder};
use crate::graph::Graph;

use super::fixtures::run;

/// Diamonds in the chain; 2^12 = 4096 s-to-t trails, which GPM still
/// enumerates fast enough that a routing regression fails on `> 0`, not on
/// a timeout.
const DIAMONDS: usize = 12;
/// Relationships in the diamond chain: four per diamond.
const DIAMOND_RELS: u64 = 4 * DIAMONDS as u64;

/// `s -> {u_i, v_i} -> m_i` for `i` in `1..=12`, each `m_i` starting the
/// next diamond and `m_12` being `t`: 4096 trails of length 24 from `s` to
/// `t` over 48 relationships.
fn diamond_chain() -> Graph {
    let b = Builder::new();
    let mut prev = b.node("s", &[]);
    for i in 1..=DIAMONDS {
        let m = if i == DIAMONDS {
            "t".into()
        } else {
            format!("m{i}")
        };
        let m = b.node(&m, &[]);
        for side in ["u", "v"] {
            let mid = b.node(&format!("{side}{i}"), &[]);
            b.chain(&[prev, mid, m]);
        }
        prev = m;
    }
    b.done()
}

#[test]
fn diamond_chain_is_bounded() {
    let g = diamond_chain();
    for q in [
        "MATCH (s {id: 's'}) MATCH (t {id: 't'}) \
         MATCH p = shortestPath((s)-[*..100]->(t)) RETURN length(p)",
        "MATCH p = ANY SHORTEST (s {id: 's'})-[*]->(x {id: 't'}) RETURN length(p)",
    ] {
        let (rows, counts) = run(&g, q);
        assert_eq!(rows, vec![vec![Int(24)]], "{q}");
        assert!(counts.route_runs > 0, "fell back to GPM: {q}");
        assert!(counts.expands > 0, "fell back to GPM: {q}");
        assert!(
            counts.expands <= 4 * DIAMOND_RELS,
            "expands={} > 4 x {DIAMOND_RELS} relationships: {q}",
            counts.expands
        );
    }
}

/// Attack-path shape: user `u` reaches four of the five `:HighValue`
/// targets over membership, admin and session edges with cross links and
/// cycles. `vault` is unreachable: its only incoming edge is from `x`,
/// which nothing points at. Returns the graph and its relationship count.
fn attack_graph() -> (Graph, u64) {
    let nodes: &[(&[&str], &[&str])] = &[
        (&["User"], &["u", "u2", "u3", "x"]),
        (&["Group"], &["g1", "g2", "g3", "g4"]),
        (&["Computer"], &["c1", "c2", "c3", "c4"]),
        (&["Group", "HighValue"], &["da", "ea"]),
        (&["Computer", "HighValue"], &["dc", "ca", "vault"]),
    ];
    let rels: &[(&str, &str, &str)] = &[
        ("u", "g1", "MemberOf"),
        ("u", "g2", "MemberOf"),
        ("g1", "g3", "MemberOf"),
        ("g2", "c1", "AdminTo"),
        ("g3", "c2", "AdminTo"),
        ("g3", "c3", "GenericAll"),
        ("c1", "u2", "HasSession"),
        ("c2", "u3", "HasSession"),
        ("c3", "u2", "HasSession"),
        ("u2", "c4", "AdminTo"),
        ("c4", "u3", "HasSession"),
        ("u2", "g4", "MemberOf"),
        ("u3", "g4", "MemberOf"),
        ("g4", "c1", "AdminTo"),
        ("g4", "da", "MemberOf"),
        ("da", "dc", "AdminTo"),
        ("da", "ea", "MemberOf"),
        ("ea", "g1", "GenericAll"),
        ("u3", "ca", "Enroll"),
        ("x", "vault", "AdminTo"),
        ("vault", "u2", "HasSession"),
    ];
    let b = Builder::new();
    let mut ids = HashMap::new();
    for &(labels, names) in nodes {
        for &id in names {
            ids.insert(id, b.node(id, labels));
        }
    }
    for &(from, to, t) in rels {
        b.rel(ids[from], ids[to], t, &[]);
    }
    (b.done(), rels.len() as u64)
}

/// Nodes of the dense mesh below.
const MESH: usize = 8;

/// A `:M` mesh with both directions on every pair — `MESH * (MESH - 1)`
/// relationships, each node of degree `MESH - 1` — plus an isolated `:T`
/// target nothing points at. No trail of any length ends at the target, so a
/// path-level predicate sends the route through the whole length range: the
/// trail count multiplies by the mesh degree per hop, orders of magnitude more
/// work than the BFS layer over the same mesh.
fn mesh_and_isolated_target() -> Graph {
    let mut labels: Vec<&[&str]> = vec![&["M"]; MESH];
    labels.push(&["T"]);
    let mut rels = Vec::new();
    for from in 0..MESH {
        for to in 0..MESH {
            if from != to {
                rels.push((from, to, "R"));
            }
        }
    }
    build(&labels, &rels).graph
}

/// The dense-graph predicate shape (ADR-0186): a path-level predicate turns the
/// BFS into a trail enumeration, so the route walks every trail of every length
/// instead of one shortest path. The query still answers within the bound,
/// charging the statement's cancellation window as it walks rather than only
/// between lengths.
#[test]
fn dense_mesh_path_predicate_search_is_bounded() {
    let g = mesh_and_isolated_target();
    let q = "MATCH (a:M {id: 0}) MATCH (t:T) \
             MATCH p = shortestPath((a)-[*1..6]->(t)) \
             WHERE NONE(n IN nodes(p) WHERE n.id = 0) RETURN count(p)";
    let started = std::time::Instant::now();
    let (rows, counts) = run(&g, q);
    let elapsed = started.elapsed();
    assert_eq!(
        rows,
        vec![vec![Int(0)]],
        "the unreachable target must match nothing"
    );
    assert!(
        counts.route_runs > 0,
        "the predicate search fell back to GPM"
    );
    assert!(
        counts.expands > 100_000,
        "expands={} did not exercise the dense enumeration",
        counts.expands
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "the dense predicate search took {elapsed:?} for {} expansions",
        counts.expands
    );
}

#[test]
fn shortest_path_to_high_value_targets_is_bounded() {
    let (g, rel_count) = attack_graph();
    let reach = "MATCH (u:User {id: 'u'}) MATCH (c:HighValue) \
                 MATCH p = shortestPath((u)-[*..100]->(c)) ";
    let (rows, counts) = run(&g, &format!("{reach}RETURN count(p)"));
    assert_eq!(rows, vec![vec![Int(4)]]);
    assert!(
        counts.route_runs > 0,
        "clause shortestPath fell back to GPM"
    );
    assert!(counts.expands > 0, "clause shortestPath fell back to GPM");
    assert!(
        counts.expands <= 5 * 4 * rel_count,
        "expands={} > 5 targets x 4 x {rel_count} relationships",
        counts.expands
    );
    let rows = exec_cypher(&g, &format!("{reach}RETURN c.id, length(p) ORDER BY c.id")).rows;
    let want = [("ca", 5), ("da", 5), ("dc", 6), ("ea", 6)]
        .map(|(id, n)| vec![Value::String(id.into()), Int(n)]);
    assert_eq!(rows, want);
}
