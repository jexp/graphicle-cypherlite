//! ADR-0186 result contract of the BFS route for `MATCH`-clause shortest
//! paths.

use cypherlite_core::Value::{self, Bool, Int, Null};

use crate::exec::test_support::{build, ints};
use crate::graph::Graph;

use super::fixtures::{gpm, on_route, plain, run, shortcut, sorted, Counts};
use super::with_forced_gpm;

fn expect(rows: &[&[Value]]) -> Vec<Vec<Value>> {
    sorted(rows.iter().map(|r| r.to_vec()).collect())
}

#[test]
fn both_endpoints_bound() {
    let g = shortcut();
    let q = "MATCH (a {id: 0}), (b {id: 3}) \
             MATCH p = shortestPath((a)-[*]->(b)) RETURN [n IN nodes(p) | n.id]";
    assert_eq!(on_route(&g, q), expect(&[&[ints(&[0, 4, 3])]]));
}

#[test]
fn only_start_bound() {
    let g = shortcut();
    let q = "MATCH (a {id: 0}) MATCH p = shortestPath((a)-[*]->(b)) RETURN b.id, length(p)";
    assert_eq!(
        on_route(&g, q),
        expect(&[
            &[Int(1), Int(1)],
            &[Int(2), Int(2)],
            &[Int(3), Int(2)],
            &[Int(4), Int(1)],
        ])
    );
}

#[test]
fn only_end_bound_reports_start_to_end() {
    let g = shortcut();
    let q = "MATCH (b {id: 3}) MATCH p = shortestPath((a)-[*]->(b)) \
             RETURN a.id, [n IN nodes(p) | n.id]";
    assert_eq!(
        on_route(&g, q),
        expect(&[
            &[Int(0), ints(&[0, 4, 3])],
            &[Int(1), ints(&[1, 2, 3])],
            &[Int(2), ints(&[2, 3])],
            &[Int(3), ints(&[3, 1, 2, 3])],
            &[Int(4), ints(&[4, 3])],
        ])
    );
}

#[test]
fn both_unbound_with_labels() {
    let rels = [(0, 1), (1, 2), (3, 2), (2, 4), (0, 4)].map(|(a, b)| (a, b, "R"));
    let g = build(&[&["S"], &[], &["T"], &["S"], &["T"]], &rels).graph;
    let q = "MATCH p = shortestPath((a:S)-[*]->(b:T)) RETURN a.id, b.id, length(p)";
    let want = expect(&[
        &[Int(0), Int(2), Int(2)],
        &[Int(0), Int(4), Int(1)],
        &[Int(3), Int(2), Int(1)],
        &[Int(3), Int(4), Int(2)],
    ]);
    assert_eq!(on_route(&g, q), want);
    assert_eq!(gpm(&g, q), want);
}

#[test]
fn endpoint_maps_read_row_and_start_variables() {
    let g = shortcut();
    for (q, want) in [
        (
            "MATCH (x {id: 3}) MATCH p = shortestPath((a {id: 0})-[*]->(b {id: x.id})) \
             RETURN b.id, length(p)",
            expect(&[&[Int(3), Int(2)]]),
        ),
        (
            "MATCH p = shortestPath((a)-[*]->(b {id: a.id})) RETURN a.id, length(p)",
            expect(&[&[Int(1), Int(3)], &[Int(2), Int(3)], &[Int(3), Int(3)]]),
        ),
    ] {
        assert_eq!(on_route(&g, q), want, "{q}");
        assert_eq!(gpm(&g, q), want, "{q}");
    }
}

#[test]
fn same_variable_directed_two_cycle() {
    let g = plain(4, &[(0, 1), (1, 0), (0, 2), (2, 3), (3, 0)]);
    let q = "MATCH (a {id: 0}) MATCH p = shortestPath((a)-[*]->(a)) RETURN length(p)";
    assert_eq!(on_route(&g, q), expect(&[&[Int(2)]]));
    let q = "MATCH p = shortestPath((a)-[*]->(a)) RETURN a.id, length(p)";
    let want = expect(&[
        &[Int(0), Int(2)],
        &[Int(1), Int(2)],
        &[Int(2), Int(3)],
        &[Int(3), Int(3)],
    ]);
    assert_eq!(on_route(&g, q), want);
    assert_eq!(gpm(&g, q), want);
}

