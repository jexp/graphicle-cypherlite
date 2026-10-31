//! Golden-store compatibility fixtures (ADR-0114 §3).
//!
//! `testdata/stores/v0.8.0/` holds stores written by CypherLite v0.8.0 in
//! the current on-disk format: a Parquet Native directory, a JSON Native
//! directory, and a single-file `.cl` bundle. Each carries a checkpoint
//! plus a WAL tail. The golden tests below copy a fixture into a temporary
//! directory (a fixture is never opened in place), open it, assert its
//! exact contents through Cypher, checkpoint, verify the format markers
//! survived, and reopen to assert the same contents again.
//!
//! The ignored [`write_golden_stores`] generator rewrote the set once, from
//! tag `v0.8.0`; a later format ADR adds a new set next to it instead of
//! regenerating this one. See `testdata/stores/README.md`.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use cypherlite::{Connection, Database, OpenOptions, Value};
use cypherlite_storage::{
    is_bundle_path, read_to_vec, SnapshotCodecKind, Vfs, LOCK_MARKER, MAGIC, ROOT_ROLE, WAL_ROLE,
};

use common::store_vfs;

/// Store-generating release: tag `v0.8.0`.
const GENERATED_FROM: &str = "v0.8.0 (2397a5daf0e06ffe1dc61d9d18e5d9eef3221980)";

/// Committed before the generator's checkpoint: the schema, the databases,
/// and the base graph every golden store shares.
///
/// The `CREATE DATABASE` entries ran under pre-ADR-0116 theater semantics
/// at the generating tag; a current binary ignores their `PutDatabase`
/// rows on load (ADR-0116 §8), so this generator must not be re-run with
/// a current binary -- a new set comes from a new tag instead.
const PRE_CHECKPOINT: &[&str] = &[
    "CREATE INDEX person_name FOR (n:Person) ON (n.name)",
    "CREATE VECTOR INDEX person_embedding FOR (n:Person) ON (n.embedding) \
     OPTIONS {indexConfig: {`vector.dimensions`: 3, `vector.similarity_function`: 'cosine'}}",
    "CREATE FULLTEXT INDEX person_bio FOR (n:Person) ON EACH [n.bio]",
    "CREATE CONSTRAINT person_email_unique FOR (n:Person) REQUIRE n.email IS UNIQUE",
    "CREATE CONSTRAINT person_name_exists FOR (n:Person) REQUIRE n.name IS NOT NULL",
    "CREATE DATABASE analytics",
    "ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5",
    "CREATE (n:Person:Scientist {name: 'Ada', email: 'ada@example.com', \
     born: date('1815-12-10'), age: 36, score: 9.5, active: true, \
     tags: ['math', 'computing'], meeting: datetime('1815-12-10T08:00:00+01:00'), \
     tenure: duration('P1Y2M'), alarm: localtime('07:30:00'), \
     home: point({x: 1.5, y: -2.25}), bio: 'first programmer', \
     embedding: vector([0.5, 0.25, 1.0], 3, FLOAT32)})",
    "CREATE (n:Person {name: 'Charles', email: 'charles@example.com', \
     born: date('1791-12-26'), age: 44, score: 8.25, active: false, \
     tags: ['engines'], bio: 'difference engine', \
     embedding: vector([0.1, 0.9, 0.0], 3, FLOAT32)})",
    "MATCH (a:Person {name: 'Ada'}), (c:Person {name: 'Charles'}) \
     CREATE (a)-[r:KNOWS {since: 1833, weight: 0.75}]->(c)",
];

/// Committed after the checkpoint: the WAL tail. Together these cover the
/// record kinds a later format reader must still decode.
const WAL_TAIL: &[&str] = &[
    "CREATE DATABASE staging",
    "CREATE INDEX tail_scratch FOR (n:Scratch) ON (n.k)",
    "DROP INDEX tail_scratch",
    "CREATE (t:Tail {x: 1, y: 2})",
    "MATCH (t:Tail) SET t.z = 3",
    "MATCH (t:Tail) REMOVE t.y",
    "MATCH (t:Tail) SET t:Extra:Marked",
    "MATCH (t:Tail) REMOVE t:Marked",
    "MATCH (t:Tail) CREATE (t)-[r:LINK {w: 1}]->(t)",
    "MATCH ()-[r:LINK]->() SET r.w = 2",
    "MATCH ()-[r:LINK]->() DELETE r",
    "MATCH (t:Tail) DELETE t",
];

