//! Feature-level Done-when matrix (Linear LABS-132): the `db.ontology.*`
//! procedures end to end — every write and read goes through Cypher, never
//! the facade — across close/reopen on the three store kinds (Native
//! directory, shared `MemoryVfs`, `.cl` bundle) under both snapshot codecs
//! (Parquet, JSON), plus the WAL/manifest format transitions of ADR-0120
//! §4/§8 driven through the procedure path.
//!
//! `ontology_store.rs` pins the same storage guarantees through the facade
//! (`commit_ontology_set`/`commit_ontology_drop`); `ontology_procedures.rs`
//! pins the procedure semantics (validation, formats, transactions,
//! namespaces) on a single in-memory store. This file deliberately pins
//! only what neither covers: procedure-driven durability across the full
//! store-kind × codec matrix, and procedure-driven format transitions.

mod common;

use std::sync::Arc;

use cypherlite::{Database, OpenOptions, Value};
use cypherlite_storage::{MemoryVfs, SnapshotCodecKind, Vfs};

use common::{assert_checkpoint_format, err_text, get_doc, magics, segment_magics, set_doc};

/// The YAML matrix document (the twin-ontology shape, freely formatted —
/// the byte-identity legs pin verbatim storage across reopens).
const DOC_YAML: &str = "\
# matrix document, kept verbatim
schema: https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json
id: twin
version: \"1\"
name: Twin
nodes:
  Person:
    label: Person
    properties:
      id:
        type: STRING
        key: true
relationships:
  KNOWS:
    type: KNOWS
    from:
      node: Person
    to:
      node: Person
";

/// The CypherLite-authored main-shape example (pretty-printed JSON) —
/// the JSON-in leg pins that `get('json')` hands these exact bytes back
/// after a reopen.
const EXAMPLE_JSON: &str = include_str!("../../cypherlite-ontology/schema/example.json");

const GET_YAML: &str = "CALL db.ontology.get() YIELD ontology RETURN ontology";
const GET_JSON: &str = "CALL db.ontology.get('json') YIELD ontology RETURN ontology";

/// The default options pin the snapshot codec only: no automatic
/// checkpoint interval, so every durability boundary below is the explicit
/// `db.checkpoint()` in [`close`].
fn opts(codec: SnapshotCodecKind) -> OpenOptions {
    OpenOptions {
        snapshot_codec: Some(codec),
        ..OpenOptions::default()
    }
}

/// Checkpoint, then drop the handle: an honest close on every store kind.
/// On a shared `MemoryVfs` the explicit checkpoint is what carries the
/// store into the next open (a graceful drop discards the non-durable WAL
/// tail, ADR-0105); on Native and bundle stores it keeps the segment
/// assertions below deterministic.
fn close(db: Database) {
    db.checkpoint().unwrap();
    drop(db);
}

/// ADR-0229 §1/§3: after a reopen the `system_ontologies` namespace stays
/// not queryable — no graph is derived from the stored document — while
/// the procedures against `neo4j` still reach it.
fn assert_namespace_stays_not_queryable(conn: &mut cypherlite::Connection) {
    for query in [
        "USE system_ontologies MATCH (n) RETURN count(n) AS c",
        "USE system_ontologies MATCH ()-[r]->() RETURN count(r) AS c",
    ] {
        let err = err_text(conn.query(query));
        assert!(
            err.ends_with(
                "This database is not queryable. Use the db.ontology.* procedures on the user database 'neo4j' instead."
            ),
            "{query}: {err}"
        );
    }
    assert_eq!(
        get_doc(conn, GET_YAML),
        Value::String(DOC_YAML.into()),
        "the procedures still reach the stored document"
    );
}

/// One full matrix cell, driven entirely through the procedures:
///
/// 1. YAML `set` → close → reopen: `get()` returns the identical bytes.
/// 2. `apply` on the reopened store → close → reopen: `get()` still
///    returns the identical bytes, the applied graph type survives, and
///    the `system_ontologies` namespace stays not queryable (ADR-0229).
/// 3. `drop` → close → reopen: `get()` returns null.
/// 4. JSON `set` (the vendored example) → close → reopen: `get('json')`
///    returns the identical bytes and the default `get()` returns the
///    canonical YAML conversion.
fn matrix_cell(reopen: &dyn Fn() -> Database) {
    let db = reopen();
    let mut conn = db.connect().unwrap();
    assert_eq!(get_doc(&mut conn, GET_YAML), Value::Null);
    set_doc(&mut conn, DOC_YAML).unwrap();
    drop(conn);
    close(db);

    let db = reopen();
    let mut conn = db.connect().unwrap();
    assert_eq!(
        get_doc(&mut conn, GET_YAML),
        Value::String(DOC_YAML.into()),
        "the stored YAML bytes survive a close/reopen unchanged"
    );
    conn.query("CALL db.ontology.apply()").unwrap();
    drop(conn);
    close(db);

    let db = reopen();
    let mut conn = db.connect().unwrap();
    assert_eq!(
        get_doc(&mut conn, GET_YAML),
        Value::String(DOC_YAML.into()),
        "the stored YAML bytes survive apply + close/reopen unchanged"
    );
    assert_namespace_stays_not_queryable(&mut conn);
    conn.query("CALL db.ontology.drop()").unwrap();
    drop(conn);
    close(db);

    let db = reopen();
    let mut conn = db.connect().unwrap();
    assert_eq!(
        get_doc(&mut conn, GET_YAML),
        Value::Null,
        "drop survives a close/reopen"
    );
    set_doc(&mut conn, EXAMPLE_JSON).unwrap();
    drop(conn);
    close(db);

    let db = reopen();
    let mut conn = db.connect().unwrap();
    assert_eq!(
        get_doc(&mut conn, GET_JSON),
        Value::String(EXAMPLE_JSON.into()),
        "JSON in + get('json') returns the stored bytes unchanged"
    );
    let yaml = get_doc(&mut conn, GET_YAML);
    let Value::String(yaml) = yaml else {
        panic!("expected the canonical YAML conversion, got {yaml:?}")
    };
    assert!(
        yaml.contains("id: cypherlite-ontology-example"),
        "JSON in + get() returns the canonical YAML form: {yaml}"
    );
}

