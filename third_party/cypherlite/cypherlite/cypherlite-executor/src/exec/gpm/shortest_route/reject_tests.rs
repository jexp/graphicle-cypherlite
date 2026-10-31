//! Patterns outside ADR-0186's routed selectors and shape keep GPM
//! enumerate-then-select: the guard rejects them, the route neither runs
//! nor expands, and the rows are those of the GPM route.

use std::collections::{BTreeMap, HashSet};

use cypherlite_core::Value;
use cypherlite_front::parse_and_lower;
use cypherlite_ir::{Clause, MatchMode, PathPattern};

use crate::eval::Row;
use crate::exec::test_support::try_exec_cypher_params;
use crate::graph::Graph;

use super::fixtures::{counting, gpm, plain, run, shortcut, sorted, Counts};
use super::{route_shape, with_forced_gpm};

/// The match mode and first pattern of the statement's last `MATCH` clause.
fn last_match(cypher: &str) -> (MatchMode, PathPattern) {
    let query = parse_and_lower(cypher).unwrap();
    query.parts[0]
        .clauses
        .iter()
        .rev()
        .find_map(|c| match c {
            Clause::Match {
                match_mode,
                patterns,
                ..
            } => Some((*match_mode, patterns.first()?.clone())),
            _ => None,
        })
        .unwrap()
}

fn guard_accepts(cypher: &str) -> bool {
    let (mode, path) = last_match(cypher);
    route_shape(&path, mode, &HashSet::new(), &Row::new()).is_some()
}

/// `q` is rejected by the guard, neither runs nor expands on the route, and
/// returns `want` lengths, the rows the GPM route gives.
fn assert_stays_on_gpm(g: &Graph, q: &str, want: &[i64]) {
    assert!(!guard_accepts(q), "guard accepted {q}");
    let (rows, counts) = run(g, q);
    assert_eq!(counts, Counts::default(), "route ran for {q}");
    let rows = sorted(rows);
    assert_eq!(rows, gpm(g, q), "{q}");
    let want: Vec<_> = want.iter().map(|&n| vec![Value::Int(n)]).collect();
    assert_eq!(rows, want, "{q}");
}

#[test]
fn routed_control_is_accepted() {
    for q in [
        "MATCH p = shortestPath((a {id: 0})-[*]->(b {id: 3})) RETURN length(p)",
        "MATCH p = allShortestPaths((a)-[:R|S*0..4]-(b:B)) RETURN length(p)",
        "MATCH p = ANY SHORTEST (a)-[*]->(b) RETURN length(p)",
        "MATCH p = SHORTEST 1 TRAIL (a)-[*1..]->(b) RETURN length(p)",
        "MATCH p = SHORTEST 1 WALK GROUP (a:A:B)-[*]->(b) RETURN length(p)",
        "MATCH p = ALL SHORTEST (a {id: 0})-[*]->(b) RETURN length(p)",
    ] {
        assert!(guard_accepts(q), "guard rejected {q}");
    }
}

#[test]
fn shortest_k_greater_than_one_stays_on_gpm() {
    let g = shortcut();
    let q = "MATCH p = SHORTEST 2 (a {id: 0})-[*]->(b {id: 3}) RETURN length(p)";
    assert_stays_on_gpm(&g, q, &[2, 3]);
}

#[test]
fn parameter_count_stays_on_gpm() {
    let g = shortcut();
    let q = "MATCH p = SHORTEST $k (a {id: 0})-[*]->(b {id: 3}) RETURN length(p)";
    assert!(!guard_accepts(q));
    let params = BTreeMap::from([("k".to_string(), Value::Int(1))]);
    let exec = || try_exec_cypher_params(&g, q, &params).unwrap().rows;
    let (rows, counts) = counting(exec);
    assert_eq!(counts, Counts::default());
    assert_eq!(sorted(rows.clone()), sorted(with_forced_gpm(exec)));
    assert_eq!(rows, vec![vec![Value::Int(2)]]);
}