/// One fixture store: a subdirectory (or file) of `testdata/stores/v0.8.0/`.
/// The store kind is derived from the path — a `.cl` subpath is a
/// single-file bundle, a subdirectory a Native directory store.
struct GoldenStore {
    /// Path below the versioned set directory.
    subpath: &'static str,
    /// Snapshot codec the store was written with.
    codec: SnapshotCodecKind,
}

#[cfg(feature = "parquet")]
const PARQUET_DIR: GoldenStore = GoldenStore {
    subpath: "parquet",
    codec: SnapshotCodecKind::Parquet,
};

const JSON_DIR: GoldenStore = GoldenStore {
    subpath: "json",
    codec: SnapshotCodecKind::Json,
};

#[cfg(feature = "parquet")]
const GRAPH_BUNDLE: GoldenStore = GoldenStore {
    subpath: "graph.cl",
    codec: SnapshotCodecKind::Parquet,
};

fn stores_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/stores/v0.8.0")
}

/// Rewrite the full golden set into `$CYPHERLITE_GOLDEN_STORES_OUT`. Run
/// from a detached worktree at the generating tag, never against a tree
/// with unrelated edits, and never into a directory that already holds a
/// set:
///
/// ```sh
/// CYPHERLITE_GOLDEN_STORES_OUT=<repo>/testdata/stores/vX.Y.Z \
///   cargo test -p cypherlite --test golden_stores write_golden_stores \
///   -- --ignored --exact --nocapture
/// ```
#[test]
#[ignore = "fixture generator; needs CYPHERLITE_GOLDEN_STORES_OUT"]
fn write_golden_stores() {
    let out = std::env::var("CYPHERLITE_GOLDEN_STORES_OUT")
        .expect("CYPHERLITE_GOLDEN_STORES_OUT must name the output directory");
    let root = Path::new(&out);
    if root.exists() && root.read_dir().unwrap().next().is_some() {
        panic!("{out} is not empty; a golden set is never regenerated in place");
    }
    std::fs::create_dir_all(root).unwrap();
    #[cfg(feature = "parquet")]
    write_store(&root.join("parquet"), Some(SnapshotCodecKind::Parquet));
    write_store(&root.join("json"), Some(SnapshotCodecKind::Json));
    #[cfg(feature = "parquet")]
    write_store(&root.join("graph.cl"), None);
}

