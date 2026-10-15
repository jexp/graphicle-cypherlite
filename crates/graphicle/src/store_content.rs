//! The `repo_*` content store on the graph (migration plan §2.2): content-
//! addressed objects, file heads, parses, chunks, snapshots, syncs and the
//! event log, plus the sync protocol's batch statements.
//!
//! ```text
//! (:Project)-[:HAS_OBJECT]->(:Object {repository_id, sha256, content, bytes})
//! (:File {repository_id, path})-[:HEAD]->(:Object)
//! (:File)-[:IN_PROJECT]->(:Project)
//! (:File)-[:PARSED_AS]->(:Parse {repository_id, path, sha256, revision, data})
//! (:Chunk {repository_id, path, sha256, line, end_line, index_text})
//!   -[:PART_OF]->(:Object)
//! (:Snapshot {repository_id, sha256, manifest, metadata, created_at})-[:OF]->(:Project)
//! (:Sync {id, repository_id, expected_snapshot, snapshot, manifest, metadata, state, …})
//!   -[:OF]->(:Project)
//! (:Event {repository_id, seq, kind, data, created_at})-[:OF]->(:Project)
//! ```
//!
//! Every function keeps the module boundary's ergonomics: `serde_json::Value`
//! in/out, per [`crate::store_graph`]. Writes that batch are one `UNWIND
//! $rows` statement per entity kind inside a single
//! [`GraphStore::transaction`] — the per-file `SELECT EXISTS` loops of the
//! SQL store are gone. Chunk text is searchable through the `chunk_text`
//! fulltext index ([`search_chunks`]); the index maintains itself on writes.
//!
//! The SQL `repo_*` tables stay until the cleanup node removes rusqlite;
//! modules outside this domain keep reading them while their own nodes
//! migrate.
use crate::store_graph::{GraphStore, Params, json_to_value, one, rows, s};
use crate::{Result, now};
use cypherlite::Connection;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Build query parameters from `(name, json)` pairs.
fn params(pairs: Vec<(&str, Value)>) -> Params {
    pairs
        .into_iter()
        .map(|(name, value)| (name.to_string(), json_to_value(&value)))
        .collect()
}

/// The `Project` node the content of `repository` hangs off, created on
/// first use. Projects are registered through the SQL ownership spine; the
/// graph node carries the same identity properties so the spine's own node
/// can `MERGE` onto it later. All three key-ish properties go into the
/// `MERGE` pattern: the graph type declares `workspace_id`/`project_key`
/// `NOT NULL`, so a bare `MERGE (p:Project {id: …})` would not commit.
fn ensure_project(db: &mut Connection, repository: &Value) -> Result<()> {
    rows(
        db,
        "MERGE (p:Project {id:$id, workspace_id:$workspace, project_key:$key}) \
         ON CREATE SET p.name=$name RETURN p.id AS id",
        params(vec![
            ("id", repository["id"].clone()),
            ("workspace", repository["workspace_id"].clone()),
            ("key", repository["project_key"].clone()),
            ("name", repository["name"].clone()),
        ]),
    )?;
    Ok(())
}

/// The paths of `entries` (`[{path, sha256, bytes}]`) whose content the
/// store does not hold: one `UNWIND` round trip, replacing the SQL store's
/// per-file `SELECT EXISTS` loop.
pub fn missing_objects(store: &GraphStore, repository: &str, entries: &Value) -> Result<Vec<String>> {
    let shas: Vec<&str> = entries
        .as_array()
        .map(|rows| rows.iter().map(|row| s(row, "sha256")).collect())
        .unwrap_or_default();
    // One scan filtered by the IN list: per-row MATCH/MERGE lookups are
    // superlinear in the store's size on this engine (v0.12.2); one scan is
    // linear. The diff is computed in Rust.
    let held: std::collections::HashMap<String, i64> = store.read(|db| {
        Ok(rows(
            db,
            "MATCH (o:Object {repository_id:$repo}) WHERE o.sha256 IN $shas \
             RETURN o.sha256 AS sha256, o.bytes AS bytes",
            params(vec![("repo", json!(repository)), ("shas", json!(shas))]),
        )?
        .into_iter()
        .map(|row| (s(&row, "sha256").to_string(), row["bytes"].as_i64().unwrap_or(-1)))
        .collect())
    })?;
    let mut missing = Vec::new();
    for row in entries.as_array().into_iter().flatten() {
        if held.get(s(row, "sha256")) != row["bytes"].as_i64().as_ref() {
            missing.push(s(row, "path").to_string());
        }
    }
    Ok(missing)
}

/// Store `rows` (`[{sha256, content, bytes}]`) as objects of `repository`
/// (the SQL project row, for [`ensure_project`]). Content-addressed: an
/// object already present is untouched (`INSERT OR IGNORE` semantics).
pub fn put_objects(store: &GraphStore, repository: &Value, rows_json: &Value) -> Result<()> {
    let repo = s(repository, "id").to_string();
    store.transaction(|db| {
        ensure_project(db, repository)?;
        // Create only the objects the scan does not find; a fresh object
        // gets its HAS_OBJECT relationship in the same CREATE (no MERGE —
        // MERGE and relationship existence checks are superlinear here).
        let existing = existing_object_shas(db, &repo, rows_json)?;
        let missing: Vec<Value> = rows_json
            .as_array()
            .into_iter()
            .flatten()
            .filter(|row| !existing.contains(s(row, "sha256")))
            .cloned()
            .collect();
        if !missing.is_empty() {
            rows(
                db,
                "MATCH (p:Project {id:$repo}) \
                 WITH p UNWIND $rows AS row \
                 CREATE (p)-[:HAS_OBJECT]->(o:Object {repository_id:$repo, sha256:row.sha256, content:row.content, bytes:row.bytes})",
                params(vec![("repo", json!(repo)), ("rows", json!(missing))]),
            )?;
        }
        Ok(())
    })
}

/// The sha256s of `rows_json` (`[{sha256, …}]`) already stored — one scan.
fn existing_object_shas(
    db: &mut Connection,
    repo: &str,
    rows_json: &Value,
) -> Result<std::collections::BTreeSet<String>> {
    let shas: Vec<&str> = rows_json
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| s(row, "sha256"))
        .collect();
    Ok(rows(
        db,
        "MATCH (o:Object {repository_id:$repo}) WHERE o.sha256 IN $shas RETURN o.sha256 AS sha256",
        params(vec![("repo", json!(repo)), ("shas", json!(shas))]),
    )?
    .into_iter()
    .map(|row| s(&row, "sha256").to_string())
    .collect())
}

