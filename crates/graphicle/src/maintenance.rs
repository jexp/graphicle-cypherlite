//! Source removal, storage pruning and local backups share the same
//! persistence boundary. Forgetting (connector.delete, source.forget) is
//! implemented and verified in [`crate::deletion`].
//!
//! Backups run on the graph engine's primitives (migration plan §6 item
//! 10): checkpoint the live store, copy its snapshot files, then scrub the
//! *copy* — secrets, sessions and entitlements never leave the device. There
//! is no live-backup API to replace; a checkpointed snapshot is a consistent
//! point-in-time image, and the WAL is deliberately not copied.
use crate::deletion::{self, Scope};
use crate::store::{self, Store, decode, repo, s};
use crate::store_graph::{GraphStore, Params, json_to_value, one, rows};
use crate::{Error, Result, integer, now, string};
use rusqlite::{Connection, params as sqlparams};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

/// The graph store of this data directory (`<dir>/reviews.graph`), opened
/// beside the SQLite store while the migration runs (docs/store-api.md:
/// "beside, not instead of"). Opening applies the schema idempotently.
/// This MUST be the shared registry handle: a second writer open of the
/// same directory blocks on the engine lease the first handle's pooled
/// connections hold (a self-deadlock), which a `graph_for(db)` call below
/// this one would otherwise run into.
pub(crate) fn graph_store(store: &Store) -> Result<std::sync::Arc<GraphStore>> {
    // The Arc, not a clone: the registry holds only a Weak, and a bare
    // clone would let the registered handle drop while this one's pooled
    // connections still pin the engine lease — a later `graph_for(db)`
    // would then reopen the directory and block on its own lease forever.
    Ok(store.graph())
}

/// Whether this project's per-episode by-products live on the graph store
/// yet (they migrate with the projection pipeline); until then the legacy
/// SQL retention path serves the project.
pub(crate) fn project_episodes_on_graph(graph: &GraphStore, repository: &str) -> Result<bool> {
    graph.read(|g| {
        one(
            g,
            "MATCH (e:Episode {repository_id: $repo}) RETURN 1 AS x LIMIT 1",
            params(&[("repo", json!(repository))]),
        )
        .map(|r| r.is_some())
    })
}

fn params(pairs: &[(&str, Value)]) -> Params {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), json_to_value(v)))
        .collect()
}

/// A project event on the graph store: `(:Event {seq, kind, data,
/// created_at})-[:OF]->(:Project)`, `seq` continuing the project's log
/// (migration plan §2.2).
fn event(db: &mut cypherlite::Connection, repository: &str, kind: &str, data: Value) -> Result<()> {
    db.query_with_params(
        "MATCH (p:Project {id: $repo}) \
         OPTIONAL MATCH (e:Event)-[:OF]->(p) \
         WITH p, coalesce(max(e.seq), 0) + 1 AS seq \
         CREATE (:Event {seq: seq, kind: $kind, data: $data, created_at: $at, repository_id: $repo})-[:OF]->(p)",
        params(&[
            ("repo", json!(repository)),
            ("kind", json!(kind)),
            ("data", json!(data.to_string())),
            ("at", json!(now())),
        ]),
    )?;
    Ok(())
}

/// A JSON text property that may already be a map/list (the graph store
/// keeps JSON columns as map/list properties; opaque payloads stay
/// stringified).
fn decoded(value: &Value) -> Result<Value> {
    match value {
        Value::String(text) => Ok(serde_json::from_str(text)?),
        other => Ok(other.clone()),
    }
}

/// Counts of `cypher` per key set (the repository binds as `$repo`, the
/// keys as `$keys`), summed.
fn counted(
    db: &mut cypherlite::Connection,
    cypher: &str,
    repository: &str,
    keys: Vec<Value>,
) -> Result<i64> {
    Ok(one(
        db,
        cypher,
        params(&[("repo", json!(repository)), ("keys", json!(keys))]),
    )?
    .and_then(|r| r["n"].as_i64())
    .unwrap_or(0))
}

/// Delete one project: queue the cleanup of its published projections,
/// forget its dependency-index entries, purge its graph cache, delete its
/// node and everything scoped to it, and sign its receipt, with `removed`
/// added to the receipt's `removed`.
pub(crate) fn delete_project(
    db: &mut cypherlite::Connection,
    sql: &rusqlite::Transaction<'_>,
    p: &store::Principal,
    identifier: &str,
    mut removed: Value,
) -> Result<Value> {
    let connectors: Vec<String> = rows(
        db,
        "MATCH (c:Connector {repository_id: $repo}) RETURN c.id AS id ORDER BY c.id",
        params(&[("repo", json!(identifier))]),
    )?
    .iter()
    .map(|c| crate::store_graph::s(c, "id").to_string())
    .collect();
    crate::projection::enqueue(sql, identifier, "project_deleted")?;
    removed["references"] =
        deletion::forget_project_references(db, &p.workspace, identifier, &connectors)?;
    if let Some(cache) = crate::graph_projection::cache_root(sql, identifier) {
        crate::graph_materialize::purge(&cache)?;
    }
    // The SQL store cascaded from repo_projects; on the graph the cascade is
    // explicit: everything scoped to the project, then the project itself.
    db.query_with_params(
        "MATCH (n) WHERE n.repository_id = $repo AND NOT n:DeletionReceipt DETACH DELETE n",
        params(&[("repo", json!(identifier))]),
    )?;
    // Connector-scoped nodes (versions, stage, permissions) key on the
    // connector, not the project.
    if !connectors.is_empty() {
        db.query_with_params(
            "MATCH (n) WHERE n.connector_id IN $connectors DETACH DELETE n",
            params(&[("connectors", json!(connectors))]),
        )?;
    }
    db.query_with_params(
        "MATCH (n)-[:OF|IN_PROJECT|PART_OF]->(:Project {id: $repo}) DETACH DELETE n",
        params(&[("repo", json!(identifier))]),
    )?;
    db.query_with_params(
        "MATCH (p:Project {id: $repo}) DETACH DELETE p",
        params(&[("repo", json!(identifier))]),
    )?;
    // The SQL side still holds the project's registration and its legacy
    // rows; deleting the project row cascades to them.
    sql.execute("DELETE FROM repo_projects WHERE id=?", [identifier])?;
    graphicle_memory::cache::clear(identifier).map_err(Error::bad)?;
    deletion::project_receipt(db, sql, p, identifier, &connectors, removed)
}

