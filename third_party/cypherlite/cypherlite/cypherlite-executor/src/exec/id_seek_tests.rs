//! id()/elementId() equality seek tests (ADR-0155/LABS-157 hot path):
//! `id(x) = <expr>` / `id(x) IN <list>` (and the elementId spelling)
//! answer from the id-keyed storage instead of scanning every node.
//! Split from `pushdown_tests.rs` when that file grew past the 1k-line
//! review threshold; the string/property seek tests stayed there.

use super::test_support::{exec_cypher, node_id};
use super::*;
use crate::graph::Graph;
use cypherlite_core::NodeId;

/// `id(x) = <expr>` and `id(x) IN <list>` answer from the id-keyed storage
/// instead of scanning every node (the pull/import hot path: one lookup per
/// relationship endpoint).
#[test]
fn id_equality_seeks_instead_of_scanning() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["N".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["N".into()], BTreeMap::new()).unwrap();
    let c = g.create_node(vec!["N".into()], BTreeMap::new()).unwrap();
    g.commit_write();

    // Equality against a literal.
    let rows = exec_cypher(&g, &format!("MATCH (n) WHERE id(n) = {b} RETURN n")).rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(node_id(&rows[0][0]), b);

    // IN over a list of ids.
    let rows = exec_cypher(&g, &format!("MATCH (n) WHERE id(n) IN [{a}, {c}] RETURN n")).rows;
    assert_eq!(rows.len(), 2, "IN seek: {rows:?}");
    let mut ids: Vec<NodeId> = rows.iter().map(|r| node_id(&r[0])).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![a.min(c), a.max(c)]);

    // A duplicate list value stays one row: IN is set membership, the
    // seek dedups (a duplicate candidate row would be a regression).
    let rows = exec_cypher(&g, &format!("MATCH (n) WHERE id(n) IN [{a}, {a}] RETURN n")).rows;
    assert_eq!(rows.len(), 1, "duplicate IN value: {rows:?}");

    // A non-existent id yields no rows (not an error, not a scan hit).
    let rows = exec_cypher(&g, "MATCH (n) WHERE id(n) = 999999 RETURN n").rows;
    assert!(rows.is_empty());
}

/// The seek must not change semantics: a label on the pattern still filters
/// the seeked candidate.
#[test]
fn id_seek_keeps_pattern_semantics() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    g.commit_write();

    // The label filter still applies to the seeked candidate.
    let rows = exec_cypher(&g, &format!("MATCH (n:B) WHERE id(n) = {a} RETURN n")).rows;
    assert!(rows.is_empty(), "label must filter the seeked id: {rows:?}");
    let rows = exec_cypher(&g, &format!("MATCH (n:B) WHERE id(n) = {b} RETURN n")).rows;
    assert_eq!(rows.len(), 1);

    // A seeked id that was deleted between writes yields no rows.
    g.begin_write().unwrap();
    g.delete_node(a, true).unwrap();
    g.commit_write();
    let rows = exec_cypher(&g, &format!("MATCH (n) WHERE id(n) = {a} RETURN n")).rows;
    assert!(rows.is_empty(), "deleted id must not be returned: {rows:?}");
}

/// `elementId(x) = <string>` / `IN <list>` seeks too (the spelling generated
/// code and `cypherlite pull` use; `id()` is deprecated in Neo4j Cypher).
#[test]
fn element_id_equality_seeks_instead_of_scanning() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["N".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["N".into()], BTreeMap::new()).unwrap();
    g.commit_write();

    let rows = exec_cypher(
        &g,
        &format!("MATCH (n) WHERE elementId(n) = '4:cypherlite:n{b}' RETURN n"),
    )
    .rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(node_id(&rows[0][0]), b);

    let rows = exec_cypher(
        &g,
        &format!(
            "MATCH (n) WHERE elementId(n) IN ['4:cypherlite:n{a}', '4:cypherlite:n{b}'] RETURN n"
        ),
    )
    .rows;
    assert_eq!(rows.len(), 2);

    // A relationship element id never matches a node, and a foreign
    // namespace is not this engine's id space: no rows, no error.
    for bad in ["5:cypherlite:r0", "4:neo4j:n0", "garbage"] {
        let rows = exec_cypher(
            &g,
            &format!("MATCH (n) WHERE elementId(n) = '{bad}' RETURN n"),
        )
        .rows;
        assert!(rows.is_empty(), "{bad} must not match: {rows:?}");
    }
}
