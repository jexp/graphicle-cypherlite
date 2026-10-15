//! Dependency index and staleness markers for derived artifacts.
//!
//! Producers of derived data (chunks, embeddings, graph nodes, summaries, cached
//! answers) record what they were derived from with [`record_dependency`].  When a
//! source changes, [`invalidate_source`] walks the dependency graph transitively and
//! marks every dependent stale.  Readers report the workspace-wide [`Freshness`] so
//! callers can see when an answer may rest on outdated artifacts.
//!
//! **Graph model** (CypherLite; migration plan §2.5). A dependency is
//! `(:Artifact {workspace, kind, id})-[:DEPENDS_ON {recorded_at}]->(:Artifact)`
//! from the dependent to its source (both endpoints are artifacts; a source
//! is simply an artifact nothing recorded as derived). The invalidation walk
//! is one variable-length `MATCH` (`[:DEPENDS_ON*1..]`) instead of the old
//! covering-index scans of `gf_dependencies`.
//!
//! Staleness is a **`:Stale` marker label** on the artifact node with
//! `stale_reason`/`stale_since` properties (chosen over plain properties: the
//! label is the cheapest filter for the freshness scans, and clearing removes
//! label and properties in one `REMOVE`, so a stale marker can never leave
//! its properties behind). `gf_dependencies`/`gf_stale` in `schema.sql` are
//! superseded; they stay declared for stores that still carry rows.
use crate::store_graph::{GraphStore, Params, json_to_value, one, rows, s};
use crate::{Result, now};
use cypherlite::Connection as GraphConnection;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

/// Artifact kinds used as `kind` on `:Artifact` nodes.
pub const FILE: &str = "file";
pub const CHUNK: &str = "chunk";
pub const EMBEDDING: &str = "embedding";
pub const GRAPH_NODE: &str = "graph_node";
pub const GRAPH_EDGE: &str = "graph_edge";
pub const COMMUNITY: &str = "community";
pub const SUMMARY: &str = "summary";
pub const REPORT: &str = "report";
pub const DOCUMENT: &str = "document";
pub const MEMORY: &str = "memory";
pub const ANSWER: &str = "answer";
pub const PRINCIPAL: &str = "principal";
pub const SOURCE: &str = "source";
/// One anchored document chunk of the semantic overlay (`gf_anchor_edges`).
pub const ANCHOR: &str = "anchor";

/// Upper bound on the number of artifacts marked in one transitive walk.
const WALK_LIMIT: usize = 100_000;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Freshness {
    pub stale_count: u64,
    pub by_kind: BTreeMap<String, u64>,
    pub oldest_since: Option<String>,
}

impl Freshness {
    pub fn is_fresh(&self) -> bool {
        self.stale_count == 0
    }
}

/// Build the composite identifier used for a repository-scoped file artifact.
pub fn file_id(repository: &str, path: &str) -> String {
    format!("{repository}:{path}")
}
/// Build the composite identifier used for a repository-scoped graph node.
pub fn node_id(repository: &str, node: &str) -> String {
    format!("{repository}:{node}")
}
/// Build the composite identifier of one anchored chunk ([`ANCHOR`]).
pub fn anchor_id(repository: &str, chunk: &str) -> String {
    format!("{repository}:{chunk}")
}
/// Build the composite identifier used for a repository-scoped chunk.
pub fn chunk_id(repository: &str, path: &str, line: i64) -> String {
    format!("{repository}:{path}:{line}")
}

fn key_params(workspace: &str, kind: &str, id: &str) -> Params {
    Params::from([
        ("w".to_string(), json_to_value(&json!(workspace))),
        ("k".to_string(), json_to_value(&json!(kind))),
        ("id".to_string(), json_to_value(&json!(id))),
    ])
}

