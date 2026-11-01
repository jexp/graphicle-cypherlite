//! Relationship id()/elementId() equality seek tests (LABS-161), the rel
//! twin of `id_seek_tests.rs`: `elementId(r) = <expr>` / `id(r) = <expr>`
//! on the first rel of a pattern resolves the RelId by parsing the
//! transparent `5:cypherlite:r<id>` form and seeds the walk from that
//! rel's endpoint, instead of scanning every node and walking each
//! adjacency per row.

use super::test_support::{exec_cypher, rel_id, try_exec_cypher_params};
use super::*;
use crate::graph::Graph;
use cypherlite_core::RelId;

/// 40 nodes in a chain of 40 rels; returns the middle rel's id and the
/// rel every query below seeks.
fn chain(size: i64) -> (Graph, RelId) {
    let g = Graph::default();
    g.begin_write().unwrap();
    let mut prev = g.create_node(vec!["N".into()], BTreeMap::new()).unwrap();
    let mut sought = RelId::default();
    for i in 0..size {
        let n = g.create_node(vec!["N".into()], BTreeMap::new()).unwrap();
        let rid = g.create_rel(prev, n, "R".into(), BTreeMap::new()).unwrap();
        if i == size / 2 {
            sought = rid;
        }
        prev = n;
    }
    g.commit_write();
    (g, sought)
}

/// `elementId(r) = <string>` seeks the id-keyed rel storage instead of
/// walking every node's adjacency (the rel-heavy re-pull hot path).
#[test]
fn element_id_equality_seeks_instead_of_walking() {
    let (g, rid) = chain(40);

    take_neighbor_walks();
    let rows = exec_cypher(
        &g,
        &format!("MATCH ()-[r]->() WHERE elementId(r) = '5:cypherlite:r{rid}' RETURN r"),
    )
    .rows;
    let walks = take_neighbor_walks();
    assert_eq!(rows.len(), 1);
    assert_eq!(rel_id(&rows[0][0]), rid);
    assert_eq!(walks, 1, "the seek must not walk the adjacency: {walks}");

    // A parameter value seeks too (the pull loader's per-row shape).
    let rows = try_exec_cypher_params(
        &g,
        "MATCH ()-[r]->() WHERE elementId(r) = $eid RETURN r",
        &BTreeMap::from([(
            "eid".into(),
            Value::String(format!("5:cypherlite:r{rid}").into()),
        )]),
    )
    .unwrap()
    .rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rel_id(&rows[0][0]), rid);

    // A non-existent rel id yields no rows (not an error, not a scan hit).
    let rows = exec_cypher(
        &g,
        "MATCH ()-[r]->() WHERE elementId(r) = '5:cypherlite:r999999' RETURN r",
    )
    .rows;
    assert!(rows.is_empty());
}

/// `id(r) = <int>` seeks too, like the node side's `id(n)` form.
#[test]
fn id_equality_seeks_instead_of_walking() {
    let (g, rid) = chain(40);

    take_neighbor_walks();
    let rows = exec_cypher(
        &g,
        &format!("MATCH ()-[r]->() WHERE id(r) = {rid} RETURN r"),
    )
    .rows;
    let walks = take_neighbor_walks();
    assert_eq!(rows.len(), 1);
    assert_eq!(rel_id(&rows[0][0]), rid);
    assert_eq!(walks, 1, "the seek must not walk the adjacency: {walks}");

    let rows = exec_cypher(&g, "MATCH ()-[r]->() WHERE id(r) = 999999 RETURN r").rows;
    assert!(rows.is_empty());
}

/// A node element id never matches a relationship, and a foreign
/// namespace is not this engine's id space: no rows, no error.
#[test]
fn non_rel_element_id_matches_nothing() {
    let (g, _) = chain(4);
    for bad in ["4:cypherlite:n0", "5:neo4j:r0", "garbage"] {
        let rows = exec_cypher(
            &g,
            &format!("MATCH ()-[r]->() WHERE elementId(r) = '{bad}' RETURN r"),
        )
        .rows;
        assert!(rows.is_empty(), "{bad} must not match: {rows:?}");
    }
}

