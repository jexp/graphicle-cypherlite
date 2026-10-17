//! Optional Qdrant index. Only identities and vectors are projected; original text stays canonical.
//!
//! Passage embeddings themselves live in the graph store: a `Vector`-typed
//! `embedding` property (with `embedding_model_key` / `embedding_source_sha`)
//! on the `Memory` or `Chunk` node the passage came from (migration plan
//! §2.7; `docs/store-api.md`). The helpers below are the memory pipeline's
//! reads and writes of those properties; `am_vectors` is gone.
use crate::{
    Error, Result,
    jobs::{self, Job, Worker},
    model, query,
    store::{Store, repo, s},
    store_graph::{self, GraphStore},
    string,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// The `Memory` embedding of `identity` (`memory:<id>`) or the `Chunk`
/// embedding of a passage identity, as the `am_vectors` row shape the
/// callers of the SQL table had (`vector` decoded to a number list).
fn embedding_row(node: &Value, identity: String) -> Value {
    let vector = node["embedding"].clone();
    json!({"identity":identity,"source_sha":node["embedding_source_sha"],
        "model_key":node["embedding_model_key"],
        "dimensions":vector.as_array().map(|v| v.len() as i64).unwrap_or(0),
        "vector":vector})
}

/// Every embedding of one repository and model, as `am_vectors` rows
/// (`ORDER BY identity`).
pub fn graph_vectors(graph: &GraphStore, repository: &str, model_key: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    graph.read(|db| {
        for row in store_graph::rows(
            db,
            "MATCH (m:Memory {repository_id: $repo}) \
             WHERE m.embedding_model_key = $key AND m.embedding IS NOT NULL \
             RETURN m",
            store_graph::params(&json!({"repo": repository, "key": model_key})),
        )? {
            let node = store_graph::node(&row, "m");
            out.push(embedding_row(&node, format!("memory:{}", s(&node, "id"))));
        }
        for row in store_graph::rows(
            db,
            "MATCH (c:Chunk {repository_id: $repo}) \
             WHERE c.embedding_model_key = $key AND c.embedding IS NOT NULL \
             RETURN c",
            store_graph::params(&json!({"repo": repository, "key": model_key})),
        )? {
            let node = store_graph::node(&row, "c");
            let identity = s(&node, "id").to_string();
            out.push(embedding_row(&node, identity));
        }
        Ok(())
    })?;
    out.sort_by(|a, b| s(a, "identity").cmp(s(b, "identity")));
    Ok(out)
}

/// Every embedding of one repository, any model, as `am_vectors` rows
/// including `repository_id` (deletion's canonical store digest renders
/// them). `vector` stays a number list, as the SQL text decoded to.
pub fn graph_vectors_all(graph: &GraphStore, repository: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    graph.read(|db| {
        for (label, prefixed) in [("Memory", true), ("Chunk", false)] {
            for row in store_graph::rows(
                db,
                &format!(
                    "MATCH (n:{label} {{repository_id: $repo}}) \
                     WHERE n.embedding IS NOT NULL \
                     RETURN n.id AS id, n.embedding_source_sha AS sha, \
                     n.embedding_model_key AS model_key, n.embedding AS vector"
                ),
                store_graph::params(&json!({"repo": repository})),
            )? {
                let id = s(&row, "id");
                let vector = row["vector"].clone();
                out.push(json!({"repository_id":repository,
                    "identity":if prefixed { format!("memory:{id}") } else { id.to_string() },
                    "source_sha":row["sha"],"model_key":row["model_key"],
                    "dimensions":vector.as_array().map(|v| v.len() as i64).unwrap_or(0),
                    "vector":vector}));
            }
        }
        Ok(())
    })?;
    Ok(out)
}

/// The number of embeddings a repository holds (any model) — the `embed`
/// job's progress counter.
pub fn graph_vector_count(graph: &GraphStore, repository: &str) -> Result<u64> {
    let mut total = 0;
    graph.read(|db| {
        for label in ["Memory", "Chunk"] {
            let row = store_graph::one(
                db,
                &format!(
                    "MATCH (n:{label} {{repository_id: $repo}}) \
                     WHERE n.embedding IS NOT NULL RETURN count(n) AS count"
                ),
                store_graph::params(&json!({"repo": repository})),
            )?;
            total += row.and_then(|r| r["count"].as_u64()).unwrap_or(0);
        }
        Ok(())
    })?;
    Ok(total)
}

/// Store passage embeddings on the nodes they describe, creating the
/// store's vector indexes for the model's dimension on first use
/// (`GraphStore::ensure_vector_indexes`). `rows` is `(identity,
/// source_sha, vector)`; a `memory:<id>` identity lands on that Memory
/// node, anything else on the passage's Chunk node.
pub fn store_embeddings(
    graph: &GraphStore,
    repository: &str,
    model_key: &str,
    rows: &[(String, String, Vec<f64>)],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let dimensions = rows[0].2.len() as u32;
    if dimensions == 0 || rows.iter().any(|(_, _, v)| v.len() as u32 != dimensions) {
        return Err(Error::bad("Vector dimensions do not match"));
    }
    graph.ensure_vector_indexes(dimensions)?;
    let memories: Vec<Value> = rows
        .iter()
        .filter_map(|(identity, sha, vector)| {
            identity
                .strip_prefix("memory:")
                .map(|id| json!({"id":id,"sha":sha,"vec":vector}))
        })
        .collect();
    let chunks: Vec<Value> = rows
        .iter()
        .filter(|(identity, _, _)| !identity.starts_with("memory:"))
        .map(|(identity, sha, vector)| json!({"id":identity,"sha":sha,"vec":vector}))
        .collect();
    graph.transaction(|db| {
        if !memories.is_empty() {
            store_graph::rows(
                db,
                "UNWIND $rows AS row MERGE (m:Memory {id: row.id}) \
                 SET m.embedding = vector(row.vec, $dim, FLOAT64), \
                     m.embedding_model_key = $key, m.embedding_source_sha = row.sha \
                 RETURN count(m) AS n",
                store_graph::params(&json!({"rows":memories,"dim":dimensions,"key":model_key})),
            )?;
        }
        if !chunks.is_empty() {
            store_graph::rows(
                db,
                "UNWIND $rows AS row MERGE (c:Chunk {id: row.id}) \
                 SET c.repository_id = $repo, \
                     c.embedding = vector(row.vec, $dim, FLOAT64), \
                     c.embedding_model_key = $key, c.embedding_source_sha = row.sha \
                 RETURN count(c) AS n",
                store_graph::params(
                    &json!({"rows":chunks,"repo":repository,"dim":dimensions,"key":model_key}),
                ),
            )?;
        }
        Ok(())
    })
}

/// Drop the embeddings of `identities` (passage ids and `memory:<id>`s);
/// returns how many nodes carried one. Stub `Chunk` nodes exist only to
/// carry an embedding, so they go with it.
pub fn delete_embeddings(
    graph: &GraphStore,
    repository: &str,
    identities: &[String],
) -> Result<usize> {
    if identities.is_empty() {
        return Ok(0);
    }
    let memories: Vec<&str> = identities
        .iter()
        .filter_map(|i| i.strip_prefix("memory:"))
        .collect();
    let chunks: Vec<&str> = identities
        .iter()
        .filter(|i| !i.starts_with("memory:"))
        .map(String::as_str)
        .collect();
    graph.transaction(|db| {
        let mut removed = 0usize;
        if !memories.is_empty() {
            let row = store_graph::one(
                db,
                "UNWIND $ids AS id MATCH (m:Memory {id: id, repository_id: $repo}) \
                 WHERE m.embedding IS NOT NULL \
                 WITH count(m) AS n, collect(m) AS ms UNWIND ms AS m \
                 REMOVE m.embedding, m.embedding_model_key, m.embedding_source_sha \
                 RETURN n",
                store_graph::params(&json!({"ids":memories,"repo":repository})),
            )?;
            removed += row
                .and_then(|r| r["n"].as_u64().map(|n| n as usize))
                .unwrap_or(0);
        }
        if !chunks.is_empty() {
            let row = store_graph::one(
                db,
                "UNWIND $ids AS id MATCH (c:Chunk {id: id, repository_id: $repo}) \
                 WHERE c.embedding IS NOT NULL \
                 WITH collect(c) AS cs UNWIND cs AS c DETACH DELETE c \
                 RETURN size(cs) AS n",
                store_graph::params(&json!({"ids":chunks,"repo":repository})),
            )?;
            removed += row
                .and_then(|r| r["n"].as_u64().map(|n| n as usize))
                .unwrap_or(0);
        }
        Ok(removed)
    })
}

/// The `(identity, source_sha)` pairs of every embedding of one repository
/// and model, without the vector payloads — the freshness filter of the
/// dense arm and the embed job's reuse check.
pub fn graph_vector_index(
    graph: &GraphStore,
    repository: &str,
    model_key: &str,
) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    graph.read(|db| {
        for (label, prefixed) in [("Memory", true), ("Chunk", false)] {
            for row in store_graph::rows(
                db,
                &format!(
                    "MATCH (n:{label} {{repository_id: $repo}}) \
                     WHERE n.embedding_model_key = $key AND n.embedding IS NOT NULL \
                     RETURN n.id AS id, n.embedding_source_sha AS sha"
                ),
                store_graph::params(&json!({"repo": repository, "key": model_key})),
            )? {
                let id = s(&row, "id");
                out.push((
                    if prefixed {
                        format!("memory:{id}")
                    } else {
                        id.to_string()
                    },
                    s(&row, "sha").to_string(),
                ));
            }
        }
        Ok(())
    })?;
    Ok(out)
}

