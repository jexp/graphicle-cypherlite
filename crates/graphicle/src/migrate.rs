//! One-time migration of a legacy SQLite store (`reviews.sqlite3`) into a
//! CypherLite graph store directory (migration plan §5; usage and caveats
//! in `docs/migrate-store.md`).
//!
//! The pipeline is deliberately two-stage with no rusqlite involvement:
//! the standalone exporter (`scripts/migrate/export_store.py`, embedded
//! below and run with `python3`) dumps each migrated table to a CSV, and
//! the per-domain Cypher scripts (`migrate/*.cypher`) load those CSVs
//! into the graph via `LOAD CSV … MERGE` upserts, matching the nodes a
//! relationship hangs off. The source file is opened read-only by the
//! exporter and stays untouched as the backup; the target directory must
//! be empty (or absent) and is never overwritten.
use crate::{
    Error, Result,
    store_graph::{self, GraphStore, Params},
};
use serde_json::{Map, Value as Json, json};
use std::{path::Path, process::Command};

/// The exporter, embedded so an installed binary has no script lookup.
const EXPORTER: &str = include_str!("../../../scripts/migrate/export_store.py");

/// The per-domain import scripts, run in order (later domains MATCH the
/// nodes earlier ones created).
const SCRIPTS: &[(&str, &str)] = &[
    ("ownership", include_str!("migrate/00_ownership.cypher")),
    ("content", include_str!("migrate/01_content.cypher")),
    ("memory", include_str!("migrate/02_memory.cypher")),
    ("symbols", include_str!("migrate/03_symbols.cypher")),
    ("commits", include_str!("migrate/04_commits.cypher")),
    ("timeline", include_str!("migrate/05_timeline.cypher")),
    ("accounts", include_str!("migrate/06_accounts.cypher")),
];

/// One `-- @csv` section of a script: the CSV file it loads, the import
/// statement, and the count query that verifies it.
struct Section {
    csv: String,
    verify: String,
    statement: String,
}

/// Parse a script's `-- @csv <file>` / `-- @verify <cypher>` sections.
/// Comment lines (`-- …`) are stripped from statement bodies.
fn sections(domain: &str, script: &str) -> Result<Vec<Section>> {
    let mut out: Vec<Section> = Vec::new();
    for line in script.lines() {
        if let Some(csv) = line.strip_prefix("-- @csv ") {
            out.push(Section {
                csv: csv.trim().to_string(),
                verify: String::new(),
                statement: String::new(),
            });
        } else if let Some(verify) = line.strip_prefix("-- @verify ") {
            let section = out
                .last_mut()
                .ok_or_else(|| Error::bad(format!("migrate {domain}: @verify before @csv")))?;
            section.verify = verify.trim().trim_end_matches(';').to_string();
        } else if !line.starts_with("--") {
            if let Some(section) = out.last_mut() {
                section.statement.push_str(line);
                section.statement.push('\n');
            }
        }
    }
    for section in &mut out {
        section.statement = section
            .statement
            .trim()
            .trim_end_matches(';')
            .trim()
            .to_string();
        if section.verify.is_empty() || section.statement.is_empty() {
            return Err(Error::bad(format!(
                "migrate {domain}: section {} needs a statement and a @verify count",
                section.csv
            )));
        }
    }
    Ok(out)
}

/// The escape-decoding parameters of the import scripts (the inverse of
/// the exporter's `encode`: U+0001 introduces `\x01n`/`\x01r`/`\x01\x01`).
fn decode_params() -> Params {
    store_graph::params(&json!({
        "EE": "\u{1}\u{1}", "EN": "\u{1}n", "ER": "\u{1}r",
        "PH": "\u{2}", "E": "\u{1}", "NL": "\n", "CR": "\r",
    }))
}

/// A `file:///` URL for a local path (the form cypherlite's LOAD CSV
/// policy accepts).
fn file_url(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| Error::bad("CSV path is not valid UTF-8"))?
        .replace('\\', "/");
    Ok(format!("file:///{}", path.trim_start_matches('/')))
}