#[test]
fn undirected_closed_trail_on_triangle_and_single_edge() {
    let triangle = plain(3, &[(0, 1), (1, 2), (2, 0)]);
    let q = "MATCH (a {id: 0}) MATCH p = shortestPath((a)-[*]-(a)) RETURN length(p)";
    assert_eq!(on_route(&triangle, q), expect(&[&[Int(3)]]));
    let all = "MATCH (a {id: 0}) MATCH p = allShortestPaths((a)-[*]-(a)) \
               RETURN [r IN relationships(p) | r.k]";
    let want = expect(&[&[ints(&[0, 1, 2])], &[ints(&[2, 1, 0])]]);
    assert_eq!(on_route(&triangle, all), want);
    assert_eq!(gpm(&triangle, all), want);

    let edge = plain(2, &[(0, 1)]);
    assert!(
        on_route(&edge, q).is_empty(),
        "a trail cannot return over its only edge"
    );
}

#[test]
fn self_loop_closes_at_length_one() {
    let g = plain(2, &[(0, 1), (1, 0), (0, 0)]);
    for q in [
        "MATCH (a {id: 0}) MATCH p = shortestPath((a)-[*]->(a)) RETURN length(p)",
        "MATCH (a {id: 0}) MATCH p = shortestPath((a)-[*]-(a)) RETURN length(p)",
    ] {
        assert_eq!(on_route(&g, q), expect(&[&[Int(1)]]), "{q}");
    }
}

#[test]
fn zero_min_hops_on_same_node_is_length_zero() {
    let g = plain(2, &[(0, 1), (1, 0)]);
    let q = "MATCH (a {id: 0}) MATCH p = shortestPath((a)-[*0..2]->(a)) RETURN length(p)";
    let (rows, counts) = run(&g, q);
    assert_eq!(rows, vec![vec![Int(0)]]);
    assert_eq!(
        counts.route_runs, 1,
        "the zero-length path skipped the route"
    );
    assert_eq!(gpm(&g, q), expect(&[&[Int(0)]]));
}

#[test]
fn relationship_variable_binds_path_relationships_in_order() {
    let g = plain(4, &[(0, 1), (1, 2), (2, 3)]);
    for q in [
        "MATCH (a {id: 0}), (b {id: 3}) MATCH p = shortestPath((a)-[r*]->(b)) \
         RETURN [x IN r | x.k], r = relationships(p)",
        "MATCH (b {id: 3}) MATCH p = shortestPath((a {id: 0})-[r*]->(b)) \
         RETURN [x IN r | x.k], r = relationships(p)",
        "MATCH (a {id: 0}) MATCH p = allShortestPaths((a)-[r*]->(b {id: 3})) \
         RETURN [x IN r | x.k], r = relationships(p)",
    ] {
        assert_eq!(
            on_route(&g, q),
            expect(&[&[ints(&[0, 1, 2]), Bool(true)]]),
            "{q}"
        );
    }
    let q = "MATCH (b {id: 3}) MATCH p = shortestPath((b)<-[r*]-(a {id: 0})) \
             RETURN [x IN r | x.k], [n IN nodes(p) | n.id]";
    assert_eq!(
        on_route(&g, q),
        expect(&[&[ints(&[2, 1, 0]), ints(&[3, 2, 1, 0])]])
    );
}

#[test]
fn unreachable_pair_gives_no_row_and_optional_gives_null() {
    let g = plain(3, &[(0, 1), (2, 1)]);
    let q = "MATCH (a {id: 0}), (b {id: 2}) MATCH p = shortestPath((a)-[*]->(b)) RETURN p";
    assert!(on_route(&g, q).is_empty());
    let q = "MATCH (a {id: 0}), (b {id: 2}) \
             OPTIONAL MATCH p = shortestPath((a)-[*]->(b)) RETURN p";
    assert_eq!(on_route(&g, q), vec![vec![Null]]);
}