/// Total retained object bytes of `repository` (all versions).
pub fn retained_bytes(store: &GraphStore, repository: &str) -> Result<i64> {
    store.read(|db| {
        Ok(one(
            db,
            "MATCH (o:Object {repository_id:$repo}) RETURN coalesce(sum(o.bytes),0) AS bytes",
            params(vec![("repo", json!(repository))]),
        )?
        .and_then(|row| row["bytes"].as_i64())
        .unwrap_or(0))
    })
}

/// The contents of `shas`, in the same order, `None` where an object is
/// absent. One statement per call; callers chunk their batches.
pub fn object_contents(
    store: &GraphStore,
    repository: &str,
    shas: &[String],
) -> Result<Vec<Option<String>>> {
    let found: std::collections::HashMap<String, String> = store.read(|db| {
        Ok(rows(
            db,
            "MATCH (o:Object {repository_id:$repo}) WHERE o.sha256 IN $shas \
             RETURN o.sha256 AS sha256, o.content AS content",
            params(vec![("repo", json!(repository)), ("shas", json!(shas))]),
        )?
        .into_iter()
        .map(|row| (s(&row, "sha256").to_string(), s(&row, "content").to_string()))
        .collect())
    })?;
    Ok(shas.iter().map(|sha| found.get(sha).cloned()).collect())
}

/// The commit plan of a sync's manifest (`[{path, sha256}]`): per entry
/// whether the object is present, whether a current-revision parse exists
/// and whether chunks exist — one `UNWIND` statement, replacing the SQL
/// store's three per-file `SELECT EXISTS` loops.
pub fn commit_plan(
    store: &GraphStore,
    repository: &str,
    entries: &Value,
    revisions: [&str; 2],
) -> Result<Vec<Value>> {
    let rows_in: Vec<&Value> = entries.as_array().into_iter().flatten().collect();
    let shas: Vec<&str> = rows_in.iter().map(|row| s(row, "sha256")).collect();
    let paths: Vec<&str> = rows_in.iter().map(|row| s(row, "path")).collect();
    // Three scans filtered by IN lists; per-row OPTIONAL MATCH / EXISTS
    // lookups are superlinear in the store's size on this engine. The join
    // runs in Rust.
    let (objects, parses, chunked) = store.read(|db| {
        let objects: std::collections::BTreeSet<String> = rows(
            db,
            "MATCH (o:Object {repository_id:$repo}) WHERE o.sha256 IN $shas RETURN o.sha256 AS sha256",
            params(vec![("repo", json!(repository)), ("shas", json!(shas))]),
        )?
        .into_iter()
        .map(|row| s(&row, "sha256").to_string())
        .collect();
        let parses: std::collections::HashMap<(String, String), String> = rows(
            db,
            "MATCH (p:Parse {repository_id:$repo}) WHERE p.path IN $paths \
             RETURN p.path AS path, p.sha256 AS sha256, p.revision AS revision",
            params(vec![("repo", json!(repository)), ("paths", json!(paths.clone()))]),
        )?
        .into_iter()
        .map(|row| {
            (
                (s(&row, "path").to_string(), s(&row, "sha256").to_string()),
                s(&row, "revision").to_string(),
            )
        })
        .collect();
        let chunked: std::collections::BTreeSet<(String, String)> = rows(
            db,
            "MATCH (c:Chunk {repository_id:$repo}) WHERE c.path IN $paths \
             RETURN DISTINCT c.path AS path, c.sha256 AS sha256",
            params(vec![("repo", json!(repository)), ("paths", json!(paths))]),
        )?
        .into_iter()
        .map(|row| (s(&row, "path").to_string(), s(&row, "sha256").to_string()))
        .collect();
        Ok((objects, parses, chunked))
    })?;
    Ok(rows_in
        .iter()
        .map(|row| {
            let path = s(row, "path").to_string();
            let sha = s(row, "sha256").to_string();
            let parsed = parses
                .get(&(path.clone(), sha.clone()))
                .is_some_and(|revision| revisions.contains(&revision.as_str()));
            json!({"path":path,"sha256":sha,"present":objects.contains(&sha),
                   "parsed":parsed,"chunked":chunked.contains(&(path, sha))})
        })
        .collect())
}

/// Store parse rows (`[{path, sha256, revision, data}]`; `data` is the
/// analysis as its JSON text — the engine has no map-typed property values,
/// so this payload stays stringified). `INSERT OR REPLACE` semantics:
/// a newer revision overwrites. The `File` node and its `PARSED_AS`/
/// `IN_PROJECT` relationships are merged alongside.
pub fn put_parses(store: &GraphStore, repository: &Value, rows_json: &Value) -> Result<()> {
    let repo = s(repository, "id").to_string();
    store.transaction(|db| {
        ensure_project(db, repository)?;
        let paths = ensure_files(db, &repo, rows_json)?;
        // `INSERT OR REPLACE`: SET the parses one scan found; CREATE the
        // rest with their PARSED_AS relationship (the File's id is bound in
        // the row — id() lookups are the engine's fast path).
        let existing = existing_parse_keys(db, &repo, &paths)?;
        let mut updates = Vec::new();
        let mut creates = Vec::new();
        for row in rows_json.as_array().into_iter().flatten() {
            if existing.contains(&(s(row, "path").to_string(), s(row, "sha256").to_string())) {
                updates.push(row.clone());
            } else {
                creates.push(row.clone());
            }
        }
        if !updates.is_empty() {
            rows(
                db,
                "UNWIND $rows AS row \
                 MATCH (p:Parse {repository_id:$repo, path:row.path, sha256:row.sha256}) \
                 SET p.revision=row.revision, p.data=row.data",
                params(vec![("repo", json!(repo)), ("rows", json!(updates))]),
            )?;
        }
        if !creates.is_empty() {
            let files = file_ids(db, &repo, &paths)?;
            let mut with_files = Vec::new();
            for mut row in creates {
                let file = files.get(s(&row, "path")).copied().unwrap_or(-1);
                row["file_id"] = json!(file);
                with_files.push(row);
            }
            rows(
                db,
                "UNWIND $rows AS row \
                 MATCH (f:File) WHERE id(f)=row.file_id \
                 CREATE (f)-[:PARSED_AS]->(p:Parse {repository_id:$repo, path:row.path, sha256:row.sha256, revision:row.revision, data:row.data})",
                params(vec![("repo", json!(repo)), ("rows", json!(with_files))]),
            )?;
        }
        Ok(())
    })
}

