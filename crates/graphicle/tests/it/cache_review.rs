//! The passage-embedding cache lives in the store's graph
//! (`:Embedding` nodes); a malformed entry is a miss, never a failure.

use graphicle_runtime::embedding_cache::Cache;
use graphicle_runtime::store_graph::{GraphStore, params, rows};
use serde_json::json;

#[test]
fn malformed_graph_values_are_cache_misses() {
    let graph = GraphStore::open_in_memory().unwrap();
    let cache = Cache::new(graph.clone(), "model");
    let text = "private source";
    // A vector stored as a string (or anything but a list of finite numbers)
    // is a miss, as a malformed blob was in the SQLite cache.
    graph
        .transaction(|db| {
            rows(
                db,
                "CREATE (e:Embedding {model_key: 'passage:model', text_sha256: $sha, vector: 'not a vector'})",
                params(&json!({"sha": graphicle_memory::text::sha256(text.as_bytes())})),
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(cache.get(text).unwrap(), None);
    // A mixed-type list is not even storable: the engine rejects it.
    assert!(
        graph
            .transaction(|db| {
                rows(
                    db,
                    "CREATE (e:Embedding {model_key: 'passage:model', text_sha256: $sha, vector: [1.0, 'x']})",
                    params(&json!({"sha": graphicle_memory::text::sha256(b"other")})),
                )?;
                Ok(())
            })
            .is_err()
    );
    assert_eq!(cache.get("other").unwrap(), None);
}

#[test]
fn entries_are_scoped_by_model_key_and_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("graph");
    {
        let graph = GraphStore::open(&path).unwrap();
        Cache::new(graph, "model-a")
            .put([("passage", [1.0, 2.0].as_slice())])
            .unwrap();
    }
    let graph = GraphStore::open(&path).unwrap();
    assert_eq!(
        Cache::new(graph.clone(), "model-a").get("passage").unwrap(),
        Some(vec![1.0, 2.0])
    );
    assert_eq!(Cache::new(graph, "model-b").get("passage").unwrap(), None);
}