/// Every embedding identity of one repository, any model (`memory:<id>` for
/// Memory nodes, the passage id for Chunk stubs).
pub fn graph_vector_identities(graph: &GraphStore, repository: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    graph.read(|db| {
        for (label, prefixed) in [("Memory", true), ("Chunk", false)] {
            for row in store_graph::rows(
                db,
                &format!(
                    "MATCH (n:{label} {{repository_id: $repo}}) \
                     WHERE n.embedding IS NOT NULL RETURN n.id AS id"
                ),
                store_graph::params(&json!({"repo": repository})),
            )? {
                let id = s(&row, "id");
                out.push(if prefixed {
                    format!("memory:{id}")
                } else {
                    id.to_string()
                });
            }
        }
        Ok(())
    })?;
    Ok(out)
}

/// Drop every embedding of one repository (the derived-cache purge).
pub fn clear_embeddings(graph: &GraphStore, repository: &str) -> Result<()> {
    let identities = graph_vector_identities(graph, repository)?;
    delete_embeddings(graph, repository, &identities)?;
    Ok(())
}

/// The `limit` nearest embeddings of one repository and model to `query`,
/// as `(identity, source_sha, score)` rows, best first — the vector-index
/// counterpart of the retired load-all-and-cosine pass over `am_vectors`.
pub fn semantic_search(
    graph: &GraphStore,
    repository: &str,
    model_key: &str,
    query: &[f64],
    limit: usize,
) -> Result<Vec<(String, String, f64)>> {
    let mut pairs: Vec<(String, String, f64)> = Vec::new();
    graph.read(|db| {
        for (label, index, prefixed) in [
            ("Chunk", "chunk_embed", false),
            ("Memory", "mem_embed", true),
        ] {
            for row in store_graph::rows(
                db,
                &format!(
                    "MATCH (n:{label}) \
                     SEARCH n IN (VECTOR INDEX {index} FOR $qv LIMIT $k) SCORE AS score \
                     WHERE n.repository_id = $repo AND n.embedding_model_key = $key \
                     RETURN n.id AS id, n.embedding_source_sha AS sha, score"
                ),
                store_graph::params(
                    &json!({"qv":query,"k":limit as i64,"repo":repository,"key":model_key}),
                ),
            )? {
                let id = s(&row, "id");
                pairs.push((
                    if prefixed {
                        format!("memory:{id}")
                    } else {
                        id.to_string()
                    },
                    s(&row, "sha").to_string(),
                    row["score"].as_f64().unwrap_or(0.0),
                ));
            }
        }
        Ok(())
    })?;
    pairs.sort_by(|(a, _, x), (b, _, y)| y.total_cmp(x).then(a.cmp(b)));
    pairs.truncate(limit);
    Ok(pairs)
}
fn point(identity: &str) -> String {
    let h = graphicle_memory::text::sha256(identity.as_bytes());
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}
pub fn configure(store: &Store, workspace: &str, token: &str, args: &Value) -> Result<Value> {
    store.authorized(workspace, token, "admin", |db, p| {
        repo(db, p, string(args, "repository_id", 100)?)?;
        Ok(())
    })?;
    let endpoint = string(args, "endpoint", 2000)?;
    model::checked_url(endpoint)?;
    let reference = args["credential_ref"].as_str();
    if let Some(r) = reference {
        store.resolve_credential(workspace, r)?;
    }
    let collection = string(args, "collection", 100)?;
    if !collection
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Error::bad("Use an alphanumeric collection name"));
    }
    let config = json!({"endpoint":endpoint,"collection":collection,"credential_ref":reference});
    store.authorized(workspace,token,"admin",|db,p|{let repository=repo(db,p,string(args,"repository_id",100)?)?;if let Ok(old)=self::config(db,s(&repository,"id"))&& old!=config{crate::projection::enqueue_config(db,workspace,s(&repository,"id"),"qdrant",&old,"backend_reconfigured")?;}crate::projection::backend_set(db,s(&repository,"id"),"qdrant",&config)?;Ok(json!({"configured":true,"stored_remotely":"vectors and opaque source identities; no original text"}))})
}
async fn request(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    config: &Value,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    let mut request = client.request(
        method,
        format!(
            "{}/collections/{}{}",
            s(config, "endpoint").trim_end_matches('/'),
            s(config, "collection"),
            path
        ),
    );
    if let Some(reference) = config["credential_ref"].as_str() {
        request = request.header("api-key", store.resolve_credential(workspace, reference)?);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|_| Error::bad("Vector index request failed"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(json!({"missing":true}));
    }
    model::bounded_json(response).await
}
fn config(db: &rusqlite::Connection, repository: &str) -> Result<Value> {
    crate::projection::backend_config(db, repository, "qdrant")?
        .ok_or_else(|| Error::bad("Configure a Qdrant collection first"))
}
pub(crate) async fn clear_projection(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    config: &Value,
    scope: &str,
) -> Result<()> {
    request(
        store,
        client,
        workspace,
        config,
        reqwest::Method::POST,
        "/points/delete?wait=true",
        Some(json!({"filter":{"must":[{"key":"scope","match":{"value":scope}}]}})),
    )
    .await?;
    Ok(())
}
pub async fn publish(
    store: &Store,
    client: &reqwest::Client,
    workspace: &str,
    token: &str,
    args: &Value,
) -> Result<Value> {
    let (config, snapshot, vectors, revision) =
        store.authorized(workspace, token, "admin", |db, p| {
            let r = repo(db, p, string(args, "repository_id", 100)?)?;
            let snapshot = query::snapshot(db, &store.graph(), &r, p)?;
            let (candidates, _) = query::candidates(&snapshot)?;
            let valid = candidates
                .iter()
                .map(|c| (s(c, "identity"), s(c, "source_sha")))
                .collect::<BTreeMap<_, _>>();
            let mut vectors =
                graph_vectors(&store.graph(), s(&r, "id"), string(args, "model_key", 64)?)?;
            vectors.retain(|v| valid.get(s(v, "identity")) == Some(&s(v, "source_sha")));
            let cfg = config(db, s(&r, "id"))?;
            crate::projection::ready(db, s(&r, "id"), "qdrant", &cfg)?;
            let revision = crate::projection::marker(db, s(&r, "id"), "qdrant", &cfg)?;
            Ok((cfg, snapshot, vectors, revision))
        })?;
    if vectors.is_empty() {
        return Err(Error::bad(
            "Embed this source snapshot before publishing its vector index",
        ));
    }
    let size = vectors[0]["dimensions"].as_u64().unwrap_or(0);
    if size == 0 || vectors.iter().any(|v| v["dimensions"] != size) {
        return Err(Error::bad("Vector dimensions do not match"));
    }
    let scope = s(&snapshot.repository, "id");
    let mut touched = false;
    let result=async {
    let exists = request(
        store,
        client,
        workspace,
        &config,
        reqwest::Method::GET,
        "",
        None,
    )
    .await?;
    if exists["missing"] == true {
        touched=true;
        request(
            store,
            client,
            workspace,
            &config,
            reqwest::Method::PUT,
            "",
            Some(json!({"vectors":{"size":size,"distance":"Cosine"}})),
        )
        .await?;
    } else if exists["result"]["config"]["params"]["vectors"]["size"] != size {
        return Err(Error::bad(
            "Qdrant collection dimensions differ from the embedding model",
        ));
    }
    let generation = crate::id();
    let model_key = string(args, "model_key", 64)?;
    for batch in vectors.chunks(64) {
        store.authorized(workspace, token, "admin", |db, p| {
            let r = repo(db, p, scope)?;
            if query::snapshot(db, &store.graph(), &r, p)?.state != snapshot.state || crate::projection::marker(db, scope, "qdrant", &config)? != revision {
                return Err(Error::conflict("Sources changed during vector publication"));
            }
            Ok(())
        })?;
        let points=batch.iter().map(|v|json!({"id":point(&format!("{scope}:{model_key}:{}",s(v,"identity"))),"vector":v["vector"],"payload":{"scope":scope,"identity":v["identity"],"source_sha":v["source_sha"],"model_key":model_key,"generation":generation}})).collect::<Vec<_>>();
        touched=true;
        request(
            store,
            client,
            workspace,
            &config,
            reqwest::Method::PUT,
            "/points?wait=true",
            Some(json!({"points":points})),
        )
        .await?;
    }
    let current = store.authorized(workspace, token, "admin", |db, p| {
        let r = repo(db, p, scope)?;
        Ok(query::snapshot(db, &store.graph(), &r, p)?.state == snapshot.state && crate::projection::marker(db, scope, "qdrant", &config)? == revision)
    })?;
    if !current {
        return Err(Error::conflict(
            "Source snapshot changed; only current identities can be retrieved, and publication should be repeated",
        ));
    }
    request(store,client,workspace,&config,reqwest::Method::POST,"/points/delete?wait=true",Some(json!({"filter":{"must":[{"key":"scope","match":{"value":scope}},{"key":"model_key","match":{"value":model_key}}],"must_not":[{"key":"generation","match":{"value":generation}}]}}))).await?;
    Ok(
        json!({"published":vectors.len(),"dimensions":size,"model_key":model_key,"source_state":snapshot.state}),
    )
    }.await;
    if touched && result.is_err() {
        store.transaction(|db| {
            crate::projection::enqueue_config(
                db,
                workspace,
                scope,
                "qdrant",
                &config,
                "publication_incomplete",
            )
        })?;
    }
    result
}
pub async fn ranking(
    worker: &Worker,
    job: &Job,
    candidates: &[Value],
    vector: &[f64],
    model_key: &str,
) -> Result<Value> {
    if vector.is_empty() || vector.len() > 8192 || vector.iter().any(|n| !n.is_finite()) {
        return Err(Error::bad("Invalid search vector"));
    }
    let config = jobs::guarded_read(&worker.store, job, |db, _, _| config(db, &job.repository))?;
    let valid = candidates
        .iter()
        .map(|v| (s(v, "identity"), s(v, "source_sha")))
        .collect::<BTreeMap<_, _>>();
    let ids = candidates
        .iter()
        .map(|c| {
            point(&format!(
                "{}:{model_key}:{}",
                job.repository,
                s(c, "identity")
            ))
        })
        .collect::<BTreeSet<_>>();
    // Pre-filter on canonical IDs and check the source fingerprint again below.
    let result=request(&worker.store,&worker.client,&job.workspace,&config,reqwest::Method::POST,"/points/query",Some(json!({"query":vector,"limit":200,"with_payload":true,"filter":{"must":[{"has_id":ids},{"key":"scope","match":{"value":job.repository}},{"key":"model_key","match":{"value":model_key}}]}}))).await?;
    if result["missing"] == true {
        return Err(Error::bad(
            "Qdrant collection is unavailable; publish the index first",
        ));
    }
    jobs::guarded_read(&worker.store, job, |_, _, _| Ok(()))?;
    let ranked = result["result"]["points"]
        .as_array()
        .ok_or_else(|| Error::bad("Vector backend returned an invalid result"))?
        .iter()
        .filter(|p| {
            p["payload"]["scope"] == job.repository
                && p["payload"]["model_key"] == model_key
                && valid.get(s(&p["payload"], "identity")) == Some(&s(&p["payload"], "source_sha"))
        })
        .map(|p| p["payload"]["identity"].clone())
        .collect::<Vec<_>>();
    Ok(json!({"semantic":ranked}))
}