fn record_in(
    db: &mut GraphConnection,
    workspace: &str,
    dependent: (&str, &str),
    source: (&str, &str),
    stamp: &str,
) -> Result<()> {
    if dependent == source {
        return Ok(());
    }
    let mut params = key_params(workspace, dependent.0, dependent.1);
    params.insert("sk".to_string(), json_to_value(&json!(source.0)));
    params.insert("si".to_string(), json_to_value(&json!(source.1)));
    params.insert("now".to_string(), json_to_value(&json!(stamp)));
    rows(
        db,
        "MERGE (d:Artifact {workspace: $w, kind: $k, id: $id}) \
         MERGE (s:Artifact {workspace: $w, kind: $sk, id: $si}) \
         MERGE (d)-[r:DEPENDS_ON]->(s) ON CREATE SET r.recorded_at = $now \
         RETURN count(r) AS n",
        params,
    )?;
    Ok(())
}

fn forget_in(
    db: &mut GraphConnection,
    workspace: &str,
    dependent: (&str, &str),
) -> Result<()> {
    rows(
        db,
        "MATCH (d:Artifact {workspace: $w, kind: $k, id: $id})-[r:DEPENDS_ON]->() DELETE r",
        key_params(workspace, dependent.0, dependent.1),
    )?;
    Ok(())
}

fn mark_in(
    db: &mut GraphConnection,
    workspace: &str,
    kind: &str,
    id: &str,
    reason: &str,
    stamp: &str,
) -> Result<()> {
    let mut params = key_params(workspace, kind, id);
    params.insert("reason".to_string(), json_to_value(&json!(reason)));
    params.insert("now".to_string(), json_to_value(&json!(stamp)));
    rows(
        db,
        "MERGE (a:Artifact {workspace: $w, kind: $k, id: $id}) \
         SET a:Stale, a.stale_reason = $reason, a.stale_since = coalesce(a.stale_since, $now)",
        params,
    )?;
    Ok(())
}

fn clear_in(
    db: &mut GraphConnection,
    workspace: &str,
    kind: &str,
    id: &str,
) -> Result<bool> {
    let found = one(
        db,
        "MATCH (a:Artifact:Stale {workspace: $w, kind: $k, id: $id}) \
         REMOVE a:Stale, a.stale_reason, a.stale_since \
         RETURN count(a) AS n",
        key_params(workspace, kind, id),
    )?;
    Ok(found.is_some_and(|row| row["n"].as_u64().unwrap_or(0) > 0))
}

/// Record that `dependent` was derived from `source`.  Idempotent.
pub fn record_dependency(
    graph: &GraphStore,
    workspace: &str,
    dependent: (&str, &str),
    source: (&str, &str),
) -> Result<()> {
    if dependent == source {
        return Ok(());
    }
    let stamp = now();
    graph.transaction(|db| record_in(db, workspace, dependent, source, &stamp))
}

/// Remove every dependency edge recorded for `dependent` (before re-recording).
pub fn forget_dependent(graph: &GraphStore, workspace: &str, dependent: (&str, &str)) -> Result<()> {
    graph.transaction(|db| forget_in(db, workspace, dependent))
}

/// Mark a single artifact stale without walking dependents.
pub fn mark_stale(
    graph: &GraphStore,
    workspace: &str,
    kind: &str,
    id: &str,
    reason: &str,
) -> Result<()> {
    let stamp = now();
    graph.transaction(|db| mark_in(db, workspace, kind, id, reason, &stamp))
}

/// Mark every artifact that (transitively) depends on `source` as stale.
///
/// The source itself is not marked; returns the `(kind, id)` pairs marked,
/// in the order the traversal returns them, without duplicates.
pub fn invalidate_source(
    graph: &GraphStore,
    workspace: &str,
    source: (&str, &str),
    reason: &str,
) -> Result<Vec<(String, String)>> {
    let mut params = key_params(workspace, source.0, source.1);
    params.insert("reason".to_string(), json_to_value(&json!(reason)));
    params.insert("now".to_string(), json_to_value(&json!(now())));
    graph.transaction(|db| {
        let marked = rows(
            db,
            &format!(
                "MATCH (s:Artifact {{workspace: $w, kind: $k, id: $id}}) \
                 MATCH (d:Artifact)-[:DEPENDS_ON*1..]->(s) \
                 WHERE NOT (d.kind = $k AND d.id = $id) \
                 WITH DISTINCT d \
                 LIMIT {WALK_LIMIT} \
                 SET d:Stale, d.stale_reason = $reason, \
                     d.stale_since = coalesce(d.stale_since, $now) \
                 RETURN d.kind AS kind, d.id AS id"
            ),
            params.clone(),
        )?;
        Ok(marked
            .iter()
            .map(|row| (s(row, "kind").to_string(), s(row, "id").to_string()))
            .collect())
    })
}