/// The seek must not change semantics: the pattern's type filter still
/// applies to the seeked rel, and a rel deleted between writes yields no
/// rows.
#[test]
fn seek_keeps_pattern_semantics() {
    let (g, rid) = chain(4);

    let rows = exec_cypher(
        &g,
        &format!("MATCH ()-[r:NOPE]->() WHERE elementId(r) = '5:cypherlite:r{rid}' RETURN r"),
    )
    .rows;
    assert!(rows.is_empty(), "type filter must reject the seeked rel");
    let rows = exec_cypher(
        &g,
        &format!("MATCH ()-[r:R]->() WHERE elementId(r) = '5:cypherlite:r{rid}' RETURN r"),
    )
    .rows;
    assert_eq!(rows.len(), 1);

    g.begin_write().unwrap();
    g.delete_rel(rid);
    g.commit_write();
    let rows = exec_cypher(
        &g,
        &format!("MATCH ()-[r]->() WHERE elementId(r) = '5:cypherlite:r{rid}' RETURN r"),
    )
    .rows;
    assert!(
        rows.is_empty(),
        "deleted rel must not be returned: {rows:?}"
    );
}

/// A row-bound rel variable forces the same seek: re-matching a rel
/// pulled in an earlier clause is one hop per row, not one adjacency
/// walk per node per row.
#[test]
fn row_bound_rel_seeks() {
    let (g, _) = chain(40);

    take_neighbor_walks();
    let rows = exec_cypher(
        &g,
        "MATCH ()-[r]->() WITH r MATCH (a)-[r]->(b) RETURN id(r) AS rid",
    )
    .rows;
    let walks = take_neighbor_walks();
    assert_eq!(rows.len(), 40);
    // 40 walks for the first scan, then exactly one forced hop per
    // re-match — not 40 adjacency walks per row.
    assert_eq!(walks, 80, "row-bound re-match must seek: {walks}");
}

/// Shapes the seek declines keep walking, with unchanged results: an
/// `IN` list and an undirected pattern (which matches each rel in both
/// orientations).
#[test]
fn declined_shapes_keep_walking() {
    let (g, rid) = chain(4);
    let other = rid + 1;

    let rows = exec_cypher(
        &g,
        &format!(
            "MATCH ()-[r]->() WHERE elementId(r) IN ['5:cypherlite:r{rid}', '5:cypherlite:r{other}'] RETURN r"
        ),
    )
    .rows;
    let mut ids: Vec<RelId> = rows.iter().map(|r| rel_id(&r[0])).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![rid.min(other), rid.max(other)]);

    // Undirected: both orientations of the rel match.
    let rows = exec_cypher(
        &g,
        &format!("MATCH ()-[r]-() WHERE elementId(r) = '5:cypherlite:r{rid}' RETURN r"),
    )
    .rows;
    assert_eq!(rows.len(), 2, "undirected matches both orientations");
}

/// The one-hop aggregate fold (ADR-0036, the `RETURN count(*)` shape of
/// the LABS-161 repro) seeks too: one hop per row, not one adjacency
/// walk per node per row.
#[test]
fn aggregate_fold_seeks_instead_of_walking() {
    let (g, rid) = chain(40);

    take_neighbor_walks();
    let rows = exec_cypher(
        &g,
        &format!(
            "UNWIND range(1, 20) AS _ MATCH ()-[x]->() WHERE elementId(x) = '5:cypherlite:r{rid}' RETURN count(*)"
        ),
    )
    .rows;
    let walks = take_neighbor_walks();
    assert_eq!(rows[0][0].as_int(), Some(20));
    assert_eq!(walks, 20, "the fold's seek must not walk: {walks}");

    // A miss folds to zero rows' worth of count, not an error.
    let rows = exec_cypher(
        &g,
        "MATCH ()-[x]->() WHERE elementId(x) = '5:cypherlite:r999999' RETURN count(*)",
    )
    .rows;
    assert_eq!(rows[0][0].as_int(), Some(0));

    // A node element id still matches nothing through the fold.
    let rows = exec_cypher(
        &g,
        "MATCH ()-[x]->() WHERE elementId(x) = '4:cypherlite:n0' RETURN count(*)",
    )
    .rows;
    assert_eq!(rows[0][0].as_int(), Some(0));

    // The type filter still applies through the fold.
    let rows = exec_cypher(
        &g,
        &format!(
            "MATCH ()-[x:NOPE]->() WHERE elementId(x) = '5:cypherlite:r{rid}' RETURN count(*)"
        ),
    )
    .rows;
    assert_eq!(rows[0][0].as_int(), Some(0));
}