/// 0→1→3 and 0→2→3.
fn diamond() -> Graph {
    plain(4, &[(0, 1), (1, 3), (0, 2), (2, 3)])
}

#[test]
fn all_shortest_paths_on_diamond_gives_both_routes() {
    let g = diamond();
    let q = "MATCH (a {id: 0}), (b {id: 3}) MATCH p = allShortestPaths((a)-[*]->(b)) \
             RETURN [n IN nodes(p) | n.id]";
    assert_eq!(
        on_route(&g, q),
        expect(&[&[ints(&[0, 1, 3])], &[ints(&[0, 2, 3])]])
    );
}

#[test]
fn gql_selectors_match_function_forms() {
    let g = diamond();
    let count = |q: &str| on_route(&g, q).len();
    for (endpoints, prefix) in [
        ("(a {id: 0})-[*]->(b {id: 3})", ""),
        ("(a)-[*]->(b)", ""),
        ("(a)-[*]-(b)", ""),
        ("(a)-[*1..2]-(b {id: 3})", "MATCH (b {id: 3}) "),
    ] {
        let single = count(&format!(
            "{prefix}MATCH p = shortestPath({endpoints}) RETURN p"
        ));
        let all = count(&format!(
            "{prefix}MATCH p = allShortestPaths({endpoints}) RETURN p"
        ));
        assert!(all > single, "{endpoints}: all={all} single={single}");
        for gql in ["ANY SHORTEST", "SHORTEST 1"] {
            let q = format!("{prefix}MATCH p = {gql} {endpoints} RETURN p");
            assert_eq!(count(&q), single, "{q}");
        }
        for gql in ["SHORTEST 1 GROUP", "ALL SHORTEST"] {
            let q = format!("{prefix}MATCH p = {gql} {endpoints} RETURN p");
            assert_eq!(count(&q), all, "{q}");
        }
    }
}

#[test]
fn unbounded_var_length_is_not_capped() {
    let rels: Vec<_> = (0..40).map(|i| (i, i + 1)).collect();
    let g = plain(41, &rels);
    let q = "MATCH (a {id: 0}), (b {id: 40}) MATCH p = shortestPath((a)-[*]->(b)) \
             RETURN length(p)";
    assert_eq!(on_route(&g, q), expect(&[&[Int(40)]]));
    let q = "MATCH (a {id: 0}) MATCH p = shortestPath((a)-[*]->(b {id: 40})) \
             RETURN length(p)";
    assert_eq!(on_route(&g, q), expect(&[&[Int(40)]]));
}

#[test]
fn bound_end_runs_one_search_not_one_per_start_candidate() {
    // 50 :S starts feed a 10-node chain that ends at the bound :T node.
    let mut labels: Vec<&[&str]> = vec![&["S"]; 50];
    labels.extend([&[] as &[&str]; 10]);
    labels.push(&["T"]);
    let mut rels: Vec<(usize, usize, &str)> = (0..50).map(|i| (i, 50, "R")).collect();
    rels.extend((50..60).map(|i| (i, i + 1, "R")));
    let g = build(&labels, &rels).graph;
    let q = "MATCH (b:T) MATCH p = shortestPath((a:S)-[*]->(b)) RETURN length(p)";
    let (rows, counts) = run(&g, q);
    assert_eq!(rows.len(), 50);
    assert!(rows.iter().all(|r| r[0] == Int(11)));
    assert_eq!(counts.route_runs, 1);
    assert!(counts.expands > 0);
    assert!(
        counts.expands < 3 * rels.len() as u64,
        "expands={} for {} relationships",
        counts.expands,
        rels.len()
    );
}

#[test]
fn forced_gpm_switch_bypasses_the_route() {
    let g = shortcut();
    let q = "MATCH p = allShortestPaths((a)-[*]->(b)) \
             RETURN a.id, b.id, [r IN relationships(p) | r.k]";
    let route = on_route(&g, q);
    let (forced, counts) = with_forced_gpm(|| run(&g, q));
    assert_eq!(counts, Counts::default());
    assert_eq!(route, sorted(forced));
}