/// `retention.apply` with `sources`: forget the documents the policy selects
/// through certified deletion (plan, forget, erase, verify), one receipt per
/// batch of up to 1,000 documents, each carrying the policy. The caller must
/// echo the `plan_hash` of `retention.preview`, so exactly the previewed
/// documents are forgotten.
fn apply_source_retention(
    graph: &GraphStore,
    sql: &rusqlite::Transaction<'_>,
    p: &store::Principal,
    repository: &Value,
    args: &Value,
) -> Result<Value> {
    let identifier = s(repository, "id");
    let policy = crate::retention::SourcePolicy::from_args(args)?;
    let plan = graph
        .read(|g| crate::retention::source_plan(g, p, repository, &policy, chrono::Utc::now()))?;
    if args["plan_hash"].as_str() != Some(plan.plan_hash.as_str()) {
        return Err(Error::conflict(
            "The retention plan changed or was not previewed; run retention.preview and pass its plan_hash",
        ));
    }
    let policy_value = json!(policy);
    let mut receipts = Vec::new();
    let mut forgotten_documents = 0usize;
    for (connector, documents) in &plan.documents {
        let listed: Vec<&String> = documents.iter().collect();
        for batch in listed.chunks(deletion::MAX_SCOPE_DOCUMENTS) {
            let scope =
                Scope::document_set(connector, batch.iter().map(|d| (*d).clone()).collect());
            let started = now();
            let planned =
                graph.read(|g| deletion::plan(g, sql, &p.workspace, identifier, &scope))?;
            if planned.sources() == 0 {
                continue;
            }
            crate::projection::enqueue(sql, identifier, "source_forgotten")?;
            let since = planned.since().map(str::to_string);
            // The SQL half (observer, overlay and product tables) goes before
            // the graph transaction: its forget opens graph transactions of
            // its own.
            let sql_forgotten = deletion::legacy::forget(
                sql,
                graph,
                &p.workspace,
                identifier,
                &scope,
                since.as_deref(),
            )?;
            let sql_erased = deletion::legacy::erase(sql, identifier, &scope)?;
            let receipt = graph.transaction(|g| {
                let mut forgotten =
                    deletion::forget(g, &p.workspace, identifier, &scope, since.as_deref())?;
                let mut erased = deletion::erase(g, identifier, &scope)?;
                deletion::merge_removed(&mut forgotten, &sql_forgotten);
                deletion::merge_removed(&mut erased, &sql_erased);
                graphicle_memory::cache::clear(identifier).map_err(Error::bad)?;
                event(
                    g,
                    identifier,
                    "source_forgotten",
                    json!({"derived_records_removed":forgotten["memories"],"retention":true}),
                )?;
                deletion::policy_receipt(
                    g,
                    sql,
                    p,
                    repository,
                    "retention.forget",
                    &planned,
                    &started,
                    forgotten,
                    erased,
                    Some(&policy_value),
                )
            })?;
            forgotten_documents += batch.len();
            receipts.push(receipt);
        }
    }
    Ok(json!({
        "forgotten_documents": forgotten_documents,
        "plan_hash": plan.plan_hash,
        "cutoff": plan.cutoff,
        "receipts": receipts,
        "receipt": receipts.last().cloned().unwrap_or(Value::Null),
        "local_backups_are_separate": true,
    }))
}