/// Open a Native-directory store in a fresh tempdir under `codec`; the
/// returned reopen closure owns the tempdir.
fn native_dir_reopen(codec: SnapshotCodecKind) -> Box<dyn Fn() -> Database> {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap().to_string();
    Box::new(move || {
        let _tempdir = &dir;
        Database::open_with(&uri, opts(codec)).unwrap()
    })
}

/// Open a store on a fresh shared `MemoryVfs` under `codec`; the returned
/// reopen closure reuses the same VFS handle, which is what carries the
/// store across open/close (the ontology_store.rs pattern).
fn memory_vfs_reopen(codec: SnapshotCodecKind) -> Box<dyn Fn() -> Database> {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    Box::new(move || Database::open_vfs(Arc::clone(&vfs), opts(codec)).unwrap())
}

/// Open a `.cl` bundle in a fresh tempdir under `codec`; the returned
/// reopen closure owns the tempdir.
fn bundle_reopen(codec: SnapshotCodecKind) -> Box<dyn Fn() -> Database> {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("graph.cl").to_str().unwrap().to_string();
    Box::new(move || {
        let _tempdir = &dir;
        Database::open_with(&uri, opts(codec)).unwrap()
    })
}

/// Run [`matrix_cell`] under every snapshot codec compiled into this
/// build (Parquet needs the `parquet` feature, so a parquet-less build
/// runs the JSON leg only); `open` builds the store's reopen closure for
/// the codec.
fn run_matrix(open: impl Fn(SnapshotCodecKind) -> Box<dyn Fn() -> Database>) {
    for &codec in SnapshotCodecKind::compiled() {
        matrix_cell(&open(codec));
    }
}

#[test]
fn native_directory_procedure_matrix_both_codecs() {
    common::preview_ontology();
    run_matrix(native_dir_reopen);
}

#[test]
fn memory_vfs_procedure_matrix_both_codecs() {
    common::preview_ontology();
    run_matrix(memory_vfs_reopen);
}

#[test]
fn bundle_procedure_matrix_both_codecs() {
    common::preview_ontology();
    run_matrix(bundle_reopen);
}

/// ADR-0120 §4/§8 through the procedure path: the first `db.ontology.set`
/// rotates the active WAL segment to `MDBWAL02`; segments created while
/// the ontology is held stay `MDBWAL02`; after `db.ontology.drop()` +
/// checkpoint + WAL recycle the manifest is format_version 3 (Parquet) /
/// graph.json carries no `ontology` field at value_encoding 2 (JSON) and
/// new segments are `MDBWAL01` again.
///
/// `ontology_store.rs` pins the same transitions through the facade; this
/// is the Done-when coverage through Cypher, including the pre-checkpoint
/// rotation the facade test does not observe.
#[test]
fn procedure_driven_format_transitions() {
    common::preview_ontology();
    for &codec in SnapshotCodecKind::compiled() {
        let dir = tempfile::tempdir().unwrap();
        let uri = dir.path().to_str().unwrap().to_string();

        let db = Database::open_with(&uri, opts(codec)).unwrap();
        let mut conn = db.connect().unwrap();
        // A fresh store is the plain format: one MDBWAL01 segment.
        assert_eq!(magics(dir.path()), vec![*b"MDBWAL01"]);

        // The first set rotates the active segment to MDBWAL02, before any
        // checkpoint runs.
        set_doc(&mut conn, DOC_YAML).unwrap();
        let segments = segment_magics(dir.path());
        assert_eq!(
            segments.last().unwrap().1,
            *b"MDBWAL02",
            "the first set rotated the active segment to MDBWAL02: {segments:?}"
        );

        // Segments created while the ontology is held stay MDBWAL02, and
        // the checkpoint publishes the ontology.
        conn.query("CREATE (:Widget {id: 1})").unwrap();
        db.checkpoint().unwrap();
        assert!(
            magics(dir.path()).iter().all(|m| m == b"MDBWAL02"),
            "every segment is MDBWAL02 while the ontology is held"
        );
        assert_checkpoint_format(dir.path(), codec, true, None);

        // Drop through the procedure, checkpoint, recycle: the store is
        // back at the plain format.
        conn.query("CALL db.ontology.drop()").unwrap();
        db.checkpoint().unwrap();
        assert_checkpoint_format(dir.path(), codec, false, None);
        assert_eq!(magics(dir.path()), vec![*b"MDBWAL01"]);

        // Segments created after the recycle are MDBWAL01 again.
        conn.query("CREATE (:Widget {id: 2})").unwrap();
        let segments = segment_magics(dir.path());
        assert_eq!(segments.last().unwrap().1, *b"MDBWAL01");

        // And the empty state survives a reopen, observed through Cypher.
        drop(conn);
        drop(db);
        let db = Database::open_with(&uri, opts(codec)).unwrap();
        let mut conn = db.connect().unwrap();
        assert_eq!(get_doc(&mut conn, GET_YAML), Value::Null);
        assert_eq!(magics(dir.path()), vec![*b"MDBWAL01"]);
    }
}
