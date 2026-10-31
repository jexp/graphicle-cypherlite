//! The projection's row contract: a `RETURN`/`WITH` whose items the match
//! sink already evaluated passes its rows through, and every other batch has
//! its items evaluated against the bindings — whichever names those bindings
//! happen to hold. The sink's caller decides which case it is; a row carries
//! no marker, so a user alias in the engine's internal `__` namespace can
//! neither forge the sink's state nor be overwritten by it.

use super::test_support::{build, exec_cypher, node_id};
use crate::graph::Graph;
use cypherlite_core::Value;

/// The harness report's minimal query: `WITH 1 AS __projected` hands the
/// next `RETURN` a batch whose only name is a user alias in the engine's
/// internal `__` namespace. The `RETURN` must project `x` (its item,
/// evaluated against the binding) instead of treating the batch as
/// preprojected and panicking on the absent column.
#[test]
fn user_alias_in_the_internal_namespace_is_projected_not_taken_for_sink_rows() {
    let r = exec_cypher(&Graph::default(), "WITH 1 AS __projected RETURN 1 AS x");
    assert_eq!(r.columns, vec!["x".to_string()]);
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
}

/// The other direction of the same collision: the match sink projects
/// `n AS __projected`, and the alias is the user's column — it must keep the
/// node the item evaluated to, not an internal value written over the same
/// name.
#[test]
fn sink_projection_keeps_a_user_column_in_the_internal_namespace() {
    let built = build(&[&["A"], &["A"]], &[(0, 1, "E")]);
    let r = exec_cypher(&built.graph, "MATCH (n:A {id: 0}) RETURN n AS __projected");
    assert_eq!(r.columns, vec!["__projected".to_string()]);
    assert_eq!(node_id(&r.rows[0][0]), built.nodes[0]);
}

/// A batch the sink declined keeps its bindings: `WITH`'s filter names `r`,
/// which the projection does not produce, so the sink leaves the clause to
/// the ordinary projection — whose output name `n` is already bound in those
/// rows. The item must still be evaluated (`n` is the node either way, but a
/// preprojected misread would move the binding out and drop `n`'s evaluation
/// of the alias).
#[test]
fn declined_sink_still_evaluates_items_over_bound_output_names() {
    let built = build(&[&["A"], &["A"]], &[(0, 1, "E")]);
    let r = exec_cypher(
        &built.graph,
        "MATCH (n:A)-[r:E]->(m) WITH n AS n WHERE r IS NOT NULL RETURN n",
    );
    assert_eq!(r.columns, vec!["n".to_string()]);
    assert_eq!(node_id(&r.rows[0][0]), built.nodes[0]);
    // The same sharp edge on the eager-page decline (`SKIP` keeps the
    // ordinary projection, CIP-27): the binding `n` is the output name.
    let r = exec_cypher(&built.graph, "MATCH (n:A {id: 0}) RETURN n SKIP 0");
    assert_eq!(node_id(&r.rows[0][0]), built.nodes[0]);
}

/// The sink still parks its `ORDER BY` keys on the row it produced and the
/// projection still sorts by them (the removed marker was not the only thing
/// riding that batch).
#[test]
fn sink_projection_orders_by_its_parked_keys() {
    let built = build(&[&["A"], &["A"], &["A"]], &[]);
    let r = exec_cypher(
        &built.graph,
        "MATCH (n:A) RETURN n.id AS id ORDER BY id DESC",
    );
    assert_eq!(r.columns, vec!["id".to_string()]);
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(2)],
            vec![Value::Int(1)],
            vec![Value::Int(0)]
        ]
    );
}

/// The `__` namespace is reserved against user aliases (OPLANE_REQ-00328162):
/// an `AS __ord0` alias cannot collide with the sink's parked sort keys —
/// the sink declines and the ordinary projection keeps the user column,
/// sorted by its real value rather than a forged key.
#[test]
fn user_alias_in_the_parked_key_namespace_does_not_collide() {
    let built = build(&[&["A"], &["A"], &["A"]], &[]);
    let r = exec_cypher(
        &built.graph,
        "MATCH (n:A) RETURN n.id AS __ord0 ORDER BY __ord0 DESC",
    );
    assert_eq!(r.columns, vec!["__ord0".to_string()]);
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(2)],
            vec![Value::Int(1)],
            vec![Value::Int(0)]
        ]
    );
}
