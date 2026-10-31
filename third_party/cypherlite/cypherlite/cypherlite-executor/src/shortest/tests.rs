//! The shared searches called directly: trails as relationship ids, the
//! pair search's dispatch, and the followable-pattern check.

use cypherlite_core::RelId;
use cypherlite_front::parse_and_lower;
use cypherlite_ir::{Clause, Direction, PatternElement};

use crate::exec::test_support::{build, Built};
use crate::overlay::ExecGraph;

use super::test_support::counting_expands;
use super::{followable, pair, path_from_trail, Search};

fn search<'a, 'g>(graph: &'a ExecGraph<'g>, dir: Direction, max: u32) -> Search<'a, 'g> {
    Search {
        graph,
        types: &[],
        dir,
        min: 1,
        max,
    }
}

fn sorted(mut trails: Vec<Vec<RelId>>) -> Vec<Vec<RelId>> {
    trails.sort();
    trails
}

#[test]
fn closed_finds_both_triangle_trails_and_self_loops() {
    let Built {
        graph: g,
        nodes: n,
        rels: r,
    } = build(&[&[][..]; 3], &[(0, 1, "R"), (1, 2, "R"), (2, 0, "R")]);
    let view = ExecGraph::read(&g);
    let s = search(&view, Direction::Both, 8);
    assert_eq!(
        sorted(s.closed(n[0], true).unwrap()),
        sorted(vec![vec![r[0], r[1], r[2]], vec![r[2], r[1], r[0]]])
    );
    assert_eq!(s.closed(n[0], false).unwrap().len(), 1);
    assert!(search(&view, Direction::Both, 2)
        .closed(n[0], true)
        .unwrap()
        .is_empty());

    let Built {
        graph: g,
        nodes: n,
        rels: r,
    } = build(&[&[][..]; 2], &[(0, 1, "R"), (0, 0, "R"), (1, 0, "R")]);
    let view = ExecGraph::read(&g);
    let s = search(&view, Direction::Outgoing, 8);
    assert_eq!(s.closed(n[0], true).unwrap(), vec![vec![r[1]]]);
    assert_eq!(s.closed(n[1], true).unwrap(), vec![vec![r[2], r[0]]]);
}

#[test]
fn dag_keeps_every_shortest_predecessor_only_for_all_paths() {
    // 0→1→3, 0→2→3, 3→4.
    let Built {
        graph: g,
        nodes: n,
        rels: r,
    } = build(
        &[&[][..]; 5],
        &[
            (0, 1, "R"),
            (1, 3, "R"),
            (0, 2, "R"),
            (2, 3, "R"),
            (3, 4, "R"),
        ],
    );
    let view = ExecGraph::read(&g);
    let s = search(&view, Direction::Outgoing, 8);
    let all = s.dag(n[0], true, None, |_, _| false).unwrap();
    // Newest-first expansion (ADR-0203): node 0's out-chunk walks r[2]
    // (0→2) before r[0] (0→1), so the BFS order visits 2 before 1.
    assert_eq!(all.order(), &[n[2], n[1], n[3], n[4]]);
    assert_eq!(
        sorted(all.trails(&view, n[4]).unwrap()),
        vec![vec![r[0], r[1], r[4]], vec![r[2], r[3], r[4]]]
    );
    assert_eq!(all.trails(&view, n[0]).unwrap(), vec![Vec::<RelId>::new()]);
    let one = s.dag(n[0], false, None, |_, _| false).unwrap();
    assert_eq!(one.trails(&view, n[4]).unwrap().len(), 1);
    let stopped = s.dag(n[0], true, None, |id, _| id == n[3]).unwrap();
    assert_eq!(stopped.trails(&view, n[3]).unwrap().len(), 2);
    assert!(stopped.trails(&view, n[4]).unwrap().is_empty());
    let shallow = search(&view, Direction::Outgoing, 1)
        .dag(n[0], true, None, |_, _| false)
        .unwrap();
    assert_eq!(shallow.order(), &[n[2], n[1]]);
}

#[test]
fn pair_search_tie_breaks_to_the_newest_relationship() {
    // LABS-255 / ADR-0203: two parallel relationships 0→1 (r[0] older,
    // r[1] newer) plus 1→2 (r[2]). The Neo4j reference expands a node's
    // relationship chain newest-first, so its equal-hop tie-break picks
    // the trail through r[1]; both pair-search strategies must agree.
    let Built {
        graph: g,
        nodes: n,
        rels: r,
    } = build(&[&[][..]; 3], &[(0, 1, "R"), (0, 1, "R"), (1, 2, "R")]);
    let view = ExecGraph::read(&g);
    // ADR-0037 shallow intersection (directed, max <= 3).
    let shallow = search(&view, Direction::Outgoing, 3);
    assert_eq!(pair(&shallow, n[0], n[2]).unwrap(), Some(vec![r[1], r[2]]));
    // ADR-0034 bidirectional BFS (max > 3, and the undirected pattern of
    // the openstreetmap walkthrough).
    let bidir = search(&view, Direction::Outgoing, 8);
    assert_eq!(pair(&bidir, n[0], n[2]).unwrap(), Some(vec![r[1], r[2]]));
    let undirected = search(&view, Direction::Both, 8);
    assert_eq!(
        pair(&undirected, n[0], n[2]).unwrap(),
        Some(vec![r[1], r[2]])
    );
}