/// `storage.prune`: bound the content store to the newest `keep_snapshots`
/// (plus the current and run-referenced ones), drop unreferenced objects,
/// finished syncs and unreferenced original documents, and sign a receipt
/// over what was pruned.
fn storage_prune(
    sql: &rusqlite::Connection,
    db: &mut cypherlite::Connection,
    p: &store::Principal,
    repository: &Value,
    args: &Value,
) -> Result<Value> {
    let identifier = s(repository, "id");
    let keep = integer(args, "keep_snapshots", 20, 1000)?.max(1);
    let snapshots = rows(
        db,
        "MATCH (sn:Snapshot {repository_id: $repo}) \
         RETURN sn.sha256 AS sha256, sn.manifest AS manifest \
         ORDER BY sn.created_at DESC, sn.sha256",
        params(&[("repo", json!(identifier))]),
    )?;
    let mut keep_snapshots: BTreeSet<String> = snapshots
        .iter()
        .take(keep)
        .map(|r| crate::store_graph::s(r, "sha256").to_string())
        .collect();
    keep_snapshots.insert(s(repository, "snapshot").to_string());
    for run in rows(
        db,
        "MATCH (r:Run {repository_id: $repo}) RETURN r.start_snapshot AS start_snapshot, r.end_snapshot AS end_snapshot",
        params(&[("repo", json!(identifier))]),
    )? {
        for key in ["start_snapshot", "end_snapshot"] {
            if let Some(v) = run[key].as_str() {
                keep_snapshots.insert(v.to_string());
            }
        }
    }
    // Reachable objects: the heads, the kept snapshots' manifests and the
    // dependencies recorded in memory revisions.
    let mut objects: BTreeSet<String> = rows(
        db,
        "MATCH (f:File {repository_id: $repo})-[:HEAD]->(o:Object) RETURN o.sha256 AS sha256",
        params(&[("repo", json!(identifier))]),
    )?
    .iter()
    .map(|r| crate::store_graph::s(r, "sha256").to_string())
    .collect();
    let mut snapshots_removed: Vec<String> = Vec::new();
    for snapshot in &snapshots {
        let sha = crate::store_graph::s(snapshot, "sha256").to_string();
        if keep_snapshots.contains(&sha) {
            let manifest: Value = decoded(&snapshot["manifest"])?;
            for value in manifest.as_object().into_iter().flat_map(|o| o.values()) {
                if let Some(sha) = value["sha256"].as_str() {
                    objects.insert(sha.to_string());
                }
            }
        } else {
            db.query_with_params(
                "MATCH (sn:Snapshot {repository_id: $repo, sha256: $sha}) DETACH DELETE sn",
                params(&[("repo", json!(identifier)), ("sha", json!(sha))]),
            )?;
            snapshots_removed.push(sha);
        }
    }
    for revision in rows(
        db,
        "MATCH (rev:MemoryRevision)-[:REVISION_OF]->(m:Memory)-[:OF]->(:Project {id: $repo}) \
         RETURN rev.data AS data",
        params(&[("repo", json!(identifier))]),
    )? {
        let mut memory: Value = decoded(&revision["data"])?;
        decode(&mut memory, "data")?;
        for sha in memory["data"]["dependencies"]
            .as_object()
            .into_iter()
            .flat_map(|o| o.values())
            .filter_map(Value::as_str)
        {
            objects.insert(sha.to_string());
        }
    }
    let mut freed = 0u64;
    let mut objects_removed: Vec<String> = Vec::new();
    for object in rows(
        db,
        "MATCH (o:Object {repository_id: $repo}) RETURN o.sha256 AS sha256, o.bytes AS bytes",
        params(&[("repo", json!(identifier))]),
    )? {
        let sha = crate::store_graph::s(&object, "sha256").to_string();
        if !objects.contains(&sha) {
            freed += object["bytes"].as_u64().unwrap_or(0);
            objects_removed.push(sha);
        }
    }
    if !objects_removed.is_empty() {
        db.query_with_params(
            "MATCH (o:Object {repository_id: $repo}) WHERE o.sha256 IN $keys DETACH DELETE o",
            params(&[
                ("repo", json!(identifier)),
                ("keys", json!(objects_removed)),
            ]),
        )?;
    }
    let syncs_removed = counted(
        db,
        "MATCH (sy:Sync {repository_id: $repo}) WHERE NOT sy.state IN ['pending','preparing'] RETURN count(sy) AS n",
        identifier,
        vec![],
    )?;
    if syncs_removed > 0 {
        db.query_with_params(
            "MATCH (sy:Sync {repository_id: $repo}) WHERE NOT sy.state IN ['pending','preparing'] DETACH DELETE sy",
            params(&[("repo", json!(identifier))]),
        )?;
    }
    let mut originals_freed = 0u64;
    let mut originals_removed: Vec<(String, String)> = Vec::new();
    for original in rows(
        db,
        "MATCH (od:OriginalDocument {repository_id: $repo}) \
         RETURN od.source_id AS source_id, od.sha256 AS sha256, od.text_sha256 AS text_sha256, od.bytes AS bytes",
        params(&[("repo", json!(identifier))]),
    )? {
        // Keep originals referenced by any retained source object, including
        // snapshots.
        let source_id = crate::store_graph::s(&original, "source_id").to_string();
        let sha = crate::store_graph::s(&original, "sha256").to_string();
        // The connector document this original belongs to: its id is the raw
        // document id (the `connector:<connector>:<id>` form is the link
        // rows' node), and its metadata is one JSON text property.
        let current = source_id
            .strip_prefix("connector:")
            .and_then(|rest| rest.split_once(':'))
            .is_some_and(|(connector, document)| {
                one(
                    db,
                    "MATCH (d:ConnectorDoc {repository_id: $repo, connector_id: $connector, id: $document, present: true}) \
                     RETURN d.metadata AS metadata LIMIT 1",
                    params(&[
                        ("repo", json!(identifier)),
                        ("connector", json!(connector)),
                        ("document", json!(document)),
                    ]),
                )
                .ok()
                .flatten()
                .and_then(|row| {
                    serde_json::from_str::<Value>(crate::store_graph::s(&row, "metadata")).ok()
                })
                .is_some_and(|metadata| {
                    metadata["provider_metadata"]["original_sha256"].as_str() == Some(sha.as_str())
                })
            });
        if !current && !objects.contains(crate::store_graph::s(&original, "text_sha256")) {
            originals_freed += original["bytes"].as_u64().unwrap_or(0);
            db.query_with_params(
                "MATCH (od:OriginalDocument {repository_id: $repo, source_id: $source_id, sha256: $sha}) DETACH DELETE od",
                params(&[
                    ("repo", json!(identifier)),
                    ("source_id", json!(source_id)),
                    ("sha", json!(sha)),
                ]),
            )?;
            originals_removed.push((source_id, sha));
        }
    }
    let mut result = json!({"bytes_reclaimed":freed+originals_freed,"original_bytes_reclaimed":originals_freed,"kept_snapshots":keep_snapshots.len(),"memory_revisions_preserved":true});
    if !(snapshots_removed.is_empty() && objects_removed.is_empty() && originals_removed.is_empty())
    {
        // Re-count what was pruned, then sign: no silent deletion.
        let remaining = json!({
            "repo_snapshots": counted(db, "MATCH (sn:Snapshot {repository_id: $repo}) WHERE sn.sha256 IN $keys RETURN count(sn) AS n", identifier, snapshots_removed.iter().map(|k| json!(k)).collect())?,
            "repo_objects": counted(db, "MATCH (o:Object {repository_id: $repo}) WHERE o.sha256 IN $keys RETURN count(o) AS n", identifier, objects_removed.iter().map(|k| json!(k)).collect())?,
            "am_original_documents": counted(db, "UNWIND $keys AS pair MATCH (od:OriginalDocument {repository_id: $repo, source_id: pair[0], sha256: pair[1]}) RETURN count(od) AS n", identifier, originals_removed.iter().map(|(a,b)| json!([a,b])).collect())?,
        });
        let mut pruned: Vec<String> = Vec::new();
        pruned.extend(snapshots_removed.iter().map(|k| format!("snapshot:{k}")));
        pruned.extend(objects_removed.iter().map(|k| format!("object:{k}")));
        pruned.extend(originals_removed.iter().map(|(a, b)| format!("original:{a}@{b}")));
        let removed = json!({"repo_snapshots":snapshots_removed.len(),"repo_objects":objects_removed.len(),"repo_syncs":syncs_removed,"am_original_documents":originals_removed.len(),"bytes":freed+originals_freed});
        result["receipt"] = deletion::rows_receipt(
            db,
            sql,
            p,
            identifier,
            "storage.prune",
            "storage",
            json!({"keep_snapshots": keep}),
            removed,
            &pruned,
            remaining,
            false,
        )?;
    }
    Ok(result)
}

// ------------------------------------------------------ legacy SQL fallback
//
// A project whose rows have not migrated to the graph store yet
// (docs/store-api.md: "beside, not instead of") is served by these SQL
// paths; the receipt lands in the legacy SQL store, where
// [`crate::deletion::verify`] and [`crate::deletion::list`] find it.

/// Rows of `sql` still present for each key (the repository binds first,
/// then the key's columns), summed.
fn still_present(
    db: &Connection,
    sql: &str,
    repository: &str,
    keys: impl Iterator<Item = Vec<String>>,
) -> Result<i64> {
    let mut total = 0;
    for key in keys {
        total += db.query_row(
            sql,
            rusqlite::params_from_iter(std::iter::once(repository.to_string()).chain(key)),
            |r| r.get::<_, i64>(0),
        )?;
    }
    Ok(total)
}