/// Write one store: schema + base graph, a checkpoint, then the WAL tail,
/// and drop without checkpointing so the tail stays WAL-only.
fn write_store(path: &Path, codec: Option<SnapshotCodecKind>) {
    let db = Database::open_with(
        path.to_str().unwrap(),
        OpenOptions {
            checkpoint_interval: Some(Duration::ZERO),
            snapshot_codec: codec,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let mut conn = db.connect().unwrap();
    for query in PRE_CHECKPOINT {
        conn.query(query).unwrap();
    }
    db.checkpoint().unwrap();
    for query in WAL_TAIL {
        conn.query(query).unwrap();
    }
    drop(conn);
    db.leak_without_checkpoint();
}

#[cfg(feature = "parquet")]
#[test]
fn golden_store_parquet_directory_opens_checkpoints_and_reopens() {
    run_golden(&PARQUET_DIR);
}

#[test]
fn golden_store_json_directory_opens_checkpoints_and_reopens() {
    run_golden(&JSON_DIR);
}

#[cfg(feature = "parquet")]
#[test]
fn golden_store_graph_bundle_opens_checkpoints_and_reopens() {
    run_golden(&GRAPH_BUNDLE);
}

fn run_golden(store: &GoldenStore) {
    let fixture = stores_root().join(store.subpath);
    assert!(
        fixture.exists(),
        "fixture {} is missing; set written by {GENERATED_FROM}",
        fixture.display()
    );
    let tmp = tempfile::tempdir().unwrap();
    let working = tmp.path().join(store.subpath);
    copy_fixture(&fixture, &working);

    // A bundle locks the host file itself; only a Native directory carries
    // marker files.
    if !is_bundle_path(&working) {
        assert_eq!(std::fs::read(working.join(LOCK_MARKER)).unwrap(), ROOT_ROLE);
        assert_eq!(
            std::fs::read(working.join("wal").join(LOCK_MARKER)).unwrap(),
            WAL_ROLE
        );
        let mut tmp_files = Vec::new();
        collect_tmp_files(&working, &mut tmp_files);
        assert!(tmp_files.is_empty(), "stray *.tmp files: {tmp_files:?}");
    }

    let vfs = store_vfs(&working);
    let checkpoint_lsn = manifest_checkpoint_lsn(&*vfs);
    let scan = scan_wal(&*vfs, checkpoint_lsn);
    assert!(scan.segments > 0, "fixture must carry a WAL tail");
    assert!(
        scan.tail_records > 0,
        "fixture must carry records above checkpoint_lsn {checkpoint_lsn}"
    );
    for required in [
        "CreateNode",
        "CreateRel",
        "SetNodeProp",
        "RemoveNodeProp",
        "SetRelProp",
        "AddLabels",
        "RemoveLabels",
        "DeleteNode",
        "DeleteRel",
        "PutDatabase",
    ] {
        assert!(
            scan.tail_ops.contains(required),
            "WAL tail is missing a {required} record"
        );
    }
    assert!(
        [
            "CreateIndex",
            "DropIndex",
            "CreateConstraint",
            "DropConstraint"
        ]
        .iter()
        .any(|kind| scan.tail_ops.contains(*kind)),
        "WAL tail is missing an index/constraint record"
    );

    // Release the store handle before opening the database: a bundle VFS
    // holds the host file's exclusive lock.
    drop(vfs);
    {
        let db = Database::open(working.to_str().unwrap()).unwrap();
        let mut conn = db.connect().unwrap();
        assert_contents(&mut conn);
        db.checkpoint().unwrap();
    }

    // LABS-132: re-checkpointing a v0.8.0 store is byte-stable.
    assert_recheckpoint_byte_stable(&working, store);

    // The checkpoint rewrites the snapshot in the same format; the WAL
    // stays MDBWAL01.
    let vfs = store_vfs(&working);
    let manifest = read_to_vec(&*vfs, "manifest.json").unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
    // ADR-0116 §8: the checkpoint image no longer carries the fixture's
    // `analytics`/`staging` theater rows -- only the fixed namespaces.
    let theater = match store.codec {
        SnapshotCodecKind::Parquet => {
            assert_eq!(manifest["format_version"], 3);
            // LABS-132: no ontology anywhere in a re-checkpointed v0.8.0
            // store — no `ontology_path` in the manifest.
            assert!(
                manifest.get("ontology_path").is_none(),
                "v0.8.0 manifest carries no ontology_path: {manifest}"
            );
            manifest["theater"].clone()
        }
        SnapshotCodecKind::Json => {
            let graph = read_to_vec(&*vfs, "graph.json").unwrap();
            let graph: serde_json::Value = serde_json::from_slice(&graph).unwrap();
            assert_eq!(graph["value_encoding"], 2);
            // LABS-132: no ontology anywhere — no `ontology_path` in the
            // manifest, no `ontology` field in graph.json.
            assert!(
                manifest.get("ontology_path").is_none(),
                "v0.8.0 manifest carries no ontology_path: {manifest}"
            );
            assert!(
                graph.get("ontology").is_none(),
                "v0.8.0 graph.json carries no ontology field: {graph}"
            );
            graph["theater"].clone()
        }
    };
    // And no `ontology/` document objects in the store (the prefix family
    // exists only when a v4 manifest published one, ADR-0120 §5).
    assert!(
        vfs.list_dir("ontology")
            .map(|n| n.is_empty())
            .unwrap_or(true),
        "{} holds ontology/ objects",
        store.subpath
    );
    let mut names: Vec<String> = theater["databases"]
        .as_object()
        .expect("checkpoint theater carries a databases map")
        .keys()
        .cloned()
        .collect();
    names.sort();
    assert_eq!(names, ["neo4j", "system", "system_ontologies"]);
    let scan = scan_wal(&*vfs, u64::MAX);
    assert!(scan.segments > 0);
    drop(vfs);

    let db = Database::open(working.to_str().unwrap()).unwrap();
    let mut conn = db.connect().unwrap();
    assert_contents(&mut conn);
}

/// The exact graph and catalog every golden store holds after recovery.
fn assert_contents(conn: &mut Connection) {
    let r = conn.query("MATCH (n) RETURN count(n)").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(2)]]);
    let r = conn.query("MATCH ()-[r]->() RETURN count(r)").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(1)]]);
    // The WAL tail created and deleted :Tail; nothing of it survives.
    let r = conn.query("MATCH (t:Tail) RETURN count(t)").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int(0)]]);

    // Label sets.
    let r = conn
        .query("MATCH (n:Person:Scientist) RETURN n.name")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("Ada".into())]]);
    let r = conn
        .query("MATCH (n:Person) WHERE NOT n:Scientist RETURN n.name")
        .unwrap();
    assert_eq!(r.rows, vec![vec![Value::String("Charles".into())]]);

    // Every property kind, compared with an equality predicate.
    for query in [
        "MATCH (n:Person {name: 'Ada'}) RETURN n.name = 'Ada'",
        "MATCH (n:Person {name: 'Ada'}) RETURN n.email = 'ada@example.com'",
        "MATCH (n:Person {name: 'Ada'}) RETURN n.born = date('1815-12-10')",
        "MATCH (n:Person {name: 'Ada'}) RETURN n.age = 36",
        "MATCH (n:Person {name: 'Ada'}) RETURN n.score = 9.5",
        "MATCH (n:Person {name: 'Ada'}) RETURN n.active = true",
        "MATCH (n:Person {name: 'Ada'}) RETURN n.tags = ['math', 'computing']",
        "MATCH (n:Person {name: 'Ada'}) \
         RETURN n.meeting = datetime('1815-12-10T08:00:00+01:00')",
        "MATCH (n:Person {name: 'Ada'}) RETURN n.tenure = duration('P1Y2M')",
        "MATCH (n:Person {name: 'Ada'}) RETURN n.alarm = localtime('07:30:00')",
        "MATCH (n:Person {name: 'Ada'}) RETURN n.home = point({x: 1.5, y: -2.25})",
        "MATCH (n:Person {name: 'Ada'}) RETURN n.bio = 'first programmer'",
        "MATCH (n:Person {name: 'Ada'}) \
         RETURN n.embedding = vector([0.5, 0.25, 1.0], 3, FLOAT32)",
        "MATCH (n:Person {name: 'Charles'}) RETURN n.born = date('1791-12-26')",
        "MATCH (n:Person {name: 'Charles'}) RETURN n.active = false",
        "MATCH (n:Person {name: 'Charles'}) RETURN n.tags = ['engines']",
        "MATCH (n:Person {name: 'Charles'}) \
         RETURN n.embedding = vector([0.1, 0.9, 0.0], 3, FLOAT32)",
        "MATCH ()-[r:KNOWS]->() RETURN r.since = 1833 AND r.weight = 0.75",
    ] {
        let r = conn.query(query).unwrap();
        assert_eq!(r.rows, vec![vec![Value::Bool(true)]], "{query}");
    }

    // Catalog rows: the pre-checkpoint schema, with the tail's scratch
    // index dropped again.
    let rows = schema_rows(
        conn,
        "SHOW INDEXES YIELD name, type, labelsOrTypes, properties \
         RETURN name, type, labelsOrTypes, properties",
    );
    assert_eq!(
        rows,
        vec![
            (
                "person_bio".to_string(),
                "FULLTEXT".to_string(),
                vec!["Person".to_string()],
                vec!["bio".to_string()],
            ),
            (
                "person_email_unique".to_string(),
                "RANGE".to_string(),
                vec!["Person".to_string()],
                vec!["email".to_string()],
            ),
            (
                "person_embedding".to_string(),
                "VECTOR".to_string(),
                vec!["Person".to_string()],
                vec!["embedding".to_string()],
            ),
            (
                "person_name".to_string(),
                "RANGE".to_string(),
                vec!["Person".to_string()],
                vec!["name".to_string()],
            ),
        ]
    );

    let rows = schema_rows(
        conn,
        "SHOW CONSTRAINTS YIELD name, type, labelsOrTypes, properties \
         RETURN name, type, labelsOrTypes, properties",
    );
    assert_eq!(
        rows,
        vec![
            (
                "person_email_unique".to_string(),
                "NODE_PROPERTY_UNIQUENESS".to_string(),
                vec!["Person".to_string()],
                vec!["email".to_string()],
            ),
            (
                "person_name_exists".to_string(),
                "NODE_PROPERTY_EXISTENCE".to_string(),
                vec!["Person".to_string()],
                vec!["name".to_string()],
            ),
        ]
    );

    // ADR-0116 §1/§8: exactly the three fixed namespaces. The fixture's
    // `CREATE DATABASE analytics`/`CREATE DATABASE staging` theater rows
    // (snapshot and WAL tail) are ignored on load.
    let r = conn
        .query(
            "SHOW DATABASES YIELD name, type, home, defaultLanguage \
             RETURN name, type, home, defaultLanguage",
        )
        .unwrap();
    let mut rows: Vec<(String, String, bool, String)> = r
        .rows
        .iter()
        .map(|row| {
            (
                row[0].as_string().unwrap().to_string(),
                row[1].as_string().unwrap().to_string(),
                row[2].as_bool().unwrap(),
                row[3].as_string().unwrap().to_string(),
            )
        })
        .collect();
    rows.sort();
    assert_eq!(
        rows,
        vec![
            (
                "neo4j".to_string(),
                "standard".to_string(),
                true,
                "CYPHER 5".to_string(),
            ),
            (
                "system".to_string(),
                "system".to_string(),
                false,
                "CYPHER 25".to_string(),
            ),
            (
                "system_ontologies".to_string(),
                "standard".to_string(),
                false,
                "CYPHER 25".to_string(),
            ),
        ]
    );
}

