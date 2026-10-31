//! RETURN shares entity-free payloads (LABS-156, ADR-0170).
//!
//! RETURN refreshes entity snapshots in every returned value. A list or map
//! that holds no node, relationship or path has nothing to refresh, so the
//! returned rows carry the row's shared payload instead of a per-row deep
//! copy. Containers that hold entities are still rebuilt with fresh
//! snapshots. A payload shared by many rows is walked once per RETURN, not
//! once per row.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cypherlite::{Database, Value};

fn rows(query: &str) -> Vec<Vec<Value>> {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    conn.query(query).unwrap().rows
}

/// Collects the column-0 payload of every row, expecting one variant.
fn payloads<'a, T>(
    rows: &'a [Vec<Value>],
    what: &str,
    pick: fn(&'a Value) -> Option<&'a Arc<T>>,
) -> Vec<&'a Arc<T>> {
    rows.iter()
        .map(|row| pick(&row[0]).unwrap_or_else(|| panic!("expected a {what}, got {:?}", row[0])))
        .collect()
}

/// Asserts every payload is the same shared allocation as the first.
fn assert_all_shared<T>(payloads: &[&Arc<T>], what: &str) {
    for p in &payloads[1..] {
        assert!(
            Arc::ptr_eq(payloads[0], p),
            "rows copied the {what} payload"
        );
    }
}

#[test]
fn returned_rows_share_entity_free_payloads() {
    let rows = rows(
        "UNWIND range(1, 20000) AS i WITH collect(i) AS ns \
         UNWIND range(1, 3) AS k RETURN ns",
    );
    assert_eq!(rows.len(), 3);
    let lists = payloads(&rows, "list", |v| match v {
        Value::List(xs) => Some(xs),
        _ => None,
    });
    assert_eq!(lists[0].len(), 20_000);
    assert_eq!(lists[0][19_999], Value::Int(20_000));
    assert_all_shared(&lists, "list");
}

#[test]
fn returned_large_list_rows_walk_payload_once() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let started = Instant::now();
    let rows = conn
        .query(
            "UNWIND range(1, 100000) AS i WITH collect(i) AS ns \
             UNWIND range(1, 20000) AS k RETURN ns",
        )
        .unwrap()
        .rows;
    let elapsed = started.elapsed();
    assert_eq!(rows.len(), 20_000);
    let lists = payloads(&rows, "list", |v| match v {
        Value::List(xs) => Some(xs),
        _ => None,
    });
    assert_eq!(lists[0].len(), 100_000);
    assert_all_shared(&lists, "list");
    assert!(
        elapsed < Duration::from_secs(10),
        "RETURN of a shared list over 20000 rows took {elapsed:?}"
    );
}

#[test]
fn returned_rows_share_entity_free_map_payloads() {
    let rows = rows(
        "UNWIND range(1, 20000) AS i WITH collect(i) AS ns \
         WITH {ns: ns, nested: [ns, {name: 'x'}], name: 'x'} AS m \
         UNWIND range(1, 3) AS k RETURN m",
    );
    assert_eq!(rows.len(), 3);
    let maps = payloads(&rows, "map", |v| match v {
        Value::Map(m) => Some(m),
        _ => None,
    });
    assert_eq!(maps[0]["name"], Value::String("x".into()));
    match &maps[0]["ns"] {
        Value::List(xs) => assert_eq!(xs.len(), 20_000),
        other => panic!("expected a list, got {other:?}"),
    }
    assert_all_shared(&maps, "map");
}

#[test]
fn returned_list_refreshes_node_set_later_in_statement() {
    let rows = rows(
        "CREATE (a:T {v: 1}) WITH collect(a) AS xs \
         UNWIND xs AS x SET x.v = 2 \
         WITH xs RETURN xs",
    );
    assert_eq!(rows.len(), 1);
    let Value::List(xs) = &rows[0][0] else {
        panic!("expected a list, got {:?}", rows[0][0]);
    };
    let [Value::Node(n)] = xs.as_slice() else {
        panic!("expected one node, got {xs:?}");
    };
    assert_eq!(n.properties.get("v"), Some(&Value::Int(2)));
}

#[test]
fn returned_map_refreshes_node_beside_scalar_payloads() {
    let rows = rows(
        "CREATE (a:T {v: 1}) \
         WITH {node: a, ns: [1, 2, 3], inner: {xs: [a]}} AS m, a \
         SET a.v = 2 RETURN m",
    );
    assert_eq!(rows.len(), 1);
    let Value::Map(m) = &rows[0][0] else {
        panic!("expected a map, got {:?}", rows[0][0]);
    };
    let Value::Node(n) = &m["node"] else {
        panic!("expected a node, got {:?}", m["node"]);
    };
    assert_eq!(n.properties.get("v"), Some(&Value::Int(2)));
    let Value::Map(inner) = &m["inner"] else {
        panic!("expected a map, got {:?}", m["inner"]);
    };
    let Value::List(xs) = &inner["xs"] else {
        panic!("expected a list, got {:?}", inner["xs"]);
    };
    let [Value::Node(n)] = xs.as_slice() else {
        panic!("expected one node, got {xs:?}");
    };
    assert_eq!(n.properties.get("v"), Some(&Value::Int(2)));
    assert_eq!(
        m["ns"],
        Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)].into())
    );
}