/// Ensure a `File` node (with its `IN_PROJECT` relationship) for every
/// distinct `path` in `rows_json`; returns the sorted distinct paths. New
/// files are CREATEd — the existing set comes from one scan.
fn ensure_files(db: &mut Connection, repo: &str, rows_json: &Value) -> Result<Vec<String>> {
    let mut paths: Vec<String> = rows_json
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| s(row, "path").to_string())
        .collect();
    paths.sort();
    paths.dedup();
    let existing = rows(
        db,
        "MATCH (f:File {repository_id:$repo}) WHERE f.path IN $paths RETURN f.path AS path",
        params(vec![("repo", json!(repo)), ("paths", json!(paths.clone()))]),
    )?;
    let have: std::collections::BTreeSet<&str> =
        existing.iter().map(|row| s(row, "path")).collect();
    let missing: Vec<&String> = paths.iter().filter(|path| !have.contains(path.as_str())).collect();
    if !missing.is_empty() {
        rows(
            db,
            "MATCH (p:Project {id:$repo}) \
             WITH p UNWIND $paths AS path \
             CREATE (f:File {repository_id:$repo, path:path})-[:IN_PROJECT]->(p)",
            params(vec![("repo", json!(repo)), ("paths", json!(missing))]),
        )?;
    }
    Ok(paths)
}

/// `(path, sha256)` of the parses already stored for `paths` — one scan.
fn existing_parse_keys(
    db: &mut Connection,
    repo: &str,
    paths: &[String],
) -> Result<std::collections::BTreeSet<(String, String)>> {
    Ok(rows(
        db,
        "MATCH (p:Parse {repository_id:$repo}) WHERE p.path IN $paths \
         RETURN p.path AS path, p.sha256 AS sha256",
        params(vec![("repo", json!(repo)), ("paths", json!(paths))]),
    )?
    .into_iter()
    .map(|row| (s(&row, "path").to_string(), s(&row, "sha256").to_string()))
    .collect())
}

/// path → engine id of the `File` nodes at `paths` — one scan; `id()` is
/// the fast binding for the relationship CREATEs that follow.
fn file_ids(
    db: &mut Connection,
    repo: &str,
    paths: &[String],
) -> Result<std::collections::HashMap<String, i64>> {
    Ok(rows(
        db,
        "MATCH (f:File {repository_id:$repo}) WHERE f.path IN $paths \
         RETURN f.path AS path, id(f) AS id",
        params(vec![("repo", json!(repo)), ("paths", json!(paths))]),
    )?
    .into_iter()
    .map(|row| (s(&row, "path").to_string(), row["id"].as_i64().unwrap_or(-1)))
    .collect())
}

/// Store chunk rows (`[{path, sha256, line, end_line, index_text}]`).
/// `INSERT OR IGNORE` semantics: an existing chunk keeps its first text.
/// The `chunk_text` fulltext index maintains itself on these writes.
pub fn put_chunks(store: &GraphStore, repository: &Value, rows_json: &Value) -> Result<()> {
    let repo = s(repository, "id").to_string();
    store.transaction(|db| {
        ensure_project(db, repository)?;
        // `INSERT OR IGNORE`: one scan lists the chunk keys already stored
        // for these paths; only the missing lines are CREATEd, bound to
        // their object by engine id.
        let paths: Vec<String> = {
            let mut paths: Vec<String> = rows_json
                .as_array()
                .into_iter()
                .flatten()
                .map(|row| s(row, "path").to_string())
                .collect();
            paths.sort();
            paths.dedup();
            paths
        };
        let existing: std::collections::BTreeSet<(String, String, i64)> = rows(
            db,
            "MATCH (c:Chunk {repository_id:$repo}) WHERE c.path IN $paths \
             RETURN c.path AS path, c.sha256 AS sha256, c.line AS line",
            params(vec![("repo", json!(repo)), ("paths", json!(paths))]),
        )?
        .into_iter()
        .map(|row| {
            (
                s(&row, "path").to_string(),
                s(&row, "sha256").to_string(),
                row["line"].as_i64().unwrap_or(-1),
            )
        })
        .collect();
        let mut creates = Vec::new();
        for row in rows_json.as_array().into_iter().flatten() {
            let key = (
                s(row, "path").to_string(),
                s(row, "sha256").to_string(),
                row["line"].as_i64().unwrap_or(-1),
            );
            if !existing.contains(&key) {
                creates.push(row.clone());
            }
        }
        if creates.is_empty() {
            return Ok(());
        }
        let shas: Vec<&str> = creates.iter().map(|row| s(row, "sha256")).collect();
        let objects: std::collections::HashMap<String, i64> = rows(
            db,
            "MATCH (o:Object {repository_id:$repo}) WHERE o.sha256 IN $shas \
             RETURN o.sha256 AS sha256, id(o) AS id",
            params(vec![("repo", json!(repo)), ("shas", json!(shas))]),
        )?
        .into_iter()
        .map(|row| (s(&row, "sha256").to_string(), row["id"].as_i64().unwrap_or(-1)))
        .collect();
        let mut with_objects = Vec::new();
        for mut row in creates {
            let object = objects.get(s(&row, "sha256")).copied().unwrap_or(-1);
            row["object_id"] = json!(object);
            with_objects.push(row);
        }
        rows(
            db,
            "UNWIND $rows AS row \
             MATCH (o:Object) WHERE id(o)=row.object_id \
             CREATE (c:Chunk {repository_id:$repo, path:row.path, sha256:row.sha256, line:row.line, end_line:row.end_line, index_text:row.index_text})-[:PART_OF]->(o)",
            params(vec![("repo", json!(repo)), ("rows", json!(with_objects))]),
        )?;
        Ok(())
    })
}

/// The chunk start lines of the given `(path, sha256)` file versions — one
/// scan for the whole set a commit reports on.
pub fn chunk_lines_for(
    store: &GraphStore,
    repository: &str,
    files: &[(String, String)],
) -> Result<std::collections::BTreeMap<(String, String), Vec<i64>>> {
    let paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
    let found = store.read(|db| {
        rows(
            db,
            "MATCH (c:Chunk {repository_id:$repo}) WHERE c.path IN $paths \
             RETURN c.path AS path, c.sha256 AS sha256, c.line AS line",
            params(vec![("repo", json!(repository)), ("paths", json!(paths))]),
        )
    })?;
    let wanted: std::collections::BTreeSet<&(String, String)> = files.iter().collect();
    let mut lines: std::collections::BTreeMap<(String, String), Vec<i64>> = std::collections::BTreeMap::new();
    for row in found {
        let key = (s(&row, "path").to_string(), s(&row, "sha256").to_string());
        if wanted.contains(&key) {
            lines.entry(key).or_default().push(row["line"].as_i64().unwrap_or(0));
        }
    }
    for lines in lines.values_mut() {
        lines.sort_unstable();
    }
    Ok(lines)
}

