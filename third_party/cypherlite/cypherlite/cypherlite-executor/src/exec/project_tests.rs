//! The projection's row contract: a `RETURN`/`WITH` whose items the match
//! sink already evaluated passes its rows through, and every other batch has
//! its items evaluated against the bindings — whichever names those bindings
//! happen to hold. The sink's caller decides which case it is; a row carries
//! no marker, so a user alias in the engine's internal `__` namespace can
//! neither forge the sink's state nor be overwritten by it.

use super::test_support::{build, column, exec_cypher, node_id, try_exec_cypher};
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

/// Alias scope in `ORDER BY`: `b AS a` re-binds `a`, so `ORDER BY a.p` sorts
/// by the projected map's `p`, not by the incoming `a.p` — even though the
/// projection also computes `a.p` as its own column `k` under the incoming
/// binding (the harness's `GroupBy`/`ProjectionSubclause`/`With` scenarios).
#[test]
fn order_by_resolves_a_grouped_projection_alias_over_the_incoming_binding() {
    let r = exec_cypher(
        &Graph::default(),
        "UNWIND [{a: {p: 1}, b: 20, c: 100}, {a: {p: 2}, b: 10, c: 300}] AS row \
         WITH row.a AS a, {p: row.b} AS b, row.c AS c \
         RETURN a.p AS k, b AS a, sum(c) AS s GROUP BY ALL ORDER BY a.p, s",
    );
    assert_eq!(
        r.columns,
        vec!["k".to_string(), "a".to_string(), "s".to_string()]
    );
    assert_eq!(r.rows[0][0], Value::Int(2));
    assert_eq!(r.rows[0][2], Value::Int(300));
    assert_eq!(r.rows[1][0], Value::Int(1));
    assert_eq!(r.rows[1][2], Value::Int(100));
}

/// Implicit grouping keeps the deprecated resolution on a `WITH` that pages
/// with `LIMIT` (the top-k selection path): `a.p` names a variable the
/// projection also re-binds (`{p: b} AS a`), so it keeps the incoming
/// `a.p` — the two rows kept are the ones with the lowest incoming `a.p`
/// (`ProjectionSubclauseDeprecation [21]`).
#[test]
fn with_order_by_keeps_the_incoming_binding_under_implicit_grouping() {
    let r = exec_cypher(
        &Graph::default(),
        "UNWIND [{a: {p: 1}, b: 20}, {a: {p: 2}, b: 10}, {a: {p: 3}, b: 5}] AS row \
         WITH row.a AS a, row.b AS b \
         WITH a.p AS k, {p: b} AS a ORDER BY a.p LIMIT 2 \
         RETURN k",
    );
    assert_eq!(r.columns, vec!["k".to_string()]);
    assert_eq!(r.rows, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}

/// The match sink parks its `ORDER BY` keys per row and takes the same
/// decision: `y AS a` shadows the matched `a`, but under implicit grouping
/// `ORDER BY a.id` keeps the incoming `a.id` (the item `a.id AS j`, evaluated
/// against the incoming `a`), not the projected `a` (the `y` nodes).
#[test]
fn sink_order_by_keeps_the_incoming_binding_under_implicit_grouping() {
    let built = build(&[&["A"], &["A"], &["B"], &["B"]], &[]);
    let r = exec_cypher(
        &built.graph,
        "MATCH (a:A), (y:B) RETURN y.id AS k, y AS a, a.id AS j ORDER BY a.id",
    );
    assert_eq!(
        r.columns,
        vec!["k".to_string(), "a".to_string(), "j".to_string()]
    );
    let k = column(&r, "k", |value| Some(value.clone()));
    // The `A` nodes carry ids 0 and 1, the `B` nodes 2 and 3.
    assert_eq!(
        k,
        vec![Value::Int(2), Value::Int(3), Value::Int(2), Value::Int(3)]
    );
}

/// An explicit `GROUP BY ALL` establishes the alias scope even for a key that
/// is not a bare alias: `ORDER BY a.p` sorts by the projected `a` map
/// (`GroupByAcceptance [74]`).
#[test]
fn group_by_all_resolves_a_projection_alias_over_the_incoming_binding() {
    let r = exec_cypher(
        &Graph::default(),
        "UNWIND [{a: {p: 1}, b: 20}, {a: {p: 2}, b: 10}, {a: {p: 3}, b: 5}] AS row \
         WITH row.a AS a, row.b AS b \
         WITH a.p AS k, {p: b} AS a GROUP BY ALL ORDER BY a.p LIMIT 2 \
         RETURN k",
    );
    assert_eq!(r.columns, vec!["k".to_string()]);
    assert_eq!(r.rows, vec![vec![Value::Int(3)], vec![Value::Int(2)]]);
}

/// The aggregation path answers an `ORDER BY` key's aggregate call from the
/// group and its non-aggregate names from the projected row — so `a` in
/// `a + sum(b.p)` is the projected `a.p` alias, and the key sorts by
/// `a + s` (the harness's `[69]` scenario). Reading the incoming `a` map
/// there would leave every key null and keep the group order.
#[test]
fn order_by_aggregate_key_resolves_the_projection_alias() {
    let r = exec_cypher(
        &Graph::default(),
        "UNWIND [{a: {p: 1}, b: {p: 10}}, {a: {p: 2}, b: {p: 1}}, \
         {a: {p: 2}, b: {p: 2}}, {a: {p: 3}, b: {p: 5}}] AS row \
         WITH row.a AS a, row.b AS b \
         RETURN a.p AS a, sum(b.p) AS s ORDER BY a + sum(b.p)",
    );
    assert_eq!(r.columns, vec!["a".to_string(), "s".to_string()]);
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(2), Value::Int(3)],
            vec![Value::Int(3), Value::Int(5)],
            vec![Value::Int(1), Value::Int(10)],
        ]
    );
}