pub fn is_stale(graph: &GraphStore, workspace: &str, kind: &str, id: &str) -> Result<bool> {
    Ok(graph
        .read(|db| {
            one(
                db,
                "MATCH (a:Artifact:Stale {workspace: $w, kind: $k, id: $id}) RETURN a.id AS id LIMIT 1",
                key_params(workspace, kind, id),
            )
        })?
        .is_some())
}

/// Clear the stale marker for an artifact once it has been recomputed.
pub fn clear_stale(graph: &GraphStore, workspace: &str, kind: &str, id: &str) -> Result<bool> {
    graph.transaction(|db| clear_in(db, workspace, kind, id))
}

/// Clear every stale marker of one kind whose id starts with `prefix`
/// (e.g. every graph node of a repository after a fresh projection).
pub fn clear_stale_prefix(
    graph: &GraphStore,
    workspace: &str,
    kind: &str,
    prefix: &str,
) -> Result<usize> {
    let mut params = key_params(workspace, kind, "");
    params.insert("prefix".to_string(), json_to_value(&json!(prefix)));
    graph.transaction(|db| {
        let found = one(
            db,
            "MATCH (a:Artifact:Stale {workspace: $w, kind: $k}) \
             WHERE a.id STARTS WITH $prefix \
             REMOVE a:Stale, a.stale_reason, a.stale_since \
             RETURN count(a) AS n",
            params.clone(),
        )?;
        Ok(found
            .and_then(|row| row["n"].as_u64())
            .unwrap_or(0) as usize)
    })
}

fn freshness_of(rows: &[serde_json::Value]) -> Freshness {
    let mut freshness = Freshness::default();
    for row in rows {
        let count = row["n"].as_u64().unwrap_or(0);
        if count == 0 {
            continue;
        }
        let kind = s(row, "kind").to_string();
        let since = s(row, "since").to_string();
        freshness.stale_count += count;
        freshness.by_kind.insert(kind, count);
        match &freshness.oldest_since {
            Some(oldest) if *oldest <= since => {}
            _ => freshness.oldest_since = Some(since),
        }
    }
    freshness
}

/// Workspace-wide staleness summary.
pub fn freshness(graph: &GraphStore, workspace: &str) -> Result<Freshness> {
    let found = graph.read(|db| {
        rows(
            db,
            "MATCH (a:Artifact:Stale {workspace: $w}) \
             RETURN a.kind AS kind, count(a) AS n, min(a.stale_since) AS since",
            key_params(workspace, "", ""),
        )
    })?;
    Ok(freshness_of(&found))
}

/// Freshness restricted to artifacts belonging to one repository (ids prefixed
/// with `repository:`), plus workspace-level artifacts (answers, memories).
pub fn repository_freshness(
    graph: &GraphStore,
    workspace: &str,
    repository: &str,
) -> Result<Freshness> {
    let mut params = key_params(workspace, "", "");
    params.insert(
        "prefix".to_string(),
        json_to_value(&json!(format!("{repository}:"))),
    );
    params.insert(
        "kinds".to_string(),
        json_to_value(&json!([ANSWER, MEMORY, PRINCIPAL])),
    );
    let found = graph.read(|db| {
        rows(
            db,
            "MATCH (a:Artifact:Stale {workspace: $w}) \
             WHERE a.id STARTS WITH $prefix OR a.kind IN $kinds \
             RETURN a.kind AS kind, count(a) AS n, min(a.stale_since) AS since",
            params.clone(),
        )
    })?;
    Ok(freshness_of(&found))
}