/// The current heads of `repository`: path → sha256.
pub fn heads(store: &GraphStore, repository: &str) -> Result<BTreeMap<String, String>> {
    store.read(|db| {
        Ok(rows(
            db,
            "MATCH (f:File {repository_id:$repo})-[:HEAD]->(o:Object) \
             RETURN f.path AS path, o.sha256 AS sha256",
            params(vec![("repo", json!(repository))]),
        )?
        .iter()
        .map(|row| (s(row, "path").to_string(), s(row, "sha256").to_string()))
        .collect())
    })
}

/// Set the head of one file (`INSERT OR REPLACE` semantics), creating the
/// object reference's `File` node when new. Test and fixture tooling; the
/// sync protocol moves heads with [`activate`].
pub fn put_head(store: &GraphStore, repository: &Value, path: &str, sha256: &str) -> Result<()> {
    let repo = s(repository, "id").to_string();
    store.transaction(|db| {
        ensure_project(db, repository)?;
        rows(
            db,
            "MERGE (f:File {repository_id:$repo, path:$path}) \
             WITH f \
             OPTIONAL MATCH (f)-[old:HEAD]->() DELETE old",
            params(vec![("repo", json!(repo)), ("path", json!(path))]),
        )?;
        rows(
            db,
            "MATCH (f:File {repository_id:$repo, path:$path}) \
             MATCH (o:Object {repository_id:$repo, sha256:$sha}) \
             MERGE (f)-[:HEAD]->(o) \
             WITH f \
             MATCH (p:Project {id:$repo}) \
             MERGE (f)-[:IN_PROJECT]->(p)",
            params(vec![
                ("repo", json!(repo)),
                ("path", json!(path)),
                ("sha", json!(sha256)),
            ]),
        )?;
        Ok(())
    })
}

/// How many of `repository`'s current heads lack an analysis at a current
/// revision (`NULL` or an outdated one). One statement.
pub fn outdated_heads(store: &GraphStore, repository: &str, revisions: [&str; 2]) -> Result<i64> {
    store.read(|db| {
        Ok(one(
            db,
            "MATCH (f:File {repository_id:$repo})-[:HEAD]->(o:Object) \
             OPTIONAL MATCH (f)-[:PARSED_AS]->(p:Parse) WHERE p.sha256 = o.sha256 \
             RETURN count(CASE WHEN p IS NULL OR NOT (p.revision IN $revisions) THEN 1 END) AS n",
            params(vec![("repo", json!(repository)), ("revisions", json!(revisions))]),
        )?
        .and_then(|row| row["n"].as_i64())
        .unwrap_or(0))
    })
}

/// The files of `repository`'s current heads whose structure analysis was
/// limited (`[{path, limit, reason}]`) or skipped (`[{path, reason}]`), in
/// path order — the graph counterpart of the SQL store's `analysis_report`.
pub fn analysis_report(store: &GraphStore, repository: &str) -> Result<(Vec<Value>, Vec<Value>)> {
    let found = store.read(|db| {
        rows(
            db,
            "MATCH (f:File {repository_id:$repo})-[:HEAD]->(o:Object) \
             MATCH (f)-[:PARSED_AS]->(p:Parse) \
             WHERE p.sha256 = o.sha256 AND p.revision <> $revision \
             RETURN f.path AS path, p.data AS data ORDER BY f.path",
            params(vec![
                ("repo", json!(repository)),
                ("revision", json!(graphicle_memory::structure::REVISION)),
            ]),
        )
    })?;
    let (mut limited, mut skipped) = (Vec::new(), Vec::new());
    for row in found {
        let analysis: Value = serde_json::from_str(s(&row, "data"))?;
        if analysis.get("skipped").is_some() {
            skipped.push(json!({"path":row["path"],"reason":analysis["issues"][0]}));
        } else if let Some(limit) = analysis["limited"].as_str() {
            let reason = graphicle_memory::structure::limit_issues(&analysis).join("; ");
            limited.push(json!({"path":row["path"],"limit":limit,"reason":reason}));
        }
    }
    Ok((limited, skipped))
}

/// Chunk text search through the `chunk_text` fulltext index, scoped to
/// `repository`: `[{path, sha256, line, end_line}]` of the best `limit`
/// chunks matching `query`.
pub fn search_chunks(
    store: &GraphStore,
    repository: &str,
    query: &str,
    limit: u32,
) -> Result<Vec<Value>> {
    store.read(|db| {
        rows(
            db,
            "MATCH (c:Chunk {repository_id:$repo}) \
             SEARCH c IN (FULLTEXT INDEX chunk_text FOR $q LIMIT $k) \
             RETURN c.path AS path, c.sha256 AS sha256, c.line AS line, c.end_line AS end_line",
            params(vec![
                ("repo", json!(repository)),
                ("q", json!(query)),
                ("k", json!(limit)),
            ]),
        )
    })
}

/// A sync row as the SQL store's `SELECT * FROM repo_syncs` shaped it, or
/// `None`.
pub fn sync_get(store: &GraphStore, repository: &str, sync_id: &str) -> Result<Option<Value>> {
    store.read(|db| {
        one(
            db,
            "MATCH (s:Sync {repository_id:$repo, id:$id}) \
             RETURN s.id AS id, s.repository_id AS repository_id, \
                    s.expected_snapshot AS expected_snapshot, s.snapshot AS snapshot, \
                    s.manifest AS manifest, s.metadata AS metadata, \
                    s.state AS state, s.created_at AS created_at",
            params(vec![("repo", json!(repository)), ("id", json!(sync_id))]),
        )
    })
}

/// A sync's state, or `None` when no such sync exists.
pub fn sync_state(store: &GraphStore, repository: &str, sync_id: &str) -> Result<Option<String>> {
    Ok(sync_get(store, repository, sync_id)?
        .map(|row| s(&row, "state").to_string()))
}

/// Syncs of `repository` still waiting for their manifest or content.
pub fn syncs_unfinished(store: &GraphStore, repository: &str) -> Result<i64> {
    store.read(|db| {
        Ok(one(
            db,
            "MATCH (s:Sync {repository_id:$repo}) \
             WHERE s.state IN ['pending','preparing'] \
             RETURN count(s) AS n",
            params(vec![("repo", json!(repository))]),
        )?
        .and_then(|row| row["n"].as_i64())
        .unwrap_or(0))
    })
}