/// [`delete_project`] against the SQL store (see it for the contract).
pub(crate) fn delete_project_sql(
    db: &Connection,
    p: &store::Principal,
    identifier: &str,
    mut removed: Value,
    graph_cache: bool,
) -> Result<Value> {
    let connectors: Vec<String> = store::rows(
        db,
        "SELECT id FROM am_connectors WHERE repository_id=? ORDER BY id",
        [identifier],
    )?
    .iter()
    .map(|c| s(c, "id").to_string())
    .collect();
    crate::projection::enqueue(db, identifier, "project_deleted")?;
    removed["references"] =
        deletion::legacy::forget_project_references(db, &p.workspace, identifier, &connectors)?;
    if let Some(cache) = crate::graph_projection::cache_root(db, identifier) {
        crate::graph_materialize::purge(&cache)?;
    }
    db.execute("DELETE FROM repo_projects WHERE id=?", [identifier])?;
    graphicle_memory::cache::clear(identifier).map_err(Error::bad)?;
    deletion::legacy::project_receipt(db, p, identifier, &connectors, removed)
}

/// `project.delete` of a project whose snapshot was checked: the PR scratch
/// projects `graphicle pr` keeps for it hold copies of its code, so they go
/// first, each with its own receipt, and the project's receipt names them;
/// `removed` joins the project's receipt's `removed`.
fn delete_with_copies(
    g: &mut cypherlite::Connection,
    sql: &rusqlite::Transaction<'_>,
    p: &store::Principal,
    identifier: &str,
    mut removed: Value,
) -> Result<Value> {
    let copies = crate::folders::scratch_projects(sql, identifier)?;
    let mut scratch = Vec::with_capacity(copies.len());
    for copy in &copies {
        scratch.push(delete_project(g, sql, p, copy, json!({}))?);
    }
    if !copies.is_empty() {
        removed["pr_scratch_projects"] = json!(copies);
    }
    let receipt = delete_project(g, sql, p, identifier, removed)?;
    let mut result = json!({"deleted":true,"local_backups_are_separate":true,"receipt":receipt});
    if !scratch.is_empty() {
        result["pr_scratch_projects"] = json!(scratch);
    }
    Ok(result)
}

/// [`apply_source_retention`] against the SQL store.
fn apply_source_retention_sql(
    db: &Connection,
    graph: &GraphStore,
    p: &store::Principal,
    repository: &Value,
    args: &Value,
) -> Result<Value> {
    let identifier = s(repository, "id");
    let policy = crate::retention::SourcePolicy::from_args(args)?;
    let plan =
        crate::retention::legacy::source_plan(db, graph, p, repository, &policy, chrono::Utc::now())?;
    if args["plan_hash"].as_str() != Some(plan.plan_hash.as_str()) {
        return Err(Error::conflict(
            "The retention plan changed or was not previewed; run retention.preview and pass its plan_hash",
        ));
    }
    let policy_value = json!(policy);
    let mut receipts = Vec::new();
    let mut forgotten_documents = 0usize;
    for (connector, documents) in &plan.documents {
        let listed: Vec<&String> = documents.iter().collect();
        for batch in listed.chunks(deletion::MAX_SCOPE_DOCUMENTS) {
            let scope =
                Scope::document_set(connector, batch.iter().map(|d| (*d).clone()).collect());
            let started = now();
            let forget_plan = deletion::legacy::plan(db, &p.workspace, identifier, &scope)?;
            if forget_plan.sources() == 0 {
                continue;
            }
            crate::projection::enqueue(db, identifier, "source_forgotten")?;
            let since = forget_plan.since().map(str::to_string);
            let forgotten =
                deletion::legacy::forget(db, graph, &p.workspace, identifier, &scope, since.as_deref())?;
            let erased = deletion::legacy::erase(db, identifier, &scope)?;
            graphicle_memory::cache::clear(identifier).map_err(Error::bad)?;
            store::event(
                db,
                identifier,
                "source_forgotten",
                json!({"derived_records_removed":forgotten["memories"],"retention":true}),
            )?;
            forgotten_documents += batch.len();
            receipts.push(deletion::legacy::policy_receipt(
                db,
                p,
                repository,
                "retention.forget",
                forget_plan,
                &started,
                forgotten,
                erased,
                Some(&policy_value),
            )?);
        }
    }
    Ok(json!({
        "forgotten_documents": forgotten_documents,
        "plan_hash": plan.plan_hash,
        "cutoff": plan.cutoff,
        "receipts": receipts,
        "receipt": receipts.last().cloned().unwrap_or(Value::Null),
        "local_backups_are_separate": true,
    }))
}