/// Run the exporter: `python3 <embedded script> source csv_dir`, returning
/// its manifest (CSV name → row count).
fn export(source: &Path, work: &Path) -> Result<Map<String, Json>> {
    let script = work.join("export_store.py");
    std::fs::write(&script, EXPORTER)?;
    let csv_dir = work.join("csv");
    std::fs::create_dir_all(&csv_dir)?;
    let output = Command::new("python3")
        .arg(&script)
        .arg(source)
        .arg(&csv_dir)
        .output()
        .map_err(|e| Error::bad(format!("migrate-store needs python3 on PATH: {e}")))?;
    if !output.status.success() {
        return Err(Error::bad(format!(
            "store export failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let manifest: Json = serde_json::from_slice(&output.stdout)?;
    Ok(manifest.as_object().cloned().unwrap_or_default())
}

/// Migrate `source` (a `reviews.sqlite3` file, left untouched) into the
/// graph store directory `target`, which must be empty or not exist yet.
/// Returns the JSON report (per-domain CSV and stored row counts).
pub fn migrate_store(source: &Path, target: &Path) -> Result<Json> {
    if !source.is_file() {
        return Err(Error::bad(format!(
            "Source store {} does not exist",
            source.display()
        )));
    }
    if target.exists() {
        let non_empty = target.is_dir() && std::fs::read_dir(target)?.next().is_some();
        if !target.is_dir() || non_empty {
            return Err(Error::bad(format!(
                "Refusing to overwrite {}: the target store is not empty",
                target.display()
            )));
        }
    }
    std::fs::create_dir_all(target)?;
    let work = tempfile::tempdir()?;
    let csv_dir = work.path().join("csv");
    let manifest = export(source, work.path())?;

    // Schema first (graph type, indexes, extension structures) through the
    // regular store open, then dropped so the import can open the engine
    // with the LOAD CSV capability scoped to the export directory.
    drop(GraphStore::open(target)?);
    let uri = target
        .to_str()
        .ok_or_else(|| Error::bad("Target store path is not valid UTF-8"))?;
    let database = cypherlite::Database::open_with(
        uri,
        cypherlite::OpenOptions {
            load_csv_dir: Some(csv_dir.clone()),
            ..Default::default()
        },
    )?;
    let mut connection = database.connect()?;

    let mut domains = Vec::new();
    let mut ok = true;
    for (domain, script) in SCRIPTS {
        for section in sections(domain, script)? {
            let expected = manifest
                .get(section.csv.trim_end_matches(".csv"))
                .and_then(Json::as_i64)
                .unwrap_or(0);
            let mut parameters = decode_params();
            parameters.insert(
                "csv".into(),
                cypherlite::Value::String(file_url(&csv_dir.join(&section.csv))?.into()),
            );
            connection.begin()?;
            let outcome = store_graph::write(&mut connection, &section.statement, parameters);
            match outcome {
                Ok(_) => connection.commit()?,
                Err(error) => {
                    let _ = connection.rollback();
                    return Err(error);
                }
            }
            let stored = store_graph::one(&mut connection, &section.verify, Params::new())?
                .and_then(|row| row["n"].as_i64())
                .unwrap_or(0);
            ok &= stored == expected;
            domains.push(json!({
                "domain": domain, "csv": section.csv,
                "rows": expected, "stored": stored,
            }));
        }
    }
    database.checkpoint()?;
    if !ok {
        return Err(Error::bad(format!(
            "Migration count mismatch: {}",
            serde_json::to_string(&domains)?
        )));
    }
    Ok(json!({"ok": true, "source": source, "target": target, "domains": domains}))
}

/// The human-readable report of [`migrate_store`] (`--json` prints the
/// JSON instead).
pub fn report_text(report: &Json, _style: crate::terminal::Style) -> String {
    let mut text = String::from("Migrated store (the SQLite source is untouched):\n");
    for domain in report["domains"].as_array().into_iter().flatten() {
        text.push_str(&format!(
            "  {:<10} {:<16} {} rows\n",
            store_graph::s(domain, "domain"),
            store_graph::s(domain, "csv"),
            domain["stored"]
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a small legacy store, migrate it, and check the counts of the
    /// fixture domains, the read-only source, and the overwrite refusal.
    #[test]
    fn migrates_fixture_store() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("reviews.sqlite3");
        let db = rusqlite::Connection::open(&source).unwrap();
        db.execute_batch(
            "CREATE TABLE workspaces (id TEXT PRIMARY KEY, capability_hash TEXT NOT NULL,
                name TEXT NOT NULL, created_at TEXT NOT NULL, version INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE credentials (id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL,
                capability_hash TEXT NOT NULL, role TEXT NOT NULL, label TEXT NOT NULL,
                created_at TEXT NOT NULL, expires_at TEXT, revoked_at TEXT);
             CREATE TABLE repo_projects (id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL,
                project_key TEXT NOT NULL, name TEXT NOT NULL, snapshot TEXT NOT NULL DEFAULT '',
                generation INTEGER NOT NULL DEFAULT 0, metadata TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
             CREATE TABLE repo_objects (repository_id TEXT NOT NULL, sha256 TEXT NOT NULL,
                content TEXT NOT NULL, bytes INTEGER NOT NULL, PRIMARY KEY(repository_id, sha256));
             CREATE TABLE repo_memories (id TEXT PRIMARY KEY, repository_id TEXT NOT NULL,
                kind TEXT NOT NULL, revision INTEGER NOT NULL, state TEXT NOT NULL,
                title TEXT NOT NULL, body TEXT NOT NULL, data TEXT NOT NULL,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
             CREATE TABLE gf_nodes (id INTEGER PRIMARY KEY AUTOINCREMENT, repository_id TEXT NOT NULL,
                node_id TEXT NOT NULL, attributes TEXT NOT NULL,
                valid_from_episode TEXT NOT NULL, valid_to_episode TEXT,
                created_at TEXT NOT NULL, expired_at TEXT);
             CREATE TABLE gf_commits (repository_id TEXT NOT NULL, oid TEXT NOT NULL,
                tree_oid TEXT NOT NULL, episode TEXT NOT NULL, committed_us INTEGER NOT NULL,
                recorded_us INTEGER NOT NULL, object BLOB NOT NULL, PRIMARY KEY(repository_id, oid));
             CREATE TABLE gf_commit_parents (repository_id TEXT NOT NULL, commit_oid TEXT NOT NULL,
                parent_oid TEXT NOT NULL, position INTEGER NOT NULL,
                PRIMARY KEY(repository_id, commit_oid, position));
             CREATE TABLE gf_timeline (id INTEGER PRIMARY KEY AUTOINCREMENT, repository_id TEXT NOT NULL,
                branch TEXT NOT NULL, episode TEXT NOT NULL, valid_us INTEGER NOT NULL,
                recorded_us INTEGER NOT NULL, origin TEXT NOT NULL,
                UNIQUE(repository_id, branch, episode, valid_us, recorded_us));
             CREATE TABLE am_oauth_accounts (id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL,
                provider TEXT NOT NULL, external_id TEXT NOT NULL, email TEXT,
                display_name TEXT NOT NULL, token_secret TEXT NOT NULL, refresh_secret TEXT,
                expires_at TEXT, scopes TEXT NOT NULL, state TEXT NOT NULL, updated_at TEXT NOT NULL);
             CREATE TABLE am_provider_identities (workspace_id TEXT NOT NULL, provider TEXT NOT NULL,
                external_id TEXT NOT NULL, credential_id TEXT NOT NULL, verified_at TEXT NOT NULL,
                PRIMARY KEY(workspace_id, provider, external_id, credential_id));
             CREATE TABLE am_provider_emails (workspace_id TEXT NOT NULL, provider TEXT NOT NULL,
                external_id TEXT NOT NULL, email TEXT NOT NULL, verified_at TEXT NOT NULL,
                PRIMARY KEY(workspace_id, provider, external_id));",
        )
        .unwrap();
        db.execute_batch(
            "INSERT INTO workspaces VALUES
                ('w1','h1','One','2026-01-01',3),('w2','h2','Two','2026-01-02',0);
             INSERT INTO credentials VALUES
                ('c1','w1','ch1','owner','main','2026-01-01',NULL,NULL),
                ('c2','w2','ch2','agent','bot','2026-01-02','2027-01-01',NULL);
             INSERT INTO repo_projects VALUES
                ('p1','w1','alpha','Alpha','',1,'{}','2026-01-01','2026-01-02'),
                ('p2','w2','beta','Beta','',0,'{\"x\":1}','2026-01-03','2026-01-03');
             INSERT INTO repo_objects VALUES
                ('p1','aa','fn main() {\n  call();\n}',20),
                ('p1','bb','struct S { a: i32 }',18),
                ('p2','cc','print(\"hi, there\")',17);
             INSERT INTO repo_memories VALUES
                ('m1','p1','fact',1,'active','Title','Body line one\nline two, \"quoted\"','{}','2026-02-01','2026-02-01'),
                ('m2','p2','note',2,'draft','Other','plain','{\"k\":2}','2026-02-02','2026-02-03');
             INSERT INTO gf_nodes (repository_id,node_id,attributes,valid_from_episode,valid_to_episode,created_at) VALUES
                ('p1','n1','{\"stable_id\":\"s1\",\"kind\":\"function\",\"qualified_name\":\"main\",\"relpath\":\"src/main.rs\"}','e1',NULL,'2026-03-01'),
                ('p1','n2','{\"stable_id\":\"s2\",\"kind\":\"function\",\"qualified_name\":\"call\",\"relpath\":\"src/lib.rs\"}','e1',NULL,'2026-03-01'),
                ('p1','n3','{\"stable_id\":\"s3\",\"kind\":\"struct\",\"qualified_name\":\"S\",\"relpath\":\"src/lib.rs\"}','e2',NULL,'2026-03-02'),
                ('p1','n4','{\"stable_id\":\"s4\",\"kind\":\"function\",\"qualified_name\":\"gone\",\"relpath\":\"src/old.rs\"}','e1','e3','2026-03-01');
             INSERT INTO gf_commits VALUES
                ('p1','c1','t1','e1',1000,2000,x'00ff01'),
                ('p1','c2','t2','e2',1100,2100,x'0102'),
                ('p1','c3','t3','e3',1200,2200,x'ff'),
                ('p2','c4','t4','e4',1300,2300,x'0100');
             INSERT INTO gf_commit_parents VALUES
                ('p1','c2','c1',0),
                ('p1','c3','c1',0),
                ('p1','c3','c2',1);
             INSERT INTO gf_timeline (repository_id,branch,episode,valid_us,recorded_us,origin) VALUES
                ('p1','main','e1',1000,2000,'sync'),
                ('p1','main','e2',1100,2100,'sync'),
                ('p2','main','e1',1200,2200,'import');
             INSERT INTO am_oauth_accounts VALUES
                ('o1','w1','github','42','a@x.io','Alice','s1','s2','2027-01-01','repo','active','2026-05-01'),
                ('o2','w1','slack','U9',NULL,'Bob','s3',NULL,NULL,'','active','2026-05-02');
             INSERT INTO am_provider_identities VALUES
                ('w1','github','42','cred1','2026-05-01'),
                ('w1','slack','U9','cred2','2026-05-02');
             INSERT INTO am_provider_emails VALUES
                ('w1','github','42','a@x.io','2026-05-01');",
        )
        .unwrap();
        drop(db);
        let before = std::fs::read(&source).unwrap();

        let target = temp.path().join("reviews.graph");
        let report = migrate_store(&source, &target).unwrap();
        assert_eq!(report["ok"], Json::Bool(true));
        let counts: Map<String, Json> = report["domains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| (d["csv"].as_str().unwrap().to_string(), d["stored"].clone()))
            .collect();
        assert_eq!(counts["workspaces.csv"], 2);
        assert_eq!(counts["credentials.csv"], 2);
        assert_eq!(counts["projects.csv"], 2);
        assert_eq!(counts["objects.csv"], 3);
        assert_eq!(counts["memories.csv"], 2);
        // Three live nodes; the expired one is not exported.
        assert_eq!(counts["symbols.csv"], 3);
        assert_eq!(counts["commits.csv"], 4);
        // One PARENT relationship per gf_commit_parents row.
        assert_eq!(counts["commit_parents.csv"], 3);
        assert_eq!(counts["timeline.csv"], 3);
        assert_eq!(counts["oauth_accounts.csv"], 2);
        assert_eq!(counts["provider_identities.csv"], 2);
        assert_eq!(counts["provider_emails.csv"], 1);
        // The source store is the backup: byte-identical afterwards.
        assert_eq!(std::fs::read(&source).unwrap(), before);
        // A non-empty target is refused.
        let again = migrate_store(&source, &target).unwrap_err();
        assert!(again.message.contains("Refusing to overwrite"));

        // Spot-check content through the regular store handle.
        let graph = GraphStore::open(&target).unwrap();
        graph
            .read(|db| {
                let body = store_graph::one(
                    db,
                    "MATCH (m:Memory {id: 'm1'}) RETURN m.body AS body",
                    Params::new(),
                )?
                .unwrap();
                assert_eq!(body["body"], "Body line one\nline two, \"quoted\"");
                let rels = store_graph::one(
                    db,
                    "MATCH (:Project)-[r:HAS_OBJECT]->(:Object) RETURN count(r) AS n",
                    Params::new(),
                )?
                .unwrap();
                assert_eq!(rels["n"], 3);
                let live = store_graph::one(
                    db,
                    "MATCH (:Symbol)-[:HAS_VERSION]->(v:Live) RETURN count(v) AS n",
                    Params::new(),
                )?
                .unwrap();
                assert_eq!(live["n"], 3);
                // Commit history and its parent links.
                let parents = store_graph::one(
                    db,
                    "MATCH (:Commit)-[r:PARENT]->(:Commit) RETURN count(r) AS n",
                    Params::new(),
                )?
                .unwrap();
                assert_eq!(parents["n"], 3);
                // gf_commits.object round-trips as base64 (bytes 00 ff 01).
                let object = store_graph::one(
                    db,
                    "MATCH (c:Commit {oid: 'c1'}) RETURN c.object AS object",
                    Params::new(),
                )?
                .unwrap();
                assert_eq!(object["object"], "AP8B");
                // Timeline assertions hang off their project.
                let timeline = store_graph::one(
                    db,
                    "MATCH (a:Assertion)-[:ON_TIMELINE]->(:Project) RETURN count(a) AS n",
                    Params::new(),
                )?
                .unwrap();
                assert_eq!(timeline["n"], 3);
                Ok(())
            })
            .unwrap();
    }
}