/// One dependency edge of a workspace, as [`dependencies`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub dependent_kind: String,
    pub dependent_id: String,
    pub source_kind: String,
    pub source_id: String,
}

/// Every dependency edge of a workspace (the deletion walk filters them by
/// scope; the replacement for scanning `gf_dependencies`).
pub fn dependencies(graph: &GraphStore, workspace: &str) -> Result<Vec<Dependency>> {
    let found = graph.read(|db| {
        rows(
            db,
            "MATCH (d:Artifact {workspace: $w})-[:DEPENDS_ON]->(s:Artifact) \
             RETURN d.kind AS dependent_kind, d.id AS dependent_id, \
                    s.kind AS source_kind, s.id AS source_id",
            key_params(workspace, "", ""),
        )
    })?;
    Ok(found
        .iter()
        .map(|row| Dependency {
            dependent_kind: s(row, "dependent_kind").to_string(),
            dependent_id: s(row, "dependent_id").to_string(),
            source_kind: s(row, "source_kind").to_string(),
            source_id: s(row, "source_id").to_string(),
        })
        .collect())
}

/// Remove the given dependency edges; returns how many were removed (the
/// deletion path's replacement for deleting `gf_dependencies` rows).
pub fn forget_dependencies(
    graph: &GraphStore,
    workspace: &str,
    edges: &[Dependency],
) -> Result<usize> {
    if edges.is_empty() {
        return Ok(0);
    }
    let batch: Vec<serde_json::Value> = edges
        .iter()
        .map(|e| {
            json!({"dk":e.dependent_kind,"di":e.dependent_id,"sk":e.source_kind,"si":e.source_id})
        })
        .collect();
    let mut params = key_params(workspace, "", "");
    params.insert("rows".to_string(), json_to_value(&json!(batch)));
    graph.transaction(|db| {
        let found = one(
            db,
            "UNWIND $rows AS row \
             MATCH (d:Artifact {workspace: $w})-[r:DEPENDS_ON]->(s:Artifact) \
             WHERE d.kind = row.dk AND d.id = row.di AND s.kind = row.sk AND s.id = row.si \
             DELETE r \
             RETURN count(*) AS n",
            params.clone(),
        )?;
        Ok(found.and_then(|row| row["n"].as_u64()).unwrap_or(0) as usize)
    })
}

/// Every stale marker of a workspace as `(kind, id)` (the deletion walk
/// filters them by scope; the replacement for scanning `gf_stale`).
pub fn stale_markers(graph: &GraphStore, workspace: &str) -> Result<Vec<(String, String)>> {
    let found = graph.read(|db| {
        rows(
            db,
            "MATCH (a:Artifact:Stale {workspace: $w}) RETURN a.kind AS kind, a.id AS id",
            key_params(workspace, "", ""),
        )
    })?;
    Ok(found
        .iter()
        .map(|row| (s(row, "kind").to_string(), s(row, "id").to_string()))
        .collect())
}

/// Remove the given stale markers; returns how many were removed.
pub fn clear_stale_markers(
    graph: &GraphStore,
    workspace: &str,
    markers: &[(String, String)],
) -> Result<usize> {
    if markers.is_empty() {
        return Ok(0);
    }
    let batch: Vec<serde_json::Value> = markers
        .iter()
        .map(|(kind, id)| json!({"kind":kind,"id":id}))
        .collect();
    let mut params = key_params(workspace, "", "");
    params.insert("rows".to_string(), json_to_value(&json!(batch)));
    graph.transaction(|db| {
        let found = one(
            db,
            "UNWIND $rows AS row \
             MATCH (a:Artifact:Stale {workspace: $w}) \
             WHERE a.kind = row.kind AND a.id = row.id \
             REMOVE a:Stale, a.stale_reason, a.stale_since \
             RETURN count(*) AS n",
            params.clone(),
        )?;
        Ok(found.and_then(|row| row["n"].as_u64()).unwrap_or(0) as usize)
    })
}