/// [`storage_prune`] against the SQL store.
fn storage_prune_sql(
    db: &Connection,
    p: &store::Principal,
    repository: &Value,
    args: &Value,
) -> Result<Value> {
    let identifier = s(repository, "id");
    let keep = integer(args, "keep_snapshots", 20, 1000)?.max(1);
    let snapshots = store::rows(
        db,
        "SELECT sha256,manifest FROM repo_snapshots WHERE repository_id=? ORDER BY created_at DESC,sha256",
        [identifier],
    )?;
    let mut keep_snapshots: BTreeSet<String> = snapshots
        .iter()
        .take(keep)
        .map(|r| s(r, "sha256").to_string())
        .collect();
    keep_snapshots.insert(s(repository, "snapshot").to_string());
    for run in store::rows(
        db,
        "SELECT start_snapshot,end_snapshot FROM repo_runs WHERE repository_id=?",
        [identifier],
    )? {
        for key in ["start_snapshot", "end_snapshot"] {
            if let Some(v) = run[key].as_str() {
                keep_snapshots.insert(v.to_string());
            }
        }
    }
    let mut objects: BTreeSet<String> = store::heads(db, identifier)?.into_values().collect();
    let mut pruned: Vec<String> = Vec::new();
    let (mut snapshots_removed, mut objects_removed, mut originals_removed) =
        (Vec::new(), Vec::new(), Vec::new());
    for snapshot in &snapshots {
        if keep_snapshots.contains(s(snapshot, "sha256")) {
            let manifest: Value = serde_json::from_str(s(snapshot, "manifest"))?;
            for value in manifest.as_object().into_iter().flat_map(|o| o.values()) {
                if let Some(sha) = value["sha256"].as_str() {
                    objects.insert(sha.to_string());
                }
            }
        } else {
            db.execute(
                "DELETE FROM repo_snapshots WHERE repository_id=? AND sha256=?",
                sqlparams![identifier, s(snapshot, "sha256")],
            )?;
            snapshots_removed.push(s(snapshot, "sha256").to_string());
        }
    }
    for revision in store::rows(db,"SELECT r.data FROM repo_memory_revisions r JOIN repo_memories m ON m.id=r.memory_id WHERE m.repository_id=?",[identifier])? {
        let mut memory: Value = serde_json::from_str(s(&revision, "data"))?;
        decode(&mut memory, "data")?;
        for sha in memory["data"]["dependencies"]
            .as_object()
            .into_iter()
            .flat_map(|o| o.values())
            .filter_map(Value::as_str)
        {
            objects.insert(sha.to_string());
        }
    }
    let mut freed = 0;
    for object in store::rows(
        db,
        "SELECT sha256,bytes FROM repo_objects WHERE repository_id=?",
        [identifier],
    )? {
        if !objects.contains(s(&object, "sha256")) {
            freed += object["bytes"].as_u64().unwrap_or(0);
            db.execute(
                "DELETE FROM repo_objects WHERE repository_id=? AND sha256=?",
                sqlparams![identifier, s(&object, "sha256")],
            )?;
            objects_removed.push(s(&object, "sha256").to_string());
        }
    }
    let syncs_removed = db.execute(
        "DELETE FROM repo_syncs WHERE repository_id=? AND state NOT IN ('pending','preparing')",
        [identifier],
    )?;
    let mut originals_freed = 0;
    for original in store::rows(db,"SELECT source_id,sha256,text_sha256,length(bytes) AS bytes FROM am_original_documents WHERE repository_id=?",[identifier])? {
        // Keep originals referenced by any retained source object, including snapshots.
        let current = crate::store::one(db,"SELECT 1 AS yes FROM am_connector_docs WHERE connector_id IN (SELECT id FROM am_connectors WHERE repository_id=?) AND 'connector:'||connector_id||':'||id=? AND present=1 AND json_extract(metadata,'$.provider_metadata.original_sha256')=?",sqlparams![identifier,s(&original,"source_id"),s(&original,"sha256")])?.is_some();
        if !current && !objects.contains(s(&original, "text_sha256")) {
            originals_freed += original["bytes"].as_u64().unwrap_or(0);
            db.execute(
                "DELETE FROM am_original_documents WHERE repository_id=? AND source_id=? AND sha256=?",
                sqlparams![identifier, s(&original, "source_id"), s(&original, "sha256")],
            )?;
            originals_removed.push((
                s(&original, "source_id").to_string(),
                s(&original, "sha256").to_string(),
            ));
        }
    }
    let mut result = json!({"bytes_reclaimed":freed+originals_freed,"original_bytes_reclaimed":originals_freed,"kept_snapshots":keep_snapshots.len(),"memory_revisions_preserved":true});
    if !(snapshots_removed.is_empty() && objects_removed.is_empty() && originals_removed.is_empty())
    {
        // Re-count what was pruned, then sign: no silent deletion.
        let remaining = json!({
            "repo_snapshots":still_present(db,"SELECT count(*) FROM repo_snapshots WHERE repository_id=? AND sha256=?",identifier,snapshots_removed.iter().map(|k|vec![k.clone()]))?,
            "repo_objects":still_present(db,"SELECT count(*) FROM repo_objects WHERE repository_id=? AND sha256=?",identifier,objects_removed.iter().map(|k|vec![k.clone()]))?,
            "am_original_documents":still_present(db,"SELECT count(*) FROM am_original_documents WHERE repository_id=? AND source_id=? AND sha256=?",identifier,originals_removed.iter().map(|(a,b)|vec![a.clone(),b.clone()]))?,
        });
        pruned.extend(snapshots_removed.iter().map(|k| format!("snapshot:{k}")));
        pruned.extend(objects_removed.iter().map(|k| format!("object:{k}")));
        pruned.extend(originals_removed.iter().map(|(a, b)| format!("original:{a}@{b}")));
        let removed = json!({"repo_snapshots":snapshots_removed.len(),"repo_objects":objects_removed.len(),"repo_syncs":syncs_removed,"am_original_documents":originals_removed.len(),"bytes":freed+originals_freed});
        result["receipt"] = deletion::legacy::rows_receipt(
            db,
            p,
            identifier,
            "storage.prune",
            "storage",
            json!({"keep_snapshots": keep}),
            removed,
            &pruned,
            remaining,
            false,
        )?;
    }
    Ok(result)
}

