use super::*;
use crate::exec::test_support::{build, exec_cypher, ints};
use crate::graph::Graph;
use cypherlite_ir::{PathCount, PathSelector};
use std::sync::Arc;

fn test_node(id: NodeId) -> cypherlite_core::Node {
    cypherlite_core::Node {
        id,
        labels: Arc::new(Vec::new()),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    }
}

fn selector_state(start: NodeId, end: NodeId, length: usize) -> GpmState {
    GpmState {
        row: Row::new(),
        used: HashSet::new(),
        current: end,
        nodes: vec![test_node(start), test_node(end)],
        rels: (0..length)
            .map(|index| cypherlite_core::Relationship {
                id: index as RelId,
                start,
                end,
                rel_type: "R".into(),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            })
            .collect(),
    }
}

#[test]
fn gpm_shortest_groups_are_partitioned_by_endpoints() {
    let states = vec![
        selector_state(1, 2, 1),
        selector_state(1, 2, 2),
        selector_state(1, 2, 2),
        selector_state(3, 4, 3),
        selector_state(3, 4, 4),
    ];
    let selected = apply_gpm_selector(
        states,
        Some(&PathSelector::Shortest {
            count: Some(PathCount::Literal(2)),
            group: true,
        }),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        selected
            .iter()
            .map(|state| state.rels.len())
            .collect::<Vec<_>>(),
        vec![1, 2, 2, 3, 4]
    );
}