/// Dependent ids of one kind, prefixed by `id_prefix`, with an edge recorded
/// at or after `since` (deletion's "computed while visible" sweep).
pub fn dependents_recorded_since(
    graph: &GraphStore,
    workspace: &str,
    kind: &str,
    id_prefix: &str,
    since: &str,
) -> Result<Vec<String>> {
    let mut params = key_params(workspace, kind, "");
    params.insert("prefix".to_string(), json_to_value(&json!(id_prefix)));
    params.insert("since".to_string(), json_to_value(&json!(since)));
    let found = graph.read(|db| {
        rows(
            db,
            "MATCH (d:Artifact {workspace: $w, kind: $k})-[r:DEPENDS_ON]->() \
             WHERE d.id STARTS WITH $prefix AND r.recorded_at >= $since \
             RETURN DISTINCT d.id AS id",
            params.clone(),
        )
    })?;
    Ok(found.iter().map(|row| s(row, "id").to_string()).collect())
}

/// Remove every dependency edge of the workspace that touches an artifact id
/// starting with `prefix` (a deleted project's artifacts).
pub fn forget_id_prefix(graph: &GraphStore, workspace: &str, prefix: &str) -> Result<usize> {
    let mut params = key_params(workspace, "", "");
    params.insert("prefix".to_string(), json_to_value(&json!(prefix)));
    graph.transaction(|db| {
        let found = one(
            db,
            "MATCH (a:Artifact {workspace: $w})-[r:DEPENDS_ON]-(b:Artifact) \
             WHERE a.id STARTS WITH $prefix OR b.id STARTS WITH $prefix \
             WITH DISTINCT r \
             DELETE r \
             RETURN count(*) AS n",
            params.clone(),
        )?;
        Ok(found.and_then(|row| row["n"].as_u64()).unwrap_or(0) as usize)
    })
}

/// Clear every stale marker of the workspace whose id starts with `prefix`,
/// of any kind (a deleted project's artifacts).
pub fn clear_stale_id_prefix(graph: &GraphStore, workspace: &str, prefix: &str) -> Result<usize> {
    let mut params = key_params(workspace, "", "");
    params.insert("prefix".to_string(), json_to_value(&json!(prefix)));
    graph.transaction(|db| {
        let found = one(
            db,
            "MATCH (a:Artifact:Stale {workspace: $w}) \
             WHERE a.id STARTS WITH $prefix \
             REMOVE a:Stale, a.stale_reason, a.stale_since \
             RETURN count(a) AS n",
            params.clone(),
        )?;
        Ok(found.and_then(|row| row["n"].as_u64()).unwrap_or(0) as usize)
    })
}

/// Map an evidence document id (`file:<path>`, `connector:<id>:<doc>`, or a
/// workspace-source link id) to the `(kind, id)` source it derives from.
pub fn origin_source(repository: &str, origin: &str) -> (&'static str, String) {
    match origin.strip_prefix("file:") {
        Some(path) => (FILE, file_id(repository, path)),
        None => (DOCUMENT, origin.to_string()),
    }
}

/// Workspace that owns a repository; empty when the repository is unknown.
/// (`repo_projects` is still a SQL table until the ownership spine migrates.)
pub fn workspace_of(db: &Connection, repository: &str) -> Result<String> {
    Ok(db
        .query_row(
            "SELECT workspace_id FROM repo_projects WHERE id=?",
            [repository],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_default())
}

/// Connector document identifier as it appears in evidence documents.
pub fn connector_document_id(connector: &str, document: &str) -> String {
    format!("connector:{connector}:{document}")
}

/// Record a memory's dependencies from its saved `data` (repository file
/// paths under `dependencies`, workspace links under `evidence_links`,
/// connector documents under `native_sources`) and clear its stale marker.
pub fn record_memory(
    graph: &GraphStore,
    workspace: &str,
    repository: &str,
    memory: &str,
    data: &serde_json::Value,
) -> Result<()> {
    let stamp = now();
    graph.transaction(|db| {
        let dependent = (MEMORY, memory);
        forget_in(db, workspace, dependent)?;
        for path in data["dependencies"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, _)| k)
        {
            record_in(db, workspace, dependent, (FILE, &file_id(repository, path)), &stamp)?;
        }
        for key in ["evidence_links", "native_sources"] {
            for origin in data[key].as_object().into_iter().flatten().map(|(k, _)| k) {
                let (kind, id) = origin_source(repository, origin);
                record_in(db, workspace, dependent, (kind, &id), &stamp)?;
            }
        }
        clear_in(db, workspace, MEMORY, memory)?;
        Ok(())
    })
}