impl Store {
    pub fn maintain(
        &self,
        workspace: &str,
        token: &str,
        operation: &str,
        args: &Value,
    ) -> Result<Value> {
        let graph = graph_store(self)?;
        let mut result = self.authorized(workspace,token,"admin",|db,p| {
            let repository=repo(db,p,string(args,"repository_id",100)?)?;
            let identifier=s(&repository,"id").to_string();
            match operation {
                "connector.state" | "connector.delete" | "source.forget"=>{
                    let connector=string(args,"connector_id",100)?;
                    let present = graph.read(|g| one(g,"MATCH (c:Connector {repository_id: $repo, id: $id}) RETURN c.id AS id",params(&[("repo",json!(&identifier)),("id",json!(connector))])))?.is_some();
                    if !present {
                        // The connector lives in the legacy SQL store of a
                        // project mid-migration: the legacy path serves it.
                        crate::store::one(db,"SELECT id FROM am_connectors WHERE repository_id=? AND id=?",sqlparams![&identifier,connector])?.ok_or_else(||Error::bad("Connector unavailable"))?;
                        if operation!="connector.state" {
                            let scope=if operation=="connector.delete" {Scope::connector(connector)} else {Scope::documents(connector,&args["document_ids"])?};
                            // Capture coverage first; forget, erase, verify and sign in one transaction.
                            let started=now();
                            let plan=deletion::legacy::plan(db,&p.workspace,&identifier,&scope)?;
                            if operation=="source.forget" && plan.sources()==0 {return Err(Error::bad("None of these documents exist in this connector"));}
                            crate::projection::enqueue(db,&identifier,if operation=="connector.delete" {"connector_deleted"} else {"source_forgotten"})?;
                            let since=plan.since().map(str::to_string);
                            let forgotten=deletion::legacy::forget(db,&graph,&p.workspace,&identifier,&scope,since.as_deref())?;
                            let erased=deletion::legacy::erase(db,&identifier,&scope)?;
                            // In-process features and layouts of forgotten text go now, not at expiry.
                            graphicle_memory::cache::clear(&identifier).map_err(Error::bad)?;
                            let derived=forgotten["memories"].clone();
                            store::event(db,&identifier,"source_forgotten",json!({"derived_records_removed":derived}))?;
                            let receipt=deletion::legacy::receipt(db,p,&repository,operation,plan,&started,forgotten,erased)?;
                            return Ok(json!({"deleted":true,"derived_records_removed":derived,"local_backups_are_separate":true,"receipt":receipt}));
                        }
                        let state=string(args,"state",20)?;
                        if !["active","paused"].contains(&state){return Err(Error::bad("Activate or pause this connector"));}
                        db.execute("UPDATE am_connectors SET state=?,generation=generation+1,cursor=NULL,last_sync=NULL WHERE id=?",sqlparams![state,connector])?;
                        db.execute("DELETE FROM am_connector_stage WHERE connector_id=?",[connector])?;
                        db.execute("DELETE FROM am_model_cache WHERE repository_id=?",[&identifier])?;
                        store::invalidate(db,&identifier)?;
                        return Ok(json!({"state":state,"remote_sources_withheld_until_sync":state=="active"}));
                    }
                    if operation!="connector.state" {
                        let scope=if operation=="connector.delete" {Scope::connector(connector)} else {Scope::documents(connector,&args["document_ids"])?};
                        // Capture coverage first; forget, erase, verify and sign in one transaction.
                        // The SQL half (observer, overlay and product tables)
                        // goes before the graph transaction: its forget opens
                        // graph transactions of its own.
                        let sql_plan=deletion::legacy::plan(db,&p.workspace,&identifier,&scope)?;
                        let planned=graph.read(|g| deletion::plan(g,db,&p.workspace,&identifier,&scope))?;
                        if operation=="source.forget" && planned.sources()==0 && sql_plan.sources()==0 {return Err(Error::bad("None of these documents exist in this connector"));}
                        crate::projection::enqueue(db,&identifier,if operation=="connector.delete" {"connector_deleted"} else {"source_forgotten"})?;
                        let sql_since=planned.since().map(str::to_string);
                        let sql_forgotten=deletion::legacy::forget(db,&graph,&p.workspace,&identifier,&scope,sql_since.as_deref())?;
                        let sql_erased=deletion::legacy::erase(db,&identifier,&scope)?;
                        return graph.transaction(|g| {
                            let started=now();
                            let since=planned.since().map(str::to_string);
                            let mut forgotten=deletion::forget(g,&p.workspace,&identifier,&scope,since.as_deref())?;
                            let mut erased=deletion::erase(g,&identifier,&scope)?;
                            deletion::merge_removed(&mut forgotten,&sql_forgotten);
                            deletion::merge_removed(&mut erased,&sql_erased);
                            // In-process features and layouts of forgotten text go now, not at expiry.
                            graphicle_memory::cache::clear(&identifier).map_err(Error::bad)?;
                            let derived=forgotten["memories"].clone();
                            event(g,&identifier,"source_forgotten",json!({"derived_records_removed":derived}))?;
                            let receipt=deletion::receipt(g,db,p,&repository,operation,&planned,&started,forgotten,erased)?;
                            Ok(json!({"deleted":true,"derived_records_removed":derived,"local_backups_are_separate":true,"receipt":receipt}))
                        });
                    }
                    let state=string(args,"state",20)?;
                    if !["active","paused"].contains(&state){return Err(Error::bad("Activate or pause this connector"));}
                    graph.transaction(|g| {
                        g.query_with_params("MATCH (c:Connector {id: $id}) SET c.state=$state, c.generation=coalesce(c.generation,0)+1, c.cursor=null, c.last_sync=null",params(&[("id",json!(connector)),("state",json!(state))]))?;
                        g.query_with_params("MATCH (st:ConnectorStage {connector_id: $id}) DETACH DELETE st",params(&[("id",json!(connector))]))?;
                        g.query_with_params("MATCH (m:ModelCache {repository_id: $repo}) DETACH DELETE m",params(&[("repo",json!(identifier))]))?;
                        Ok(())
                    })?;
                    store::invalidate(db,&identifier)?;
                    Ok(json!({"state":state,"remote_sources_withheld_until_sync":state=="active"}))
                },
                "project.delete"=>{
                    if args["expected_snapshot"]!=repository["snapshot"] {return Err(Error::conflict("Project snapshot changed; inspect it before deleting"));}
                    // The PR scratch projects `graphicle pr` keeps for this
                    // project hold copies of its code: they go
                    // first, each with its own receipt, and this project's
                    // receipt names them.
                    let copies=crate::folders::scratch_projects(db,&identifier)?;
                    let on_graph = graph.read(|g| one(g,"MATCH (p:Project {id: $repo}) RETURN 1 AS x",params(&[("repo",json!(&identifier))])))?.is_some();
                    if !on_graph {
                        // Legacy SQL store (project mid-migration).
                        let mut scratch=Vec::with_capacity(copies.len());
                        for copy in &copies {
                            scratch.push(delete_project_sql(db,p,copy,json!({}),false)?);
                        }
                        let removed=if copies.is_empty() {json!({})} else {json!({"pr_scratch_projects":copies})};
                        let receipt=delete_project_sql(db,p,&identifier,removed,false)?;
                        let mut result=json!({"deleted":true,"local_backups_are_separate":true,"receipt":receipt});
                        if !scratch.is_empty() {result["pr_scratch_projects"]=json!(scratch);}
                        return Ok(result);
                    }
                    graph.transaction(|g| {
                        let mut scratch=Vec::with_capacity(copies.len());
                        for copy in &copies {
                            scratch.push(delete_project(g,db,p,copy,json!({}))?);
                        }
                        let removed=if copies.is_empty() {json!({})} else {json!({"pr_scratch_projects":copies})};
                        let receipt=delete_project(g,db,p,&identifier,removed)?;
                        let mut result=json!({"deleted":true,"local_backups_are_separate":true,"receipt":receipt});
                        if !scratch.is_empty() {result["pr_scratch_projects"]=json!(scratch);}
                        Ok(result)
                    })
                },
                "storage.prune"=>{
                    let on_graph = graph.read(|g| one(g,"MATCH (n {repository_id: $repo}) WHERE n:Snapshot OR n:Object OR n:OriginalDocument RETURN 1 AS x LIMIT 1",params(&[("repo",json!(&identifier))])))?.is_some();
                    if on_graph {
                        graph.transaction(|g| storage_prune(db,g,p,&repository,args))
                    } else {
                        storage_prune_sql(db,p,&repository,args)
                    }
                },
                "retention.apply"=>{
                    if args.get("sources").is_some_and(|v|!v.is_null()) {
                        let on_graph = graph.read(|g| one(g,"MATCH (c:Connector {repository_id: $repo}) RETURN 1 AS x LIMIT 1",params(&[("repo",json!(&identifier))])))?.is_some();
                        if on_graph {
                            return apply_source_retention(&graph,db,p,&repository,args);
                        }
                        return apply_source_retention_sql(db,&graph,p,&repository,args);
                    }
                    let retention=crate::retention::Retention::from_args(args)?;
                    let on_graph = graph.read(|g| one(g,"MATCH (e:Episode {repository_id: $repo}) RETURN 1 AS x LIMIT 1",params(&[("repo",json!(&identifier))])))?.is_some();
                    if !on_graph {
                        // Legacy SQL store (project mid-migration).
                        let mut result=crate::retention::legacy::apply(db,&identifier,&retention)?;
                        let removed=result["deleted"].clone();
                        if removed.as_object().is_some_and(|counts|counts.values().any(|n|n.as_u64().unwrap_or(0)>0)) {
                            let remaining=crate::retention::legacy::remaining(db,&identifier,&retention)?;
                            result["receipt"]=deletion::legacy::rows_receipt(db,p,&identifier,"retention.graph","graph_by_products",json!(retention),removed,&[],remaining,false)?;
                        }
                        return Ok(result);
                    }
                    graph.transaction(|g| {
                        let mut result=crate::retention::apply(g,&identifier,&retention)?;
                        let removed=result["deleted"].clone();
                        if removed.as_object().is_some_and(|counts|counts.values().any(|n|n.as_u64().unwrap_or(0)>0)) {
                            let remaining=crate::retention::remaining(g,&identifier,&retention)?;
                            result["receipt"]=deletion::rows_receipt(g,db,p,&identifier,"retention.graph","graph_by_products",json!(retention),removed,&[],remaining,false)?;
                        }
                        Ok(result)
                    })
                },
                _=>Err(Error::bad("Unknown maintenance operation"))
            }
        })?;
        if result.get("receipt").is_some() {
            // After commit, flush the deleted nodes' old images into a
            // snapshot so the write-ahead log no longer carries them; the
            // legacy SQL half's WAL is truncated too, while the store is
            // dual. The graph checkpoint reuses this call's handle: the
            // engine admits one writer per store.
            graph.checkpoint()?;
            let mut storage = deletion::legacy::checkpoint(self)?;
            storage["snapshot_checkpoint"] = json!("ok");
            result["storage"] = storage;
        }
        Ok(result)
    }
    /// Offboarding's deletion of a project ([`crate::offboard`]): what
    /// `project.delete` does, with `offboarding` (the files it removed and
    /// what it could not reach) in the project's receipt. Only the owner.
    pub fn offboard_project(
        &self,
        workspace: &str,
        token: &str,
        identifier: &str,
        expected_snapshot: &str,
        offboarding: Value,
    ) -> Result<Value> {
        let mut result = self.authorized(workspace, token, "admin", |db, p| {
            let repository = repo(db, p, identifier)?;
            if s(&repository, "snapshot") != expected_snapshot {
                return Err(Error::conflict(
                    "Project snapshot changed since the offboarding plan was shown; run graphicle offboard again",
                ));
            }
            let graph = self.graph();
            let mut g = graph.connection()?;
            delete_with_copies(&mut g, db, p, identifier, json!({"offboarding": offboarding}))
        })?;
        result["storage"] = deletion::checkpoint(self)?;
        Ok(result)
    }

