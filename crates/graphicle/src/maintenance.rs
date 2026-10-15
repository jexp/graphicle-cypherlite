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
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path};

/// The graph store of this data directory (`<dir>/reviews.graph`), opened
/// beside the SQLite store while the migration runs (docs/store-api.md:
/// "beside, not instead of"). Opening applies the schema idempotently.
pub(crate) fn graph_store(store: &Store) -> Result<GraphStore> {
    GraphStore::open(store.directory.join("reviews.graph"))
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
            ("data", data),
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
fn delete_project(
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
        "MATCH (n) WHERE n.repository_id = $repo DETACH DELETE n",
        params(&[("repo", json!(identifier))]),
    )?;
    db.query_with_params(
        "MATCH (n)-[:OF|IN_PROJECT|PART_OF]->(:Project {id: $repo}) DETACH DELETE n",
        params(&[("repo", json!(identifier))]),
    )?;
    db.query_with_params(
        "MATCH (p:Project {id: $repo}) DETACH DELETE p",
        params(&[("repo", json!(identifier))]),
    )?;
    graphicle_memory::cache::clear(identifier).map_err(Error::bad)?;
    deletion::project_receipt(db, p, identifier, &connectors, removed)
}

/// `retention.apply` with `sources`: forget the documents the policy selects
/// through certified deletion (plan, forget, erase, verify), one receipt per
/// batch of up to 1,000 documents, each carrying the policy. The caller must
/// echo the `plan_hash` of `retention.preview`, so exactly the previewed
/// documents are forgotten.
fn apply_source_retention(
    db: &mut cypherlite::Connection,
    sql: &rusqlite::Transaction<'_>,
    p: &store::Principal,
    repository: &Value,
    args: &Value,
) -> Result<Value> {
    let identifier = s(repository, "id");
    let policy = crate::retention::SourcePolicy::from_args(args)?;
    let plan = crate::retention::source_plan(db, p, repository, &policy, chrono::Utc::now())?;
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
            let forget_plan = deletion::plan(db, &p.workspace, identifier, &scope)?;
            if forget_plan.sources() == 0 {
                continue;
            }
            crate::projection::enqueue(sql, identifier, "source_forgotten")?;
            let since = forget_plan.since().map(str::to_string);
            let forgotten =
                deletion::forget(db, &p.workspace, identifier, &scope, since.as_deref())?;
            let erased = deletion::erase(db, identifier, &scope)?;
            graphicle_memory::cache::clear(identifier).map_err(Error::bad)?;
            event(
                db,
                identifier,
                "source_forgotten",
                json!({"derived_records_removed":forgotten["memories"],"retention":true}),
            )?;
            forgotten_documents += batch.len();
            receipts.push(deletion::policy_receipt(
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

/// `storage.prune`: bound the content store to the newest `keep_snapshots`
/// (plus the current and run-referenced ones), drop unreferenced objects,
/// finished syncs and unreferenced original documents, and sign a receipt
/// over what was pruned.
fn storage_prune(
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
        // ConnectorDoc metadata is flattened onto dotted properties
        // (`d.`metadata.provider_metadata.original_sha256``; verified on
        // v0.12.2 — map-typed properties are rejected, dotted names are not).
        let current = one(
            db,
            "MATCH (d:ConnectorDoc {repository_id: $repo, id: $source_id, present: true}) \
             WHERE d.`metadata.provider_metadata.original_sha256` = $sha \
             RETURN d.id AS id LIMIT 1",
            params(&[
                ("repo", json!(identifier)),
                ("source_id", json!(source_id)),
                ("sha", json!(sha)),
            ]),
        )?
        .is_some();
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
                    let present = graph.read(|g| one(g,"MATCH (c:Connector {repository_id: $repo, id: $id}) RETURN c.id AS id",params(&[("repo",json!(identifier)),("id",json!(connector))])))?.is_some();
                    if !present {return Err(Error::bad("Connector unavailable"));}
                    if operation!="connector.state" {
                        let scope=if operation=="connector.delete" {Scope::connector(connector)} else {Scope::documents(connector,&args["document_ids"])?};
                        // Capture coverage first; forget, erase, verify and sign in one transaction.
                        return graph.transaction(|g| {
                            let started=now();
                            let plan=deletion::plan(g,&p.workspace,&identifier,&scope)?;
                            if operation=="source.forget" && plan.sources()==0 {return Err(Error::bad("None of these documents exist in this connector"));}
                            crate::projection::enqueue(db,&identifier,if operation=="connector.delete" {"connector_deleted"} else {"source_forgotten"})?;
                            let since=plan.since().map(str::to_string);
                            let forgotten=deletion::forget(g,&p.workspace,&identifier,&scope,since.as_deref())?;
                            let erased=deletion::erase(g,&identifier,&scope)?;
                            // In-process features and layouts of forgotten text go now, not at expiry.
                            graphicle_memory::cache::clear(&identifier).map_err(Error::bad)?;
                            let derived=forgotten["memories"].clone();
                            event(g,&identifier,"source_forgotten",json!({"derived_records_removed":derived}))?;
                            let receipt=deletion::receipt(g,p,&repository,operation,plan,&started,forgotten,erased)?;
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
                "storage.prune"=>graph.transaction(|g| storage_prune(g,p,&repository,args)),
                "retention.apply"=>{
                    if args.get("sources").is_some_and(|v|!v.is_null()) {
                        return graph.transaction(|g| apply_source_retention(g,db,p,&repository,args));
                    }
                    let retention=crate::retention::Retention::from_args(args)?;
                    graph.transaction(|g| {
                        let mut result=crate::retention::apply(g,&identifier,&retention)?;
                        let removed=result["deleted"].clone();
                        if removed.as_object().is_some_and(|counts|counts.values().any(|n|n.as_u64().unwrap_or(0)>0)) {
                            let remaining=crate::retention::remaining(g,&identifier,&retention)?;
                            result["receipt"]=deletion::rows_receipt(g,p,&identifier,"retention.graph","graph_by_products",json!(retention),removed,&[],remaining,false)?;
                        }
                        Ok(result)
                    })
                },
                _=>Err(Error::bad("Unknown maintenance operation"))
            }
        })?;
        if result.get("receipt").is_some() {
            // After commit, flush the deleted nodes' old images into a
            // snapshot so the write-ahead log no longer carries them.
            graph.checkpoint()?;
            result["storage"] = json!({"snapshot_checkpoint":"ok"});
        }
        Ok(result)
    }

    /// A portable backup of the graph store at `path` (a directory the call
    /// creates): checkpoint the live store, copy the snapshot files, then
    /// scrub the copy — provider credentials, sessions and entitlements
    /// never leave the device. The scrubbed copy is checkpointed again so
    /// the removed data is not carried in its log either.
    pub fn backup(&self, path: &Path) -> Result<Value> {
        let store = graph_store(self)?;
        backup_graph(&store, path)
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