/// `ANY k` keeps the `k` shortest members of a partition, sharing
/// `SHORTEST k`'s order: the reference chooses arbitrarily by walking
/// breadth-first, which the conformance suite pins as `ANY k` ==
/// `SHORTEST k` on every example row. Enumeration order here is DFS
/// (deepest first), so keeping "the first `count` enumerated" would select
/// the longest paths instead.
#[test]
fn any_keeps_the_shortest_members() {
    let states = || {
        vec![
            selector_state(1, 2, 3),
            selector_state(1, 2, 1),
            selector_state(1, 2, 2),
        ]
    };
    let one =
        apply_gpm_selector(states(), Some(&PathSelector::Any(None)), &BTreeMap::new()).unwrap();
    assert_eq!(
        one.iter().map(|state| state.rels.len()).collect::<Vec<_>>(),
        vec![1]
    );
    let two = apply_gpm_selector(
        states(),
        Some(&PathSelector::Any(Some(PathCount::Literal(2)))),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        two.iter().map(|state| state.rels.len()).collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[test]
fn gpm_acyclic_and_simple_apply_distinct_node_rules() {
    let cycle = vec![test_node(1), test_node(2), test_node(1)];
    assert!(!gpm_path_mode_allows(Some(PathMode::Acyclic), &cycle, &[]));
    assert!(gpm_path_mode_allows(Some(PathMode::Simple), &cycle, &[]));
    let repeated_interior = vec![test_node(1), test_node(2), test_node(2), test_node(3)];
    assert!(!gpm_path_mode_allows(
        Some(PathMode::Simple),
        &repeated_interior,
        &[]
    ));
}

// ---- End-to-end selector semantics (PathSelectorAcceptance shapes) ----

/// Rows of `q` over `graph`.
fn query_rows(graph: &Graph, q: &str) -> Vec<Vec<Value>> {
    exec_cypher(graph, q).rows
}

/// One row of a `collect([...])` of integer lists.
fn list_of_ints(lists: &[&[i64]]) -> Vec<Vec<Value>> {
    vec![vec![Value::List(
        lists.iter().map(|l| ints(l)).collect::<Vec<_>>().into(),
    )]]
}

/// The conformance suite's "Predicate outside parentheses in selective path
/// pattern is a postfilter" graph: a length-2 path carrying an `X`, a
/// length-3 and a length-4 path without one.
fn postfilter_graph() -> Graph {
    build(
        &[&["A"], &[], &["B"], &[], &[], &[], &[], &[]],
        &[
            (0, 1, "R"),
            (1, 2, "X"),
            (0, 3, "R"),
            (3, 4, "R"),
            (4, 2, "R"),
            (0, 5, "R"),
            (5, 6, "R"),
            (6, 7, "R"),
            (7, 2, "R"),
        ],
    )
    .graph
}

/// A predicate written outside the selective path pattern's parentheses is a
/// postfilter: the selector picks first, the predicate then drops rows. The
/// counts pin that `ANY k` picks the `k` shortest paths (its arbitrary
/// choice in the reference's breadth-first order), not the enumerated ones —
/// `ANY` and `ANY 1` therefore see only the `X`-carrying shortest path.
#[test]
fn predicate_outside_a_selective_path_pattern_is_a_postfilter() {
    let graph = postfilter_graph();
    let count = |selector: &str| {
        query_rows(
            &graph,
            &format!(
                "MATCH p = {selector} (:A)-->*(:B) \
                 WHERE none(r IN relationships(p) WHERE r:X) \
                 RETURN count(*) AS result"
            ),
        )[0][0]
            .clone()
    };
    assert_eq!(count("ANY SHORTEST"), Value::Int(0));
    assert_eq!(count("SHORTEST 1"), Value::Int(0));
    assert_eq!(count("SHORTEST 2"), Value::Int(1));
    assert_eq!(count("SHORTEST 3"), Value::Int(2));
    assert_eq!(count("ALL SHORTEST"), Value::Int(0));
    assert_eq!(count("ANY"), Value::Int(0));
    assert_eq!(count("ANY 1"), Value::Int(0));
    assert_eq!(count("ANY 2"), Value::Int(1));
    assert_eq!(count("ANY 3"), Value::Int(2));
}

/// The conformance suite's disjoint-path graph: two length-2 paths (one
/// through `X`), a length-3 path carrying an `X`, and a clean length-3 path.
fn disjoint_graph() -> Graph {
    build(
        &[&["A"], &["X"], &["B"], &[], &[], &["X"], &[], &[]],
        &[
            (0, 1, "R"),
            (1, 2, "R"),
            (0, 3, "R"),
            (3, 2, "R"),
            (0, 4, "R"),
            (4, 5, "R"),
            (5, 2, "R"),
            (0, 6, "R"),
            (6, 7, "R"),
            (7, 2, "R"),
        ],
    )
    .graph
}

/// The two-selector disjoint-path query both disjoint-selector tests run:
/// the first selector picks a clean path, the second must return the
/// distinct path disjoint from it.
fn disjoint_query(selector: &str) -> String {
    format!(
        "MATCH {selector} (p = (:A)-->+(:B) WHERE none(n IN nodes(p) WHERE n:X)) \
         MATCH {selector} (q = (:A)-->+(:B) WHERE q <> p AND length(p) = length(q)) \
         WITH length(p) AS pathLength, \
              size([n IN nodes(p) WHERE n:X]) AS pxCount, \
              size([n IN nodes(q) WHERE n:X]) AS qxCount \
         ORDER BY pathLength \
         RETURN collect([pathLength, pxCount, qxCount]) AS result"
    )
}

/// A predicate written inside the selective path pattern's parentheses is a
/// restrictor: it narrows the candidates before the selector picks. Two
/// `SHORTEST` selectors therefore find two *different* paths — `q <> p`
/// is evaluated while `q` is still a candidate, so the second selector can
/// return the disjoint path instead of comparing against a path it already
/// chose.
#[test]
fn disjoint_shortest_selectors_return_the_distinct_second_path() {
    let graph = disjoint_graph();
    let disjoint = |selector: &str| query_rows(&graph, &disjoint_query(selector));
    assert_eq!(disjoint("ANY SHORTEST"), list_of_ints(&[&[2, 0, 1]]));
    assert_eq!(disjoint("SHORTEST 1"), list_of_ints(&[&[2, 0, 1]]));
    assert_eq!(
        disjoint("SHORTEST 2"),
        list_of_ints(&[&[2, 0, 1], &[3, 0, 1]])
    );
    assert_eq!(disjoint("ANY"), list_of_ints(&[&[2, 0, 1]]));
    assert_eq!(disjoint("ANY 1"), list_of_ints(&[&[2, 0, 1]]));
    assert_eq!(disjoint("ANY 2"), list_of_ints(&[&[2, 0, 1], &[3, 0, 1]]));
}

/// The parameterized spelling of the same two selectors resolves its counts
/// identically (`ANY $two` == `SHORTEST $two`).
#[test]
fn parameterized_any_on_disjoint_selectors_matches_shortest() {
    let graph = disjoint_graph();
    let params = BTreeMap::from([
        ("one".to_string(), Value::Int(1)),
        ("two".to_string(), Value::Int(2)),
    ]);
    let disjoint = |selector: &str, params: &BTreeMap<String, Value>| {
        crate::exec::test_support::try_exec_cypher_params(&graph, &disjoint_query(selector), params)
            .unwrap()
            .rows
    };
    assert_eq!(
        disjoint("SHORTEST $two", &params),
        list_of_ints(&[&[2, 0, 1], &[3, 0, 1]])
    );
    assert_eq!(
        disjoint("ANY $two", &params),
        list_of_ints(&[&[2, 0, 1], &[3, 0, 1]])
    );
    assert_eq!(disjoint("ANY $one", &params), list_of_ints(&[&[2, 0, 1]]));
}

/// The restrictor is what makes the first selector pick the shortest clean
#[test]
fn a_path_pattern_predicate_restricts_before_the_selector_picks() {
    let graph = disjoint_graph();
    let rows = query_rows(
        &graph,
        "MATCH SHORTEST 1 (p = (:A)-->+(:B) WHERE none(n IN nodes(p) WHERE n:X)) \
         RETURN length(p) AS len",
    );
    assert_eq!(rows, vec![vec![Value::Int(2)]]);
}

/// `ANY k` and `SHORTEST k` agree on every conformance example row; on a
/// graph whose paths differ in length the arbitrary choice is the shortest.
#[test]
fn any_selects_the_same_paths_as_shortest() {
    // The conformance suite's "Predicate not required when making subpath
    // variable declaration" graph: a 6-node ring with two L-labelled
    // spokes.
    let graph = build(
        &[&["L"], &[], &["L"], &["L"], &[], &[]],
        &[
            (0, 1, "R"),
            (1, 2, "R"),
            (2, 3, "R"),
            (3, 5, "R"),
            (5, 4, "R"),
            (4, 0, "R"),
        ],
    )
    .graph;
    let lengths = |selector: &str| {
        query_rows(
            &graph,
            &format!(
                "MATCH {selector} (p = (:L)--+(:L)) \
                 WITH length(p) AS pathLength, count(*) AS count ORDER BY pathLength \
                 RETURN collect([pathLength, count]) AS result"
            ),
        )
    };
    let shortest = lengths("ANY SHORTEST");
    assert_eq!(
        shortest,
        list_of_ints(&[&[1, 2], &[2, 2], &[3, 2], &[6, 3]])
    );
    assert_eq!(lengths("ANY"), shortest);
    assert_eq!(lengths("ANY 1"), shortest);
    let second = lengths("SHORTEST 2");
    assert_eq!(
        second,
        list_of_ints(&[&[1, 2], &[2, 2], &[3, 4], &[4, 2], &[5, 2], &[6, 6]])
    );
    assert_eq!(lengths("ANY 2"), second);
}