    /// A portable backup at `path`. While the store is dual
    /// (docs/store-api.md: "beside, not instead of") a backup carries both
    /// halves: the SQL store as a single scrubbed file at `path`, and the
    /// graph store as a scrubbed snapshot directory beside it
    /// (`<path>.graph`). A `path` ending in `.graph` backs up the graph
    /// store alone (a directory the call creates): checkpoint the live
    /// store, copy its snapshot files, then scrub the copy — provider
    /// credentials, sessions and entitlements never leave the device.
    pub fn backup(&self, path: &Path) -> Result<Value> {
        if path.extension().is_some_and(|e| e == "graph") {
            let store = graph_store(self)?;
            return backup_graph(&store, path);
        }
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(path)?;
        drop(file);
        let result = (|| {
            self.connect()?.backup(rusqlite::MAIN_DB, path, None)?;
            let backup = Connection::open(path)?;
            // A portable source backup must not silently contain unusable encrypted
            // credentials or require copying the device's master key beside it.
            backup.execute_batch("PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON; BEGIN IMMEDIATE; UPDATE am_connectors SET state='paused',last_sync=NULL,cursor=NULL,generation=generation+1 WHERE credential_env LIKE 'secret:%' OR credential_env LIKE 'oauth:%'; UPDATE am_projection_cleanup SET state='paused',lease_token=NULL,lease_until=NULL WHERE state!='complete'; UPDATE am_jobs SET state='cancelled' WHERE state IN ('queued','retry','running'); DELETE FROM am_secrets; COMMIT;")?;
            // Sessions, provisioning tokens and paid entitlements are scrubbed
            // on any store that has the team and commerce tables, whichever
            // edition takes the backup; then the copy is compacted.
            crate::store_compat::scrub_backup(&backup)?;
            backup.execute_batch("VACUUM;")?;
            let check: String = backup.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            if check != "ok" {
                return Err(Error::bad("Backup integrity verification failed"));
            }
            // The graph half, beside the SQL file.
            let mut out =
                json!({"path":path,"created_at":now(),"integrity":"ok","contains_private_sources":true,"provider_credentials_included":false,"reconnect_accounts_after_restore":true});
            let graph = graph_store(self)?;
            graph.checkpoint()?;
            if graph
                .path()
                .is_some_and(|p| p.join("graph.json").is_file())
            {
                let graph_path = std::path::PathBuf::from(format!("{}.graph", path.display()));
                backup_graph(&graph, &graph_path)?;
                out["graph"] = json!(graph_path);
            }
            Ok(out)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(path);
            let _ = std::fs::remove_dir_all(crate::store_graph::backup_sidecar(path));
        }
        result
    }
}

/// The snapshot files of a directory store. The WAL is never copied: a
/// checkpointed snapshot is a consistent point-in-time image, and leaving
/// the log behind also leaves deleted data's old images behind.
const SNAPSHOT_FILES: &[&str] = &["manifest.json", "graph.json"];

/// [`Store::backup`] on any [`GraphStore`].
pub fn backup_graph(store: &GraphStore, path: &Path) -> Result<Value> {
    let source = store
        .path()
        .ok_or_else(|| Error::bad("An in-memory graph store cannot be backed up"))?
        .to_path_buf();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir(path)?;
    let result = (|| {
        store.checkpoint()?;
        for file in SNAPSHOT_FILES {
            let from = source.join(file);
            if from.is_file() {
                std::fs::copy(&from, path.join(file))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(
                        path.join(file),
                        std::fs::Permissions::from_mode(0o600),
                    )?;
                }
            }
        }
        if !path.join("graph.json").is_file() {
            return Err(Error::bad("The store has no snapshot to back up"));
        }
        let copy = GraphStore::open(path)?;
        scrub_graph_copy(&copy)?;
        crate::extension::installed().scrub_backup(&copy)?;
        copy.checkpoint()?;
        drop(copy);
        // Integrity: the scrubbed copy opens and answers.
        let check = GraphStore::open(path)?;
        check.read(|db| {
            one(db, "MATCH (n) RETURN count(n) AS n", Params::new())?;
            Ok(())
        })?;
        Ok(
            json!({"path":path,"created_at":now(),"integrity":"ok","contains_private_sources":true,"provider_credentials_included":false,"reconnect_accounts_after_restore":true}),
        )
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(path);
    }
    result
}

/// The core scrub of a backup copy (never the live store): connectors that
/// hold device credentials are paused, queued work is cancelled, and
/// secrets, sessions, provisioning tokens and entitlements are deleted.
/// Label-scoped throughout: a store without those labels scrubs to a no-op.
fn scrub_graph_copy(store: &GraphStore) -> Result<()> {
    store.transaction(|db| {
        db.query(
            "MATCH (c:Connector) WHERE c.credential_env STARTS WITH 'secret:' OR c.credential_env STARTS WITH 'oauth:' \
             SET c.state='paused', c.last_sync=null, c.cursor=null, c.generation=coalesce(c.generation,0)+1",
        )?;
        db.query(
            "MATCH (p:ProjectionCleanup) WHERE p.state <> 'complete' \
             SET p.state='paused', p.lease_token=null, p.lease_until=null",
        )?;
        db.query("MATCH (j:Job) WHERE j.state IN ['queued','retry','running'] SET j.state='cancelled'")?;
        db.query("MATCH (s:Secret) DETACH DELETE s")?;
        db.query("MATCH (s:Session) DETACH DELETE s")?;
        db.query("MATCH (t:ProvisioningToken) DETACH DELETE t")?;
        db.query("MATCH (e:Entitlement) DETACH DELETE e")?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backup is the checkpointed snapshot, scrubbed: secrets and queued
    /// jobs are gone from the copy, present in the live store.
    #[test]
    fn backup_copies_the_snapshot_and_scrubs_the_copy() {
        let directory = tempfile::tempdir().unwrap();
        let store = GraphStore::open(directory.path().join("reviews.graph")).unwrap();
        store
            .transaction(|db| {
                db.query("CREATE (:Project {id: 'r', workspace_id: 'w', project_key: 'r'})")?;
                db.query("CREATE (:Secret {id: 'sec', content: 'ciphertext'})")?;
                db.query("CREATE (:Session {id: 'sess', workspace_id: 'w'})")?;
                db.query("CREATE (:Connector {id: 'c', repository_id: 'r', credential_env: 'secret:token', state: 'active'})")?;
                db.query("CREATE (:Job {id: 'j', repository_id: 'r', kind: 'project', input_hash: 'h', state: 'queued'})")?;
                db.query("MATCH (p:Project {id: 'r'}) CREATE (:Memory {id: 'm', title: 'kept'})-[:OF]->(p)")?;
                Ok(())
            })
            .unwrap();
        let backup = directory.path().join("backup");
        let result = backup_graph(&store, &backup).unwrap();
        assert_eq!(result["integrity"], json!("ok"));
        let copy = GraphStore::open(&backup).unwrap();
        copy.read(|db| {
            let secrets = one(db, "MATCH (s:Secret) RETURN count(s) AS n", Params::new())?.unwrap();
            assert_eq!(secrets["n"], json!(0));
            let sessions = one(db, "MATCH (s:Session) RETURN count(s) AS n", Params::new())?.unwrap();
            assert_eq!(sessions["n"], json!(0));
            let connector = one(db, "MATCH (c:Connector {id: 'c'}) RETURN c.state AS state, c.generation AS generation", Params::new())?.unwrap();
            assert_eq!(connector["state"], json!("paused"));
            assert_eq!(connector["generation"], json!(1));
            let job = one(db, "MATCH (j:Job {id: 'j'}) RETURN j.state AS state", Params::new())?.unwrap();
            assert_eq!(job["state"], json!("cancelled"));
            let memory = one(db, "MATCH (m:Memory {id: 'm'}) RETURN m.title AS title", Params::new())?.unwrap();
            assert_eq!(memory["title"], json!("kept"));
            Ok(())
        })
        .unwrap();
        // The live store is untouched.
        store
            .read(|db| {
                let secrets = one(db, "MATCH (s:Secret) RETURN count(s) AS n", Params::new())?.unwrap();
                assert_eq!(secrets["n"], json!(1));
                let job = one(db, "MATCH (j:Job {id: 'j'}) RETURN j.state AS state", Params::new())?.unwrap();
                assert_eq!(job["state"], json!("queued"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn an_in_memory_store_has_no_files_to_back_up() {
        let store = GraphStore::open_in_memory().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let error = backup_graph(&store, &directory.path().join("backup")).unwrap_err();
        assert!(error.message.contains("in-memory"), "{error}");
    }
}