#[test]
fn pair_search_returns_one_shortest_trail_on_both_strategies() {
    // 0→1→2→3 and the shortcut 0→4→3.
    let Built {
        graph: g,
        nodes: n,
        rels: r,
    } = build(
        &[&[][..]; 5],
        &[
            (0, 1, "R"),
            (1, 2, "R"),
            (2, 3, "R"),
            (0, 4, "R"),
            (4, 3, "R"),
        ],
    );
    let view = ExecGraph::read(&g);
    for (dir, max) in [
        (Direction::Outgoing, 3),
        (Direction::Outgoing, 8),
        (Direction::Both, 3),
    ] {
        let s = search(&view, dir, max);
        assert_eq!(pair(&s, n[0], n[3]).unwrap(), Some(vec![r[3], r[4]]));
        assert_eq!(
            pair(&s.reversed(), n[3], n[0]).unwrap(),
            Some(vec![r[4], r[3]])
        );
        assert_eq!(pair(&s, n[1], n[3]).unwrap(), Some(vec![r[1], r[2]]));
    }
    let s = search(&view, Direction::Outgoing, 3);
    assert_eq!(pair(&s, n[3], n[0]).unwrap(), None);
    assert_eq!(
        pair(&search(&view, Direction::Both, 1), n[0], n[3]).unwrap(),
        None
    );
    assert_eq!(
        pair(&search(&view, Direction::Both, 0), n[0], n[4]).unwrap(),
        None
    );
}

#[test]
fn every_search_counts_its_expansions() {
    let Built {
        graph: g, nodes: n, ..
    } = build(&[&[][..]; 3], &[(0, 1, "R"), (1, 2, "R")]);
    let view = ExecGraph::read(&g);
    let s = search(&view, Direction::Outgoing, 8);
    let (_, expands) = counting_expands(|| pair(&s, n[0], n[2]).unwrap());
    assert!(expands > 0);
    let (_, expands) = counting_expands(|| s.dag(n[0], false, None, |_, _| false).unwrap());
    assert_eq!(expands, 2);
}

#[test]
fn path_from_trail_walks_relationships_either_way() {
    let Built {
        graph: g,
        nodes: n,
        rels: r,
    } = build(&[&[][..]; 3], &[(0, 1, "R"), (2, 1, "R")]);
    let view = ExecGraph::read(&g);
    let path = path_from_trail(&view, n[0], &[r[0], r[1]]);
    let ids = |p: &cypherlite_core::Path| p.nodes.iter().map(|x| x.id).collect::<Vec<_>>();
    assert_eq!(ids(&path), n);
    assert_eq!(path.rels.iter().map(|x| x.id).collect::<Vec<_>>(), r);
}

#[test]
fn followable_needs_static_types_and_labels_and_no_property_map() {
    let shape = |cypher: &str| {
        let query = parse_and_lower(cypher).unwrap();
        let Some(Clause::Match { patterns, .. }) = query.parts[0].clauses.first() else {
            panic!("{cypher}: no MATCH");
        };
        let [PatternElement::Node(a), PatternElement::Relationship(r), PatternElement::Node(b)] =
            patterns[0].elements.as_slice()
        else {
            panic!("{cypher}: not a single relationship");
        };
        followable(a, r, b)
    };
    for ok in [
        "MATCH (a)-[*]->(b) RETURN a",
        "MATCH (a:A:B)-[:R|S*]-(b:C) RETURN a",
        "MATCH (a:A&B)-[:R*1..3]->(b) RETURN a",
    ] {
        assert!(shape(ok), "{ok}");
    }
    for rejected in [
        "MATCH (a:A|B)-[*]->(b) RETURN a",
        "MATCH (a)-[*]->(b:!B) RETURN a",
        "MATCH (a)-[:!R]->(b) RETURN a",
        "MATCH (a)-[:R&S]->(b) RETURN a",
        "MATCH (a)-[*1..3 {k: 1}]->(b) RETURN a",
    ] {
        assert!(!shape(rejected), "{rejected}");
    }
}