/// Start a sync. `fields` carries `expected_snapshot`, `snapshot`,
/// `manifest`, `metadata` and `state`; `created_at` is stamped here.
pub fn sync_create(
    store: &GraphStore,
    repository: &Value,
    sync_id: &str,
    fields: &Value,
) -> Result<()> {
    let repo = s(repository, "id").to_string();
    store.transaction(|db| {
        ensure_project(db, repository)?;
        rows(
            db,
            "MATCH (p:Project {id:$repo}) \
             CREATE (p)-[:OF]->(s:Sync {id:$id, repository_id:$repo, \
                 expected_snapshot:$expected, snapshot:$snapshot, manifest:$manifest, \
                 metadata:$metadata, state:$state, created_at:$at})",
            params(vec![
                ("repo", json!(repo)),
                ("id", json!(sync_id)),
                ("expected", fields["expected_snapshot"].clone()),
                ("snapshot", fields["snapshot"].clone()),
                ("manifest", fields["manifest"].clone()),
                ("metadata", fields["metadata"].clone()),
                ("state", fields["state"].clone()),
                ("at", json!(now())),
            ]),
        )?;
        Ok(())
    })
}

/// Update a sync's properties (`{"manifest": …}`, `{"state": "pending"}` …).
pub fn sync_update(store: &GraphStore, sync_id: &str, updates: &Value) -> Result<()> {
    store.transaction(|db| {
        rows(
            db,
            "MATCH (s:Sync {id:$id}) SET s += $updates",
            params(vec![("id", json!(sync_id)), ("updates", updates.clone())]),
        )?;
        Ok(())
    })
}

/// Drop an unfinished sync (a replayed prepare discards its partial parts).
pub fn sync_delete(store: &GraphStore, sync_id: &str) -> Result<()> {
    store.transaction(|db| {
        rows(
            db,
            "MATCH (s:Sync {id:$id}) DETACH DELETE s",
            params(vec![("id", json!(sync_id))]),
        )?;
        Ok(())
    })
}

/// `sync.abort`: mark a waiting sync aborted. Pending content already
/// uploaded stays as objects; the sync never commits.
pub fn sync_abort(store: &GraphStore, repository: &str, sync_id: &str) -> Result<()> {
    store.transaction(|db| {
        rows(
            db,
            "MATCH (s:Sync {repository_id:$repo, id:$id}) \
             WHERE s.state IN ['pending','preparing'] \
             SET s.state='aborted'",
            params(vec![("repo", json!(repository)), ("id", json!(sync_id))]),
        )?;
        Ok(())
    })
}

/// The snapshots of `repository`, newest first, `limit` at most: the
/// `history` read's `[{sha256, metadata, created_at}]`.
pub fn snapshots(store: &GraphStore, repository: &str, limit: u32) -> Result<Vec<Value>> {
    store.read(|db| {
        rows(
            db,
            "MATCH (s:Snapshot {repository_id:$repo}) \
             RETURN s.sha256 AS sha256, s.metadata AS metadata, s.created_at AS created_at \
             ORDER BY s.created_at DESC, s.sha256 LIMIT $limit",
            params(vec![("repo", json!(repository)), ("limit", json!(limit))]),
        )
    })
}

/// How many files `repository`'s heads name.
pub fn file_count(store: &GraphStore, repository: &str) -> Result<i64> {
    store.read(|db| {
        Ok(one(
            db,
            "MATCH (f:File {repository_id:$repo})-[:HEAD]->() RETURN count(f) AS n",
            params(vec![("repo", json!(repository))]),
        )?
        .and_then(|row| row["n"].as_i64())
        .unwrap_or(0))
    })
}

/// The analyses of `repository`'s current heads (`[{path, data}]`, `data`
/// the analysis JSON text), in path order — the legacy graph's input.
pub fn head_parses(store: &GraphStore, repository: &str) -> Result<Vec<Value>> {
    store.read(|db| {
        rows(
            db,
            "MATCH (f:File {repository_id:$repo})-[:HEAD]->(o:Object) \
             MATCH (f)-[:PARSED_AS]->(p:Parse) \
             WHERE p.sha256 = o.sha256 \
             RETURN f.path AS path, p.data AS data ORDER BY p.path",
            params(vec![("repo", json!(repository))]),
        )
    })
}

/// The documents of `repository`'s current heads (`[{path, sha256,
/// content}]`), in path order — `memory::documents`' file arm.
pub fn head_documents(store: &GraphStore, repository: &str) -> Result<Vec<Value>> {
    store.read(|db| {
        rows(
            db,
            "MATCH (f:File {repository_id:$repo})-[:HEAD]->(o:Object) \
             RETURN f.path AS path, o.sha256 AS sha256, o.content AS content \
             ORDER BY f.path",
            params(vec![("repo", json!(repository))]),
        )
    })
}

/// The head sha256 of one file, if headed.
pub fn head_sha(store: &GraphStore, repository: &str, path: &str) -> Result<Option<String>> {
    store.read(|db| {
        Ok(one(
            db,
            "MATCH (f:File {repository_id:$repo, path:$path})-[:HEAD]->(o:Object) \
             RETURN o.sha256 AS sha256",
            params(vec![("repo", json!(repository)), ("path", json!(path))]),
        )?
        .map(|row| s(&row, "sha256").to_string()))
    })
}

/// One object's content, if present.
pub fn object_content(store: &GraphStore, repository: &str, sha256: &str) -> Result<Option<String>> {
    store.read(|db| {
        Ok(one(
            db,
            "MATCH (o:Object {repository_id:$repo, sha256:$sha}) RETURN o.content AS content",
            params(vec![("repo", json!(repository)), ("sha", json!(sha256))]),
        )?
        .and_then(|row| row["content"].as_str().map(str::to_string)))
    })
}

/// One snapshot's `{manifest, created_at}`, if recorded.
pub fn snapshot_get(store: &GraphStore, repository: &str, sha256: &str) -> Result<Option<Value>> {
    store.read(|db| {
        one(
            db,
            "MATCH (s:Snapshot {repository_id:$repo, sha256:$sha}) \
             RETURN s.manifest AS manifest, s.created_at AS created_at",
            params(vec![("repo", json!(repository)), ("sha", json!(sha256))]),
        )
    })
}

/// Every snapshot manifest of `repository`, newest first (episode replay
/// scans them for a matching head set).
pub fn snapshot_manifests(store: &GraphStore, repository: &str) -> Result<Vec<String>> {
    store.read(|db| {
        Ok(rows(
            db,
            "MATCH (s:Snapshot {repository_id:$repo}) \
             RETURN s.manifest AS manifest ORDER BY s.created_at DESC",
            params(vec![("repo", json!(repository))]),
        )?
        .iter()
        .map(|row| s(row, "manifest").to_string())
        .collect())
    })
}