/// [`shortcut`] without its 3→1 cycle: exactly two trails from 0 to 3, so
/// `ANY 2` returns both whatever order GPM enumerates them in.
fn two_trails() -> Graph {
    plain(5, &[(0, 1), (1, 2), (2, 3), (0, 4), (4, 3)])
}

#[test]
fn any_k_stays_on_gpm() {
    let g = two_trails();
    let q = "MATCH p = ANY 2 (a {id: 0})-[*]->(b {id: 3}) RETURN length(p)";
    assert_stays_on_gpm(&g, q, &[2, 3]);
}

#[test]
fn acyclic_and_simple_modes_stay_on_gpm() {
    let g = shortcut();
    for mode in ["ACYCLIC", "SIMPLE"] {
        let q = format!(
            "MATCH p = ANY SHORTEST {mode} (a {{id: 0}})-[*]->(b {{id: 3}}) RETURN length(p)"
        );
        assert_stays_on_gpm(&g, &q, &[2]);
    }
}

#[test]
fn min_hops_above_one_stays_on_gpm() {
    let g = shortcut();
    let q = "MATCH p = shortestPath((a {id: 0})-[*2..5]->(b {id: 3})) RETURN length(p)";
    assert_stays_on_gpm(&g, q, &[2]);
}

#[test]
fn relationship_property_map_stays_on_gpm() {
    let g = shortcut();
    let q = "MATCH p = shortestPath((a {id: 0})-[*1..3 {k: 0}]->(b)) RETURN length(p)";
    assert_stays_on_gpm(&g, q, &[1]);
}

#[test]
fn inline_node_where_stays_on_gpm() {
    let g = shortcut();
    let q = "MATCH p = ANY SHORTEST (a {id: 0})-[*]->(b WHERE b.id = 3) RETURN length(p)";
    assert_stays_on_gpm(&g, q, &[2]);
}

#[test]
fn dynamic_relationship_type_stays_on_gpm() {
    let g = shortcut();
    let q = "WITH 'R' AS t MATCH (a {id: 0}), (b {id: 3}) \
             MATCH p = shortestPath((a)-[:$(t)*]->(b)) RETURN length(p)";
    assert_stays_on_gpm(&g, q, &[2]);
}

#[test]
fn quantified_relationship_stays_on_gpm() {
    let g = shortcut();
    let q = "MATCH p = ANY SHORTEST (a {id: 0})-[:R]->{1,3}(b {id: 3}) RETURN length(p)";
    assert_stays_on_gpm(&g, q, &[2]);
}

#[test]
fn repeatable_elements_stays_on_gpm() {
    let g = shortcut();
    let q = "MATCH REPEATABLE ELEMENTS p = ANY SHORTEST (a {id: 0})-[*1..4]->(b {id: 3}) \
             RETURN length(p)";
    assert_eq!(last_match(q).0, MatchMode::RepeatableElements);
    assert_stays_on_gpm(&g, q, &[2]);
}

#[test]
fn non_empty_used_set_or_bound_variables_reject() {
    let q = "MATCH p = ANY SHORTEST (a)-[r*]->(b) RETURN p";
    let (mode, path) = last_match(q);
    assert_eq!(mode, MatchMode::DifferentRelationships);
    assert!(route_shape(&path, mode, &HashSet::new(), &Row::new()).is_some());
    assert!(route_shape(&path, mode, &HashSet::from([7]), &Row::new()).is_none());
    assert!(!guard_accepts(
        "MATCH p = SHORTEST GROUP (a)-[r*]->(b) RETURN p"
    ));
    for (var, value) in [
        ("r", Value::List(Vec::new().into())),
        ("a", Value::Null),
        ("b", Value::Int(1)),
    ] {
        let row = Row::from([(var.to_string(), value)]);
        assert!(
            route_shape(&path, mode, &HashSet::new(), &row).is_none(),
            "{var}"
        );
    }
}