/// The same key scope on a `WITH` (the harness's `[76]` scenario): the
/// aggregating projection parks the key, and the following `RETURN` keeps
/// the order the `WITH` produced.
#[test]
fn with_order_by_aggregate_key_resolves_the_projection_alias() {
    let r = exec_cypher(
        &Graph::default(),
        "UNWIND [{a: {p: 1}, b: {p: 10}}, {a: {p: 2}, b: {p: 1}}, \
         {a: {p: 2}, b: {p: 2}}, {a: {p: 3}, b: {p: 5}}] AS row \
         WITH row.a AS a, row.b AS b \
         WITH a.p AS a, sum(b.p) AS s ORDER BY a + sum(b.p) \
         RETURN a, s",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(2), Value::Int(3)],
            vec![Value::Int(3), Value::Int(5)],
            vec![Value::Int(1), Value::Int(10)],
        ]
    );
}

/// The parked keys are what the top-k page reads, so a paged aggregating
/// projection sorts by the alias scope too: the two kept rows are the ones
/// with the smallest projected `a.p`, not the smallest incoming `a.p`.
#[test]
fn aggregate_order_by_alias_scope_survives_the_top_k_page() {
    let r = exec_cypher(
        &Graph::default(),
        "UNWIND [{a: {p: 1}, b: {p: 9}}, {a: {p: 2}, b: {p: 2}}, \
         {a: {p: 3}, b: {p: 1}}] AS row \
         WITH row.a AS a, row.b AS b \
         RETURN a.p AS k, sum(b.p) AS s ORDER BY a.p LIMIT 2",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(1), Value::Int(9)],
            vec![Value::Int(2), Value::Int(2)],
        ]
    );
}

/// An `ORDER BY` key is a sort key, not a result: one that cannot be
/// evaluated against the projected row sorts as null instead of failing the
/// statement. Here the key is a list comprehension over the incoming `a`
/// map, which the aggregation path must not propagate as an error (the
/// harness's `GroupByAcceptance` `[86]` shape: a list comprehension over a
/// non-grouping variable).
#[test]
fn aggregate_order_by_key_evaluation_failure_sorts_as_null() {
    let r = try_exec_cypher(
        &Graph::default(),
        "UNWIND [{a: {p: {p: 1}}, b: 20, c: 100}, \
         {a: {p: {p: 1}}, b: 10, c: 200}, \
         {a: {p: {p: 2}}, b: 20, c: 200}, \
         {a: {p: {p: 2}}, b: 10, c: 300}] AS _row \
         WITH _row.a AS a, _row.b AS b, _row.c AS c \
         RETURN a.p AS b, count(c) AS c GROUP BY b \
         ORDER BY [x IN a.p | x.p] DESC",
    )
    .expect("an ORDER BY key that cannot be evaluated sorts as null");
    assert_eq!(r.columns, vec!["b".to_string(), "c".to_string()]);
    assert_eq!(r.rows.len(), 2);
}

