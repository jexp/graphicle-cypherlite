//! Passage-embedding cache, folded into the store's graph
//! ([`crate::store_graph::GraphStore`]).
//!
//! Maps `(model key, SHA-256 of the embedded text)` to the vector the model
//! returned, as `(:Embedding {model_key, text_sha256, vector})` nodes with
//! the vector as a list of floats. A job that embeds a passage this store
//! already embedded with the same model reuses the vector instead of calling
//! the model again. Vectors round-trip as IEEE-754 doubles, so a hit is
//! bit-identical to the model's answer.
//!
//! The key is [`crate::model::key`], which covers the provider, the model and,
//! for local models, the manifest fingerprint, so a changed model or prefix
//! never matches an old entry. Remote providers are assumed to serve fixed
//! weights for a model name.
//!
//! Migration note: this used to be an opt-in second SQLite file named by
//! `GRAPHICLE_EMBEDDING_CACHE`, shared between stores. That file and its
//! configuration are gone; the cache lives in the store's own graph now.
use crate::Result;
use crate::store_graph::{GraphStore, one, params};
use serde_json::json;

pub struct Cache {
    graph: GraphStore,
    namespace: String,
}

impl Cache {
    /// The cache of `graph` for passages embedded under `model_key`.
    pub fn new(graph: GraphStore, model_key: &str) -> Self {
        Self {
            graph,
            namespace: format!("passage:{model_key}"),
        }
    }

    /// The cached vector for `text`, if any. A malformed entry is a miss.
    pub fn get(&self, text: &str) -> Result<Option<Vec<f64>>> {
        let found = self.graph.read(|db| {
            one(
                db,
                "MATCH (e:Embedding {model_key: $model, text_sha256: $sha}) RETURN e.vector AS vector",
                params(&json!({"model": self.namespace, "sha": sha(text)})),
            )
        })?;
        Ok(found.as_ref().and_then(|row| vector(&row["vector"])))
    }

    /// Record vectors for texts; an existing entry is kept.
    pub fn put<'a>(&self, entries: impl IntoIterator<Item = (&'a str, &'a [f64])>) -> Result<()> {
        let rows: Vec<serde_json::Value> = entries
            .into_iter()
            .filter(|(_, vector)| !vector.is_empty() && vector.iter().all(|x| x.is_finite()))
            .map(|(text, vector)| json!({"sha": sha(text), "vector": vector}))
            .collect();
        if rows.is_empty() {
            return Ok(());
        }
        self.graph.transaction(|db| {
            crate::store_graph::rows(
                db,
                "UNWIND $rows AS row \
                 MERGE (e:Embedding {model_key: $model, text_sha256: row.sha}) \
                 ON CREATE SET e.vector = row.vector",
                params(&json!({"rows": rows, "model": self.namespace})),
            )?;
            Ok(())
        })
    }
}

fn sha(text: &str) -> String {
    graphicle_memory::text::sha256(text.as_bytes())
}

/// A stored vector as floats, or `None` when the entry is malformed (not a
/// list of finite numbers).
fn vector(value: &serde_json::Value) -> Option<Vec<f64>> {
    let vector: Vec<f64> = value
        .as_array()?
        .iter()
        .map(|x| x.as_f64())
        .collect::<Option<_>>()?;
    (!vector.is_empty() && vector.iter().all(|x| x.is_finite())).then_some(vector)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_exact_vectors_per_model_key() {
        let graph = GraphStore::open_in_memory().unwrap();
        let cache = Cache::new(graph.clone(), "model-a");
        let vector = vec![0.1, -2.5e-300, 1.0 / 3.0];
        assert_eq!(cache.get("title\ntext").unwrap(), None);
        cache.put([("title\ntext", vector.as_slice())]).unwrap();
        assert_eq!(cache.get("title\ntext").unwrap(), Some(vector.clone()));
        assert_eq!(cache.get("title\nother").unwrap(), None);
        // Another model key never sees the entry, and an existing entry is
        // not overwritten.
        let other = Cache::new(graph.clone(), "model-b");
        assert_eq!(other.get("title\ntext").unwrap(), None);
        let again = Cache::new(graph, "model-a");
        again.put([("title\ntext", [9.0].as_slice())]).unwrap();
        assert_eq!(again.get("title\ntext").unwrap(), Some(vector));
    }

    #[test]
    fn skips_non_finite_and_rejects_malformed_entries() {
        let graph = GraphStore::open_in_memory().unwrap();
        let cache = Cache::new(graph.clone(), "m");
        cache
            .put([("nan", [f64::NAN].as_slice()), ("empty", [].as_slice())])
            .unwrap();
        assert_eq!(cache.get("nan").unwrap(), None);
        assert_eq!(cache.get("empty").unwrap(), None);
        // An entry whose vector is not a list of numbers is a miss.
        graph
            .transaction(|db| {
                crate::store_graph::rows(
                    db,
                    "CREATE (e:Embedding {model_key: 'passage:m', text_sha256: $sha, vector: 'not a vector'})",
                    params(&json!({"sha": sha("bad")})),
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(cache.get("bad").unwrap(), None);
        assert_eq!(vector(&json!([])), None);
        assert_eq!(vector(&json!([1.0, "x"])), None);
        assert_eq!(vector(&json!([1.0, 2.0])), Some(vec![1.0, 2.0]));
    }
}