/// Record that a derived artifact depends on each evidence document it used
/// (by document id as produced by `memory::documents`).
pub fn record_document_dependencies<'a>(
    graph: &GraphStore,
    workspace: &str,
    repository: &str,
    dependent: (&str, &str),
    origins: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let stamp = now();
    let edges: Vec<(&'static str, String)> = origins
        .into_iter()
        .map(|origin| origin_source(repository, origin))
        .collect();
    graph.transaction(|db| {
        for (kind, id) in &edges {
            record_in(db, workspace, dependent, (kind, id), &stamp)?;
        }
        Ok(())
    })
}

/// Record `graph_node → file` dependencies for every live node of a freshly
/// persisted episode and clear their stale markers: the projection is the
/// recomputation. Nodes whose `relpath` attribute is empty are skipped.
/// (`gf_nodes` is still a SQL table until the code graph migrates.)
pub fn record_graph_episode(
    db: &Connection,
    graph: &GraphStore,
    workspace: &str,
    repository: &str,
) -> Result<usize> {
    let mut statement = db.prepare(
        "SELECT node_id,attributes FROM gf_nodes WHERE repository_id=? AND valid_to_episode IS NULL",
    )?;
    let nodes = statement
        .query_map([repository], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let stamp = now();
    graph.transaction(|g| {
        clear_prefix_in(g, workspace, GRAPH_NODE, &format!("{repository}:"))?;
        let mut recorded = 0;
        for (node, attributes) in &nodes {
            let attributes: serde_json::Value =
                serde_json::from_str(attributes).unwrap_or_default();
            let Some(relpath) = attributes["relpath"].as_str().filter(|p| !p.is_empty()) else {
                continue;
            };
            let dependent = node_id(repository, node);
            forget_in(g, workspace, (GRAPH_NODE, &dependent))?;
            record_in(
                g,
                workspace,
                (GRAPH_NODE, &dependent),
                (FILE, &file_id(repository, relpath)),
                &stamp,
            )?;
            recorded += 1;
        }
        Ok(recorded)
    })
}

fn clear_prefix_in(
    db: &mut GraphConnection,
    workspace: &str,
    kind: &str,
    prefix: &str,
) -> Result<usize> {
    let mut params = key_params(workspace, kind, "");
    params.insert("prefix".to_string(), json_to_value(&json!(prefix)));
    let found = one(
        db,
        "MATCH (a:Artifact:Stale {workspace: $w, kind: $k}) \
         WHERE a.id STARTS WITH $prefix \
         REMOVE a:Stale, a.stale_reason, a.stale_since \
         RETURN count(a) AS n",
        params,
    )?;
    Ok(found.and_then(|row| row["n"].as_u64()).unwrap_or(0) as usize)
}

/// Incremental form of [`record_graph_episode`] for a persisted episode diff:
/// `inserted` are the `(node_id, relpath)` pairs of the nodes the episode
/// inserted, `closed` the node ids it closed, `unchanged_live` whether any
/// live node was carried over. Unchanged live nodes keep the dependency edges
/// recorded when they were inserted, so the cost follows the diff rather than
/// the repository. Stores whose live nodes predate the dependency index are
/// backfilled once through the full pass.
pub fn record_graph_nodes<'a>(
    db: &Connection,
    graph: &GraphStore,
    workspace: &str,
    repository: &str,
    inserted: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
    closed: impl IntoIterator<Item = &'a str>,
    unchanged_live: bool,
) -> Result<usize> {
    let prefix = format!("{repository}:");
    let inserted: Vec<(&str, Option<&str>)> = inserted.into_iter().collect();
    let closed: Vec<&str> = closed.into_iter().collect();
    if unchanged_live {
        let mut params = key_params(workspace, GRAPH_NODE, "");
        params.insert("prefix".to_string(), json_to_value(&json!(prefix)));
        let indexed = graph.read(|g| {
            one(
                g,
                "MATCH (d:Artifact {workspace: $w, kind: $k})-[:DEPENDS_ON]->() \
                 WHERE d.id STARTS WITH $prefix \
                 RETURN d.id AS id LIMIT 1",
                params.clone(),
            )
        })?;
        if indexed.is_none() {
            return record_graph_episode(db, graph, workspace, repository);
        }
    }
    let stamp = now();
    graph.transaction(|g| {
        clear_prefix_in(g, workspace, GRAPH_NODE, &prefix)?;
        for node in &closed {
            forget_in(g, workspace, (GRAPH_NODE, &node_id(repository, node)))?;
        }
        let mut recorded = 0;
        for (node, relpath) in &inserted {
            let dependent = node_id(repository, node);
            forget_in(g, workspace, (GRAPH_NODE, &dependent))?;
            let Some(relpath) = relpath else {
                continue;
            };
            record_in(
                g,
                workspace,
                (GRAPH_NODE, &dependent),
                (FILE, &file_id(repository, relpath)),
                &stamp,
            )?;
            recorded += 1;
        }
        Ok(recorded)
    })
}