/// `storage.prune`'s snapshot list: `[{sha256, manifest}]`, newest first.
pub fn prune_snapshot_rows(store: &GraphStore, repository: &str) -> Result<Vec<Value>> {
    store.read(|db| {
        rows(
            db,
            "MATCH (s:Snapshot {repository_id:$repo}) \
             RETURN s.sha256 AS sha256, s.manifest AS manifest \
             ORDER BY s.created_at DESC, s.sha256",
            params(vec![("repo", json!(repository))]),
        )
    })
}

/// Delete one snapshot.
pub fn snapshot_delete(store: &GraphStore, repository: &str, sha256: &str) -> Result<()> {
    store.transaction(|db| {
        rows(
            db,
            "MATCH (s:Snapshot {repository_id:$repo, sha256:$sha}) DETACH DELETE s",
            params(vec![("repo", json!(repository)), ("sha", json!(sha256))]),
        )?;
        Ok(())
    })
}

/// Every object of `repository` as `[{sha256, bytes}]` (`storage.prune`).
pub fn object_list(store: &GraphStore, repository: &str) -> Result<Vec<Value>> {
    store.read(|db| {
        rows(
            db,
            "MATCH (o:Object {repository_id:$repo}) RETURN o.sha256 AS sha256, o.bytes AS bytes",
            params(vec![("repo", json!(repository))]),
        )
    })
}

/// Delete one object with its parses and chunks (the SQL store's
/// `ON DELETE CASCADE` of `repo_parses`/`repo_chunks`; heads are never
/// pruned while referenced, and the `PART_OF`/`HAS_OBJECT` relationships go
/// with the nodes).
pub fn object_delete(store: &GraphStore, repository: &str, sha256: &str) -> Result<()> {
    store.transaction(|db| {
        rows(
            db,
            "MATCH (c:Chunk {repository_id:$repo, sha256:$sha}) DETACH DELETE c",
            params(vec![("repo", json!(repository)), ("sha", json!(sha256))]),
        )?;
        rows(
            db,
            "MATCH (p:Parse {repository_id:$repo, sha256:$sha}) DETACH DELETE p",
            params(vec![("repo", json!(repository)), ("sha", json!(sha256))]),
        )?;
        rows(
            db,
            "MATCH (o:Object {repository_id:$repo, sha256:$sha}) DETACH DELETE o",
            params(vec![("repo", json!(repository)), ("sha", json!(sha256))]),
        )?;
        Ok(())
    })
}

/// Delete a project's finished syncs (`storage.prune`); returns how many.
pub fn syncs_prune_finished(store: &GraphStore, repository: &str) -> Result<usize> {
    store.transaction(|db| {
        let doomed = rows(
            db,
            "MATCH (s:Sync {repository_id:$repo}) \
             WHERE NOT (s.state IN ['pending','preparing']) \
             RETURN s.id AS id",
            params(vec![("repo", json!(repository))]),
        )?;
        let ids: Vec<&str> = doomed.iter().map(|row| s(row, "id")).collect();
        rows(
            db,
            "UNWIND $ids AS sync_id MATCH (s:Sync {id:sync_id}) DETACH DELETE s",
            params(vec![("ids", json!(ids))]),
        )?;
        Ok(ids.len())
    })
}

/// Whether snapshot `sha256` of `repository` still exists (prune receipts).
pub fn snapshot_count(store: &GraphStore, repository: &str, sha256: &str) -> Result<i64> {
    store.read(|db| {
        Ok(one(
            db,
            "MATCH (s:Snapshot {repository_id:$repo, sha256:$sha}) RETURN count(s) AS n",
            params(vec![("repo", json!(repository)), ("sha", json!(sha256))]),
        )?
        .and_then(|row| row["n"].as_i64())
        .unwrap_or(0))
    })
}

/// Whether object `sha256` of `repository` still exists (prune receipts).
pub fn object_count(store: &GraphStore, repository: &str, sha256: &str) -> Result<i64> {
    store.read(|db| {
        Ok(one(
            db,
            "MATCH (o:Object {repository_id:$repo, sha256:$sha}) RETURN count(o) AS n",
            params(vec![("repo", json!(repository)), ("sha", json!(sha256))]),
        )?
        .and_then(|row| row["n"].as_i64())
        .unwrap_or(0))
    })
}

/// Append one event to the project's log; its sequence comes from the
/// project's counter node (the `AUTOINCREMENT` replacement, plan §6.9).
/// `data` is stored as its JSON text, as the SQL store did.
pub fn append_event(store: &GraphStore, repository: &str, kind: &str, data: &Value) -> Result<()> {
    store.transaction(|db| {
        event_statement(db, repository, kind, &data.to_string())?;
        Ok(())
    })
}

/// The event statement, shared by [`append_event`] and [`activate`] (whose
/// event must commit with the head swap). The sequence counter is keyed by
/// project id, so the event is recorded even for a project whose node no
/// content write has created yet; the `OF` relationship is added when the
/// node exists.
fn event_statement(db: &mut Connection, repository: &str, kind: &str, data: &str) -> Result<()> {
    rows(
        db,
        "MERGE (c:Counter {repository_id:$repo, name:'events'}) \
         ON CREATE SET c.value=0 \
         SET c.value=c.value+1 \
         WITH c.value AS seq \
         CREATE (e:Event {repository_id:$repo, seq:seq, kind:$kind, data:$data, created_at:$at}) \
         WITH e \
         OPTIONAL MATCH (p:Project {id:$repo}) \
         FOREACH (ignored IN CASE WHEN p IS NULL THEN [] ELSE [1] END | CREATE (p)-[:OF]->(e))",
        params(vec![
            ("repo", json!(repository)),
            ("kind", json!(kind)),
            ("data", json!(data)),
            ("at", json!(now())),
        ]),
    )?;
    Ok(())
}

/// Remove every trace of a project from the graph (the content store's part
/// of `project.delete`; the SQL tables cascade on their own until their
/// domains migrate). Everything this module creates carries
/// `repository_id` except the `Project` node itself.
pub fn delete_project(store: &GraphStore, repository: &str) -> Result<()> {
    store.transaction(|db| {
        rows(
            db,
            "MATCH (n {repository_id:$repo}) DETACH DELETE n",
            params(vec![("repo", json!(repository))]),
        )?;
        rows(
            db,
            "MATCH (p:Project {id:$repo}) DETACH DELETE p",
            params(vec![("repo", json!(repository))]),
        )?;
        Ok(())
    })
}

