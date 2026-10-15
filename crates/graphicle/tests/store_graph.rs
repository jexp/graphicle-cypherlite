//! Smoke tests for the CypherLite store core (`store_graph`): opening a
//! store applies the schema idempotently, the transaction helper commits and
//! rolls back, and UNWIND/SEARCH round-trips work through the façade helpers.
use graphicle_runtime::store_graph::{GraphStore, json_to_value, one, rows, s, value_to_json};
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn an_in_memory_store_applies_the_schema_idempotently() {
    let store = GraphStore::open_in_memory().unwrap();
    // Applying again (what a second open does) is a no-op, not an error.
    store.apply_schema().unwrap();
    store.ensure_vector_indexes(4).unwrap();
    store.ensure_vector_indexes(4).unwrap();
}

#[test]
fn a_directory_store_reopens_without_reapplying_anything() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph");
    {
        let store = GraphStore::open(&path).unwrap();
        store
            .transaction(|db| {
                db.query(
                    "CREATE (:Memory {id: 'm1', kind: 'note', title: 'First', body: 'rust graph'})",
                )?;
                Ok(())
            })
            .unwrap();
    }
    // Second open of the same store: schema application is a no-op and the
    // data is still there.
    let store = GraphStore::open(&path).unwrap();
    let row = store
        .read(|db| {
            one(
                db,
                "MATCH (m:Memory {id: 'm1'}) RETURN m.title AS title",
                BTreeMap::new(),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(s(&row, "title"), "First");
}

#[test]
fn unwind_merge_round_trip_and_graph_type_enforcement() {
    let store = GraphStore::open_in_memory().unwrap();
    let rows_param = json!([
        {"id": "c1", "path": "src/a.rs", "line": 1, "index_text": "rust borrow checker"},
        {"id": "c2", "path": "src/b.rs", "line": 1, "index_text": "python garbage collector"},
    ]);
    let mut params = BTreeMap::new();
    params.insert("rows".to_string(), json_to_value(&rows_param));
    store
        .transaction(|db| {
            db.query_with_params(
                "UNWIND $rows AS row MERGE (c:Chunk {id: row.id}) SET c += row",
                params.clone(),
            )?;
            Ok(())
        })
        .unwrap();
    let found = store
        .read(|db| {
            rows(
                db,
                "MATCH (c:Chunk) RETURN c.id AS id ORDER BY id",
                BTreeMap::new(),
            )
        })
        .unwrap();
    assert_eq!(json!(found), json!([{"id": "c1"}, {"id": "c2"}]));

    // The promoted graph type enforces managed labels: a Workspace without
    // its mandatory `name` is rejected, and the transaction rolls back.
    let result = store.transaction(|db| {
        db.query("CREATE (:Workspace {id: 'w1', capability_hash: 'h', created_at: 'now'})")?;
        Ok(())
    });
    assert!(result.is_err(), "mandatory property must be enforced");
    let count = store
        .read(|db| {
            one(
                db,
                "MATCH (w:Workspace) RETURN count(w) AS n",
                BTreeMap::new(),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(count["n"], json!(0), "the failed transaction rolled back");
}

#[test]
fn fulltext_search_uses_the_open_time_index() {
    let store = GraphStore::open_in_memory().unwrap();
    store
        .transaction(|db| {
            db.query("CREATE (:Chunk {id: 'c1', path: 'src/a.rs', index_text: 'rust borrow checker'})")?;
            db.query("CREATE (:Chunk {id: 'c2', path: 'src/b.rs', index_text: 'python garbage collector'})")?;
            Ok(())
        })
        .unwrap();
    let found = store
        .read(|db| {
            rows(
                db,
                "MATCH (c:Chunk) SEARCH c IN (FULLTEXT INDEX chunk_text FOR $q LIMIT 10) RETURN c.id AS id",
                BTreeMap::from([("q".to_string(), json_to_value(&json!("borrow")))]),
            )
        })
        .unwrap();
    assert_eq!(json!(found), json!([{"id": "c1"}]));
}

#[test]
fn vector_search_uses_the_lazily_created_index() {
    let store = GraphStore::open_in_memory().unwrap();
    store.ensure_vector_indexes(4).unwrap();
    store
        .transaction(|db| {
            db.query("CREATE (:Chunk {id: 'c1', embedding: [1.0, 0.0, 0.0, 0.0]})")?;
            db.query("CREATE (:Chunk {id: 'c2', embedding: [0.0, 1.0, 0.0, 0.0]})")?;
            Ok(())
        })
        .unwrap();
    let found = store
        .read(|db| {
            rows(
                db,
                "MATCH (c:Chunk) SEARCH c IN (VECTOR INDEX chunk_embed FOR $qv LIMIT 1) SCORE AS score RETURN c.id AS id, score",
                BTreeMap::from([("qv".to_string(), json_to_value(&json!([1.0, 0.0, 0.0, 0.0])))]),
            )
        })
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(s(&found[0], "id"), "c1");
}

#[test]
fn value_mapping_round_trips_json_maps_and_lists() {
    let value = json!({"a": [1, 2.5, "x", null, {"b": true}], "n": 7});
    assert_eq!(value_to_json(&json_to_value(&value)), value);
}