/// The `__` namespace is reserved against user aliases (OPLANE_REQ-00328162):
/// an `AS __ord0` alias cannot collide with the parked sort keys — the sink
/// declines, and the ordinary projection parks under a prefix that misses
/// every result column, so the user column keeps its real value.
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

/// The implicit-grouping resolution for a key that is not a verbatim
/// projection item: it resolves names in the projected row, so `a` is the
/// map the projection re-bound it to (`{p: b}`) and `a.p + 0` sorts by `b`
/// (5, 10, 20 → `k` 3, 2, 1). This is the same resolution the non-aggregating
/// sort and the TCK's `RETURN n.num AS n ORDER BY n + 2` pin; only a key that
/// is exactly an item expression answers from the item
/// (`ProjectionSubclauseDeprecation`).
#[test]
fn with_order_by_non_verbatim_key_resolves_in_the_projected_scope() {
    let r = exec_cypher(
        &Graph::default(),
        "UNWIND [{a: {p: 1}, b: 20}, {a: {p: 2}, b: 10}, {a: {p: 3}, b: 5}] AS row \
         WITH row.a AS a, row.b AS b \
         WITH a.p AS k, {p: b} AS a ORDER BY a.p + 0 \
         RETURN k",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(3)],
            vec![Value::Int(2)],
            vec![Value::Int(1)]
        ]
    );
}

/// A computed map alias the incoming row does not bind: `ORDER BY a.p` is not
/// a verbatim item, so it resolves in the projected row and the rows sort by
/// the projected map's `p`.
#[test]
fn with_order_by_computed_map_alias_reads_the_projected_map() {
    let r = exec_cypher(
        &Graph::default(),
        "UNWIND [{b: 20}, {b: 10}, {b: 5}] AS row \
         WITH row.b AS b \
         WITH {p: b} AS a ORDER BY a.p \
         RETURN a.p AS p",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(5)],
            vec![Value::Int(10)],
            vec![Value::Int(20)]
        ]
    );
}

/// `RETURN *` parks its sort keys like every other path and drops them
/// again: the expansion carries only the visible variable, in sorted order.
#[test]
fn return_star_orders_by_its_parked_keys_and_keeps_only_visible_columns() {
    let r = exec_cypher(
        &Graph::default(),
        "UNWIND [3, 1, 2] AS x RETURN * ORDER BY x DESC",
    );
    assert_eq!(r.columns, vec!["x".to_string()]);
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(3)],
            vec![Value::Int(2)],
            vec![Value::Int(1)]
        ]
    );
}

/// A verbatim aggregate item in `ORDER BY` sorts by the folded value on the
/// one-hop fold — the key is answered from its item, the same value the
/// projection emits. The graph's insertion order emits `id` 0 (count 2)
/// before `id` 1 (count 1), so ascending order is what proves the sort ran.
#[test]
fn fold_order_by_verbatim_aggregate_item_sorts_by_the_folded_value() {
    let built = build(
        &[&["A"], &["A"], &["A"]],
        &[(0, 1, "E"), (0, 2, "E"), (1, 2, "E")],
    );
    let r = exec_cypher(
        &built.graph,
        "MATCH (a:A)-[:E]->(b:A) RETURN a.id AS id, count(b) AS c ORDER BY count(b)",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(0), Value::Int(2)],
        ]
    );
}

/// A key the folded row cannot answer (an aggregate expression that is not
/// an item) declines the fold, and the grouping path answers it: the sort
/// is by the folded value plus zero, not by a null.
#[test]
fn fold_declines_a_computed_aggregate_order_key_for_the_grouping_path() {
    let built = build(
        &[&["A"], &["A"], &["A"]],
        &[(0, 1, "E"), (0, 2, "E"), (1, 2, "E")],
    );
    let r = exec_cypher(
        &built.graph,
        "MATCH (a:A)-[:E]->(b:A) RETURN a.id AS id, count(b) AS c \
         ORDER BY count(b) + 0",
    );
    assert_eq!(
        r.rows,
        vec![
            vec![Value::Int(1), Value::Int(1)],
            vec![Value::Int(0), Value::Int(2)],
        ]
    );
}