/// The graph node of a freshly registered project. Registration's system of
/// record is still the SQL ownership spine; the graph node exists so content
/// and events have their `Project` from the start (content writes also
/// ensure it, for projects registered before this store existed).
pub fn register_project(store: &GraphStore, repository: &Value) -> Result<()> {
    store.transaction(|db| ensure_project(db, repository))
}

/// Activate a pending sync's snapshot — the commit's write transaction in
/// one graph transaction: the head swap (clear all `HEAD` relationships,
/// merge the manifest's), the snapshot record, the project's snapshot
/// pointer, the sync's completion and the `synced` event (sequence from the
/// project's event counter, replacing `AUTOINCREMENT`).
pub fn activate(
    store: &GraphStore,
    repository: &Value,
    sync: &Value,
    documents: &Value,
    changed: &[String],
    removed: &[String],
) -> Result<()> {
    let repo = s(repository, "id").to_string();
    let snapshot = s(sync, "snapshot").to_string();
    let event = json!({"snapshot":sync["snapshot"],"changed":changed,"removed":removed});
    store.transaction(|db| {
        ensure_project(db, repository)?;
        // The head swap: clear, then CREATE the new HEAD relationships bound
        // by engine ids (MERGE and per-row property MATCH are superlinear in
        // store size on v0.12.2; id() lookup is not).
        rows(
            db,
            "MATCH (f:File {repository_id:$repo})-[h:HEAD]->() DELETE h",
            params(vec![("repo", json!(repo))]),
        )?;
        let paths = ensure_files(db, &repo, documents)?;
        let files = file_ids(db, &repo, &paths)?;
        let shas: Vec<&str> = documents
            .as_array()
            .into_iter()
            .flatten()
            .map(|row| s(row, "sha256"))
            .collect();
        let objects: std::collections::HashMap<String, i64> = rows(
            db,
            "MATCH (o:Object {repository_id:$repo}) WHERE o.sha256 IN $shas \
             RETURN o.sha256 AS sha256, id(o) AS id",
            params(vec![("repo", json!(repo)), ("shas", json!(shas))]),
        )?
        .into_iter()
        .map(|row| (s(&row, "sha256").to_string(), row["id"].as_i64().unwrap_or(-1)))
        .collect();
        let mut pairs = Vec::new();
        for row in documents.as_array().into_iter().flatten() {
            pairs.push(json!({
                "file": files.get(s(row, "path")).copied().unwrap_or(-1),
                "object": objects.get(s(row, "sha256")).copied().unwrap_or(-1),
            }));
        }
        if !pairs.is_empty() {
            rows(
                db,
                "UNWIND $pairs AS pair \
                 MATCH (f:File) WHERE id(f)=pair.file \
                 MATCH (o:Object) WHERE id(o)=pair.object \
                 CREATE (f)-[:HEAD]->(o)",
                params(vec![("pairs", json!(pairs))]),
            )?;
        }
        // The snapshot, once (`INSERT OR IGNORE`): it exists when this exact
        // tree committed before.
        let recorded = one(
            db,
            "MATCH (s:Snapshot {repository_id:$repo, sha256:$snapshot}) RETURN count(s) AS n",
            params(vec![("repo", json!(repo)), ("snapshot", json!(snapshot))]),
        )?
        .and_then(|row| row["n"].as_i64())
        .unwrap_or(0);
        if recorded == 0 {
            rows(
                db,
                "MATCH (p:Project {id:$repo}) \
                 CREATE (p)-[:OF]->(s:Snapshot {repository_id:$repo, sha256:$snapshot, manifest:$manifest, metadata:$metadata, created_at:$at})",
                params(vec![
                    ("repo", json!(repo)),
                    ("snapshot", json!(snapshot)),
                    ("manifest", sync["manifest"].clone()),
                    ("metadata", sync["metadata"].clone()),
                    ("at", json!(now())),
                ]),
            )?;
        }
        rows(
            db,
            "MATCH (p:Project {id:$repo}) \
             SET p.snapshot=$snapshot, p.metadata=$metadata, \
                 p.generation=coalesce(p.generation,0)+1, p.updated_at=$at",
            params(vec![
                ("repo", json!(repo)),
                ("snapshot", json!(snapshot)),
                ("metadata", sync["metadata"].clone()),
                ("at", json!(now())),
            ]),
        )?;
        rows(
            db,
            "MATCH (s:Sync {id:$id}) SET s.state='complete'",
            params(vec![("id", sync["id"].clone())]),
        )?;
        event_statement(db, &repo, "synced", &event.to_string())?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_graph::{Params, json_to_value};

    fn store() -> GraphStore {
        GraphStore::open_in_memory().unwrap()
    }

    fn project() -> Value {
        json!({"id":"proj-1","workspace_id":"ws-1","project_key":"key","name":"Project"})
    }

    fn seeded(store: &GraphStore) {
        let project = project();
        put_objects(
            store,
            &project,
            &json!([
                {"sha256":"sha-a","content":"alpha body","bytes":10},
                {"sha256":"sha-b","content":"beta body","bytes":9},
            ]),
        )
        .unwrap();
    }

    #[test]
    fn objects_round_trip_and_missing() {
        let store = store();
        seeded(&store);
        assert_eq!(retained_bytes(&store, "proj-1").unwrap(), 19);
        assert_eq!(
            missing_objects(
                &store,
                "proj-1",
                &json!([
                    {"path":"a.py","sha256":"sha-a","bytes":10},
                    {"path":"b.py","sha256":"sha-b","bytes":8},
                    {"path":"c.py","sha256":"sha-c","bytes":1},
                ]),
            )
            .unwrap(),
            vec!["b.py".to_string(), "c.py".to_string()]
        );
        assert_eq!(
            object_contents(&store, "proj-1", &["sha-a".into(), "sha-x".into()]).unwrap(),
            vec![Some("alpha body".to_string()), None]
        );
        // Content-addressed: re-putting changes nothing.
        seeded(&store);
        assert_eq!(retained_bytes(&store, "proj-1").unwrap(), 19);
    }

    #[test]
    fn parses_chunks_and_fulltext() {
        let store = store();
        seeded(&store);
        let project = project();
        put_parses(
            &store,
            &project,
            &json!([{"path":"a.py","sha256":"sha-a","revision":"r1","data":"{\"complete\":true}"}]),
        )
        .unwrap();
        put_chunks(
            &store,
            &project,
            &json!([
                {"path":"a.py","sha256":"sha-a","line":1,"end_line":40,"index_text":"alpha rust borrow checker"},
                {"path":"a.py","sha256":"sha-a","line":41,"end_line":80,"index_text":"alpha cargo build"},
            ]),
        )
        .unwrap();
        let plan = commit_plan(
            &store,
            "proj-1",
            &json!([{"path":"a.py","sha256":"sha-a"},{"path":"b.py","sha256":"sha-b"}]),
            ["r1", "r1+limited"],
        )
        .unwrap();
        assert_eq!(plan[0]["present"], true);
        assert_eq!(plan[0]["parsed"], true);
        assert_eq!(plan[0]["chunked"], true);
        assert_eq!(plan[1]["present"], true);
        assert_eq!(plan[1]["parsed"], false, "{plan:?}");
        assert_eq!(plan[1]["chunked"], false, "{plan:?}");
        let lines = chunk_lines_for(&store, "proj-1", &[("a.py".to_string(), "sha-a".to_string())]).unwrap();
        assert_eq!(lines[&("a.py".to_string(), "sha-a".to_string())], vec![1, 41]);
        let found = search_chunks(&store, "proj-1", "borrow", 10).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["line"], 1);
    }

    #[test]
    fn heads_and_activation() {
        let store = store();
        seeded(&store);
        let project = project();
        put_parses(
            &store,
            &project,
            &json!([{"path":"a.py","sha256":"sha-a","revision":"r1","data":"{\"limited\":\"calls\",\"issues\":[\"limited\"]}"}]),
        )
        .unwrap();
        sync_create(
            &store,
            &project,
            "sync-1",
            &json!({"expected_snapshot":"","snapshot":"snap-1","manifest":"{}","metadata":"{}","state":"pending"}),
        )
        .unwrap();
        let sync = sync_get(&store, "proj-1", "sync-1").unwrap().unwrap();
        assert_eq!(s(&sync, "state"), "pending");
        assert_eq!(syncs_unfinished(&store, "proj-1").unwrap(), 1);
        activate(
            &store,
            &project,
            &sync,
            &json!([{"path":"a.py","sha256":"sha-a"},{"path":"b.py","sha256":"sha-b"}]),
            &["a.py".to_string()],
            &[],
        )
        .unwrap();
        let heads = heads(&store, "proj-1").unwrap();
        assert_eq!(heads.len(), 2);
        assert_eq!(heads["a.py"], "sha-a");
        assert_eq!(s(&sync_get(&store, "proj-1", "sync-1").unwrap().unwrap(), "state"), "complete");
        assert_eq!(snapshots(&store, "proj-1", 100).unwrap().len(), 1);
        // One event, sequence 1 from the project's counter.
        let events = store
            .read(|db| {
                rows(
                    db,
                    "MATCH (e:Event {repository_id:$repo}) RETURN e.seq AS seq, e.kind AS kind",
                    Params::from([("repo".to_string(), json_to_value(&json!("proj-1")))]),
                )
            })
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["seq"], 1);
        assert_eq!(events[0]["kind"], "synced");
        // The limited analysis is reported; a current-revision one is not.
        let (limited, skipped) = analysis_report(&store, "proj-1").unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(s(&limited[0], "path"), "a.py");
        assert!(skipped.is_empty());
        // b.py has no parse at all, which also counts as outdated.
        assert_eq!(outdated_heads(&store, "proj-1", ["r9", "r9l"]).unwrap(), 2);
        assert_eq!(outdated_heads(&store, "proj-1", ["r1", "r9"]).unwrap(), 1);
        // A second activation moves the heads in one swap.
        sync_create(
            &store,
            &project,
            "sync-2",
            &json!({"expected_snapshot":"snap-1","snapshot":"snap-2","manifest":"{}","metadata":"{}","state":"pending"}),
        )
        .unwrap();
        let sync = sync_get(&store, "proj-1", "sync-2").unwrap().unwrap();
        activate(
            &store,
            &project,
            &sync,
            &json!([{"path":"b.py","sha256":"sha-b"}]),
            &[],
            &["a.py".to_string()],
        )
        .unwrap();
        let heads = super::heads(&store, "proj-1").unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads["b.py"], "sha-b");
        assert_eq!(snapshots(&store, "proj-1", 100).unwrap().len(), 2);
        sync_abort(&store, "proj-1", "sync-2").unwrap(); // complete: untouched
        sync_create(
            &store,
            &project,
            "sync-3",
            &json!({"expected_snapshot":"snap-2","snapshot":"","manifest":"{}","metadata":"{}","state":"preparing"}),
        )
        .unwrap();
        sync_update(&store, "sync-3", &json!({"manifest":"{\"a\":1}","state":"pending"})).unwrap();
        sync_abort(&store, "proj-1", "sync-3").unwrap();
        assert_eq!(s(&sync_get(&store, "proj-1", "sync-3").unwrap().unwrap(), "state"), "aborted");
        sync_delete(&store, "sync-3").unwrap();
        assert!(sync_get(&store, "proj-1", "sync-3").unwrap().is_none());
    }

    #[test]
    fn put_head_moves_a_single_file() {
        let store = store();
        seeded(&store);
        let project = project();
        put_head(&store, &project, "a.py", "sha-a").unwrap();
        assert_eq!(heads(&store, "proj-1").unwrap()["a.py"], "sha-a");
        put_head(&store, &project, "a.py", "sha-b").unwrap();
        let heads = heads(&store, "proj-1").unwrap();
        assert_eq!(heads.len(), 1);
        assert_eq!(heads["a.py"], "sha-b");
    }
}


#[cfg(test)]
mod durability_probe {
    use super::*;

    /// A committed write survives the last handle dropping and a fresh
    /// open (the cross-process path: CLI child writes, parent reopens).
    #[test]
    fn durable_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("reviews.graph");
        let project = json!({"id":"proj-1","workspace_id":"ws-1","project_key":"key","name":"Project"});
        {
            let store = GraphStore::open(&path).unwrap();
            put_objects(
                &store,
                &project,
                &json!([{"sha256":"sha-a","content":"alpha","bytes":5}]),
            )
            .unwrap();
            put_head(&store, &project, "a.py", "sha-a").unwrap();
            sync_create(
                &store,
                &project,
                "sync-1",
                &json!({"expected_snapshot":"","snapshot":"snap","manifest":"{}","metadata":"{}","state":"pending"}),
            )
            .unwrap();
        }
        // All handles dropped: the engine closed.
        let store = GraphStore::open(&path).unwrap();
        assert_eq!(heads(&store, "proj-1").unwrap()["a.py"], "sha-a");
        assert_eq!(
            object_content(&store, "proj-1", "sha-a").unwrap(),
            Some("alpha".to_string())
        );
        assert!(sync_get(&store, "proj-1", "sync-1").unwrap().is_some());
    }
}