/// Sorted `(name, type, labelsOrTypes, properties)` rows of a SHOW
/// INDEXES / SHOW CONSTRAINTS query.
fn schema_rows(
    conn: &mut Connection,
    query: &str,
) -> Vec<(String, String, Vec<String>, Vec<String>)> {
    let r = conn.query(query).unwrap();
    let mut rows: Vec<(String, String, Vec<String>, Vec<String>)> = r
        .rows
        .iter()
        .map(|row| {
            (
                row[0].as_string().unwrap().to_string(),
                row[1].as_string().unwrap().to_string(),
                string_list(&row[2]),
                string_list(&row[3]),
            )
        })
        .collect();
    rows.sort();
    rows
}

fn string_list(value: &Value) -> Vec<String> {
    match value {
        Value::List(items) => items
            .iter()
            .map(|item| item.as_string().unwrap().to_string())
            .collect(),
        other => panic!("expected a string list, got {other:?}"),
    }
}

fn copy_fixture(fixture: &Path, working: &Path) {
    if fixture.is_dir() {
        copy_dir_all(fixture, working);
    } else {
        std::fs::copy(fixture, working).unwrap();
    }
}

fn copy_dir_all(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn collect_tmp_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            collect_tmp_files(&path, out);
        } else if entry.file_name().to_string_lossy().ends_with(".tmp") {
            out.push(path);
        }
    }
}