/// A pattern node row-bound to null or a non-node matches nothing —
/// everywhere (match_node, node_candidates) and so in the fold too, on
/// both seats: the seek's start-incidence check and the offer's
/// far-node check. Before the fix a mismatching binding fell through
/// as an accepted hop.
#[test]
fn aggregate_fold_seek_rejects_mismatched_row_bindings() {
    let (g, rid) = chain(4);
    let seek = format!("elementId(x) = '5:cypherlite:r{rid}'");
    let count = |q: String| exec_cypher(&g, &q).rows[0][0].as_int();

    // Sanity: unbound seats accept the one seeked hop.
    assert_eq!(
        count(format!("MATCH (s)-[x]->(e) WHERE {seek} RETURN count(*)")),
        Some(1)
    );

    // Null and non-node start bindings match nothing (the seek's
    // incidence check answers Gone). A scalar in a node seat is a
    // bind-time VariableTypeConflict, so the non-node state arrives
    // here only defensively; a node bound to the wrong node exercises
    // the same decline arm. r2 runs n2 -> n3.
    assert_eq!(
        count(format!(
            "OPTIONAL MATCH (s:Missing) WITH s MATCH (s)-[x]->(e) WHERE {seek} RETURN count(*)"
        )),
        Some(0)
    );
    // A resolved-but-non-incident start is a Gone seek: the fold skips
    // the row without walking any adjacency — the regression guard for
    // the `anchored`-helper bug where Gone fell through to the scan and
    // paid the full adjacency walk per row.
    take_neighbor_walks();
    assert_eq!(
        count(format!(
            "MATCH (s) WHERE id(s) = 0 WITH s MATCH (s)-[x]->(e) WHERE {seek} RETURN count(*)"
        )),
        Some(0)
    );
    assert_eq!(
        take_neighbor_walks(),
        0,
        "Gone seek must not walk adjacency"
    );
    assert_eq!(
        count(format!(
            "MATCH (s) WHERE id(s) = 2 WITH s MATCH (s)-[x]->(e) WHERE {seek} RETURN count(*)"
        )),
        Some(1)
    );

    // Null and non-node end bindings match nothing (the offer declines).
    // The planner starts the fold from the row-bound side, so the end
    // seat is only reachable as nodes[1] when the start is bound too:
    // bound to the incident node 2, the end binding decides the hop.
    assert_eq!(
        count(format!(
            "MATCH (s) WHERE id(s) = 2 WITH s OPTIONAL MATCH (e:Missing) WITH s, e MATCH (s)-[x]->(e) WHERE {seek} RETURN count(*)"
        )),
        Some(0)
    );
    assert_eq!(
        count(format!(
            "MATCH (s) WHERE id(s) = 2 WITH s MATCH (e) WHERE id(e) = 0 WITH s, e MATCH (s)-[x]->(e) WHERE {seek} RETURN count(*)"
        )),
        Some(0)
    );
    assert_eq!(
        count(format!(
            "MATCH (s) WHERE id(s) = 2 WITH s MATCH (e) WHERE id(e) = 3 WITH s, e MATCH (s)-[x]->(e) WHERE {seek} RETURN count(*)"
        )),
        Some(1)
    );

    // The scan path (no seek conjunct) applies the same rule to the end.
    let rows = exec_cypher(
        &g,
        "OPTIONAL MATCH (e:Missing) WITH e MATCH (s)-[x]->(e) RETURN count(*)",
    )
    .rows;
    assert_eq!(rows[0][0].as_int(), Some(0));
}