/// Record `chunk → file` dependencies for the chunks of one committed file and
/// clear their stale markers (the chunks were just rebuilt). The chunk lines
/// come from the caller — the content store's `chunk_lines_for`
/// (`repo_chunks` has migrated there).
pub fn record_file_chunks(
    graph: &GraphStore,
    workspace: &str,
    repository: &str,
    path: &str,
    lines: &[i64],
) -> Result<usize> {
    let file = file_id(repository, path);
    let stamp = now();
    graph.transaction(|g| {
        for line in lines {
            let chunk = chunk_id(repository, path, *line);
            record_in(g, workspace, (CHUNK, &chunk), (FILE, &file), &stamp)?;
            clear_in(g, workspace, CHUNK, &chunk)?;
        }
        Ok(lines.len())
    })
}

/// Invalidate everything derived from a repository file that changed or was
/// removed through the sync/commit path.  Records nothing; the producers of the
/// derived artifacts own their dependency edges.
pub fn file_changed(
    graph: &GraphStore,
    workspace: &str,
    repository: &str,
    path: &str,
    reason: &str,
) -> Result<Vec<(String, String)>> {
    let id = file_id(repository, path);
    invalidate_source(graph, workspace, (FILE, &id), reason)
}

/// Invalidate everything derived from a connector document.
pub fn document_changed(
    graph: &GraphStore,
    workspace: &str,
    document: &str,
    reason: &str,
) -> Result<Vec<(String, String)>> {
    invalidate_source(graph, workspace, (DOCUMENT, document), reason)
}