/// `(virtual path, bytes)` of every entry in a working copy, read through
/// the store's VFS so Native directories and `.cl` bundles enumerate the
/// same way. The database must be closed (a bundle VFS locks the host
/// file).
fn store_file_bytes(working: &Path) -> BTreeMap<String, Vec<u8>> {
    let vfs = store_vfs(working);
    let mut files = BTreeMap::new();
    collect_file_bytes(&*vfs, "", &mut files);
    files
}

/// LABS-132: re-checkpointing a v0.8.0 store is byte-stable. Reopen,
/// checkpoint again, and compare every store file; the only tolerated
/// drift is the manifest's `bytes_per_node`/`bytes_per_rel` size
/// heuristics, which keep moving while the store settles.
fn assert_recheckpoint_byte_stable(working: &Path, store: &GoldenStore) {
    let before = store_file_bytes(working);
    {
        let db = Database::open(working.to_str().unwrap()).unwrap();
        db.checkpoint().unwrap();
    }
    let after = store_file_bytes(working);
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "re-checkpointing {} changed the file set",
        store.subpath
    );
    for (name, bytes) in &before {
        let other = &after[name];
        if name == "manifest.json" {
            let strip_heuristics = |bytes: &[u8]| -> serde_json::Value {
                let mut manifest: serde_json::Value = serde_json::from_slice(bytes).unwrap();
                let object = manifest.as_object_mut().unwrap();
                object.remove("bytes_per_node");
                object.remove("bytes_per_rel");
                manifest
            };
            assert_eq!(
                strip_heuristics(bytes),
                strip_heuristics(other),
                "re-checkpointing {} changed manifest.json",
                store.subpath
            );
        } else {
            assert_eq!(
                bytes, other,
                "re-checkpointing {} changed {name}",
                store.subpath
            );
        }
    }
}

fn collect_file_bytes(vfs: &dyn Vfs, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>) {
    for name in vfs.list_dir(prefix).unwrap() {
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if vfs.metadata(&path).unwrap().is_dir {
            collect_file_bytes(vfs, &path, out);
        } else {
            out.insert(path.clone(), read_to_vec(vfs, &path).unwrap());
        }
    }
}

fn manifest_checkpoint_lsn(vfs: &dyn Vfs) -> u64 {
    let bytes = read_to_vec(vfs, "manifest.json").unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    manifest["checkpoint_lsn"].as_u64().unwrap()
}

/// `(segment name, bytes)` pairs of every WAL segment in the working copy.
fn wal_segments(vfs: &dyn Vfs) -> Vec<(String, Vec<u8>)> {
    let mut names = vfs.list_dir("wal").unwrap();
    names.retain(|name| name.ends_with(".log"));
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let bytes = read_to_vec(vfs, &format!("wal/{name}")).unwrap();
            (name, bytes)
        })
        .collect()
}

struct WalScan {
    segments: usize,
    /// Count of records above the snapshot's checkpoint LSN.
    tail_records: usize,
    /// `op` tags of records above the snapshot's checkpoint LSN.
    tail_ops: BTreeSet<String>,
}

/// Read every WAL segment by hand: 8-byte magic, then `crc32-le + len-le +
/// JSON payload` frames. Every frame must pass its CRC and parse as JSON.
fn scan_wal(vfs: &dyn Vfs, checkpoint_lsn: u64) -> WalScan {
    let segments = wal_segments(vfs);
    let mut scan = WalScan {
        segments: segments.len(),
        tail_records: 0,
        tail_ops: BTreeSet::new(),
    };
    for (name, bytes) in &segments {
        assert!(
            bytes.len() >= 8 && &bytes[..8] == MAGIC,
            "WAL segment {name} does not start with MDBWAL01"
        );
        let mut offset = 8;
        while offset < bytes.len() {
            assert!(
                offset + 8 <= bytes.len(),
                "truncated frame header in {name}"
            );
            let crc = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            let len = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap());
            offset += 8;
            let end = offset + len as usize;
            assert!(end <= bytes.len(), "truncated frame payload in {name}");
            let payload = &bytes[offset..end];
            assert_eq!(
                crc32fast::hash(payload),
                crc,
                "WAL frame CRC mismatch in {name}"
            );
            let record: serde_json::Value = serde_json::from_slice(payload)
                .unwrap_or_else(|e| panic!("WAL frame in {name} is not JSON: {e}"));
            let lsn = record["lsn"].as_u64().unwrap();
            if lsn > checkpoint_lsn {
                scan.tail_records += 1;
                for op in record["ops"].as_array().unwrap() {
                    scan.tail_ops.insert(op["op"].as_str().unwrap().to_string());
                }
            }
            offset = end;
        }
    }
    scan
}