/// A principal's permissions changed: invalidate cached answers/context recorded
/// against that principal and mark every artifact scoped to the revoked
/// sources stale.
pub fn permissions_changed(
    graph: &GraphStore,
    workspace: &str,
    principal: Option<&str>,
    revoked_sources: &[String],
    reason: &str,
) -> Result<Vec<(String, String)>> {
    let mut marked = match principal {
        Some(principal) => invalidate_source(graph, workspace, (PRINCIPAL, principal), reason)?,
        None => Vec::new(),
    };
    for source in revoked_sources {
        marked.extend(invalidate_source(graph, workspace, (SOURCE, source), reason)?);
    }
    marked.sort();
    marked.dedup();
    Ok(marked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> GraphStore {
        GraphStore::open_in_memory().unwrap()
    }

    #[test]
    fn transitive_invalidation_marks_every_dependent_once() {
        let graph = graph();
        record_dependency(&graph, "w", (CHUNK, "c1"), (FILE, "f")).unwrap();
        record_dependency(&graph, "w", (EMBEDDING, "e1"), (CHUNK, "c1")).unwrap();
        record_dependency(&graph, "w", (GRAPH_NODE, "n1"), (FILE, "f")).unwrap();
        record_dependency(&graph, "w", (SUMMARY, "s1"), (GRAPH_NODE, "n1")).unwrap();
        record_dependency(&graph, "w", (SUMMARY, "s1"), (EMBEDDING, "e1")).unwrap();
        // A cycle must terminate.
        record_dependency(&graph, "w", (FILE, "f"), (SUMMARY, "s1")).unwrap();
        let marked = invalidate_source(&graph, "w", (FILE, "f"), "edited").unwrap();
        assert_eq!(marked.len(), 4);
        assert!(is_stale(&graph, "w", SUMMARY, "s1").unwrap());
        assert!(!is_stale(&graph, "w", FILE, "f").unwrap());
        assert!(!is_stale(&graph, "other", SUMMARY, "s1").unwrap());
        let f = freshness(&graph, "w").unwrap();
        assert_eq!(f.stale_count, 4);
        assert_eq!(f.by_kind[SUMMARY], 1);
        assert!(f.oldest_since.is_some());
        assert!(clear_stale(&graph, "w", SUMMARY, "s1").unwrap());
        assert_eq!(freshness(&graph, "w").unwrap().stale_count, 3);
        assert!(freshness(&graph, "other").unwrap().is_fresh());
    }

    #[test]
    fn prefix_clearing_is_repository_scoped() {
        let graph = graph();
        mark_stale(&graph, "w", GRAPH_NODE, "r1:a", "x").unwrap();
        mark_stale(&graph, "w", GRAPH_NODE, "r1:b", "x").unwrap();
        mark_stale(&graph, "w", GRAPH_NODE, "r2:a", "x").unwrap();
        assert_eq!(clear_stale_prefix(&graph, "w", GRAPH_NODE, "r1:").unwrap(), 2);
        assert_eq!(freshness(&graph, "w").unwrap().stale_count, 1);
        assert_eq!(repository_freshness(&graph, "w", "r2").unwrap().stale_count, 1);
        assert_eq!(repository_freshness(&graph, "w", "r1").unwrap().stale_count, 0);
    }

    #[test]
    fn dependency_snapshots_and_scoped_removal() {
        let graph = graph();
        record_dependency(&graph, "w", (CHUNK, "r1:c"), (FILE, "r1:f")).unwrap();
        record_dependency(&graph, "w", (MEMORY, "m"), (DOCUMENT, "d")).unwrap();
        let edges = dependencies(&graph, "w").unwrap();
        assert_eq!(edges.len(), 2);
        let memory_edge: Vec<_> = edges
            .iter()
            .filter(|e| e.dependent_kind == MEMORY)
            .cloned()
            .collect();
        let gone = forget_dependencies(&graph, "w", &memory_edge).unwrap();
        assert_eq!(gone, 1);
        let rest = dependencies(&graph, "w").unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].dependent_kind, CHUNK);
        mark_stale(&graph, "w", MEMORY, "m", "x").unwrap();
        let markers = stale_markers(&graph, "w").unwrap();
        assert_eq!(markers, vec![(MEMORY.to_string(), "m".to_string())]);
        assert_eq!(clear_stale_markers(&graph, "w", &markers).unwrap(), 1);
        assert!(freshness(&graph, "w").unwrap().is_fresh());
        // Prefix removal touches both ends.
        assert_eq!(forget_id_prefix(&graph, "w", "r1:").unwrap(), 1);
        assert!(dependencies(&graph, "w").unwrap().is_empty());
    }
}
