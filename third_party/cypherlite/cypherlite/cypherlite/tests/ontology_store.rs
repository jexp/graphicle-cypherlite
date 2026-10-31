//! ADR-0120 store integration roundtrips (LABS-102): the facade keeps the
//! ontology document as store state and commits `OntologySet`/
//! `OntologyDrop` through the WAL pipeline (ADR-0229 §3: the document is
//! the sole stored source of truth — no graph is derived from it), and
//! publishes the document at checkpoint — on Native directories, a
//! shared `MemoryVfs`, and `.cl` bundles, with both snapshot codecs.

mod common;

use std::sync::Arc;
use std::time::Duration;

use cypherlite::{Database, OpenOptions, Value};
use cypherlite_storage::{read_to_vec, MemoryVfs, SnapshotCodecKind, Vfs, VfsOpen};

use common::{
    active_segment, assert_checkpoint_format, err_text, get_doc, magics, ontology_set_frame,
    seed_store, set_doc, store_vfs,
};

/// A small main-shape document (the gate URI, one node entry with two
/// properties, one relationship entry).
const DOC: &str = concat!(
    "{\"schema\":\"https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json\",",
    "\"id\":\"twin\",\"version\":\"1\",\"name\":\"Twin\",",
    "\"nodes\":{\"Person\":{\"label\":\"Person\",\"properties\":{",
    "\"id\":{\"type\":\"STRING\",\"key\":true},",
    "\"tags\":{\"type\":\"LIST<STRING>\"}}}},",
    "\"relationships\":{\"KNOWS\":{\"type\":\"KNOWS\",",
    "\"from\":{\"node\":\"Person\"},",
    "\"to\":{\"node\":\"Person\",\"max_count\":5}}}}"
);

fn opts(codec: SnapshotCodecKind) -> OpenOptions {
    OpenOptions {
        checkpoint_interval: Some(Duration::ZERO),
        snapshot_codec: Some(codec),
        ..OpenOptions::default()
    }
}

fn roundtrip_set_reopen(reopen: impl Fn() -> Database, db: Database) {
    assert_eq!(db.ontology_document().unwrap(), None);
    db.commit_ontology_set(DOC.to_string()).unwrap();
    assert_eq!(db.ontology_document().unwrap().as_deref(), Some(DOC));
    db.checkpoint().unwrap();
    drop(db);

    let db = reopen();
    assert_eq!(db.ontology_document().unwrap().as_deref(), Some(DOC));
}

fn roundtrip_drop_reopen(reopen: impl Fn() -> Database, db: Database) {
    db.commit_ontology_set(DOC.to_string()).unwrap();
    db.checkpoint().unwrap();
    db.commit_ontology_drop().unwrap();
    assert_eq!(db.ontology_document().unwrap(), None);
    db.checkpoint().unwrap();
    drop(db);

    let db = reopen();
    assert_eq!(db.ontology_document().unwrap(), None);
}

/// The roundtrip matrix every store backend runs: both compiled codecs, each
/// with the set and the drop scenario. `open` is the backend's only
/// contribution — its storage setup, returning the closure that reopens the
/// store it created (the closure owns the store location, so a temp dir or
/// `MemoryVfs` outlives the reopens).
fn roundtrips_both_codecs<R>(open: impl Fn(SnapshotCodecKind) -> R)
where
    R: Fn() -> Database,
{
    for &codec in SnapshotCodecKind::compiled() {
        let reopen = open(codec);
        roundtrip_set_reopen(&reopen, reopen());
        roundtrip_drop_reopen(&reopen, reopen());
    }
}

#[test]
fn native_dir_roundtrips_both_codecs() {
    common::preview_ontology();
    roundtrips_both_codecs(|codec| {
        let dir = tempfile::tempdir().unwrap();
        move || Database::open_with(dir.path().to_str().unwrap(), opts(codec)).unwrap()
    });
}

#[test]
fn memory_vfs_roundtrips_both_codecs() {
    common::preview_ontology();
    roundtrips_both_codecs(|codec| {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        move || Database::open_vfs(Arc::clone(&vfs), opts(codec)).unwrap()
    });
}

#[test]
fn bundle_roundtrips_both_codecs() {
    common::preview_ontology();
    roundtrips_both_codecs(|codec| {
        let dir = tempfile::tempdir().unwrap();
        let uri = dir.path().join("graph.cl");
        move || Database::open_with(uri.to_str().unwrap(), opts(codec)).unwrap()
    });
}

/// A set that no checkpoint covered yet is WAL-pending: the reopen folds
/// the replayed `OntologySet` into the store state (ADR-0120 §3). On a
/// shared `MemoryVfs` automatic checkpoints never dump, so the document
/// reaches the reopen only through the WAL — on a native store the set's
/// force-rotation leaves a closed segment, which schedules a background
/// checkpoint and makes the same scenario racy.
#[test]
fn set_without_checkpoint_survives_reopen_via_wal() {
    common::preview_ontology();
    for &codec in SnapshotCodecKind::compiled() {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        {
            let db = Database::open_vfs(Arc::clone(&vfs), opts(codec)).unwrap();
            db.commit_ontology_set(DOC.to_string()).unwrap();
            // No checkpoint: the document lives only in the WAL.
            db.leak_without_checkpoint();
        }
        let db = Database::open_vfs(Arc::clone(&vfs), opts(codec)).unwrap();
        assert_eq!(db.ontology_document().unwrap().as_deref(), Some(DOC));
    }
}

/// Segment magic follows the ontology state (ADR-0120 §4/§8): the set
/// rotates to `MDBWAL02`; after the drop, a checkpoint and the WAL recycle
/// the store is back at manifest v3 / `value_encoding` 2 with `MDBWAL01`
/// segments only.
#[test]
fn drop_checkpoint_and_recycle_returns_the_store_to_the_plain_format() {
    common::preview_ontology();
    for &codec in SnapshotCodecKind::compiled() {
        let dir = tempfile::tempdir().unwrap();
        let uri = dir.path().to_str().unwrap().to_string();

        let db = Database::open_with(&uri, opts(codec)).unwrap();
        db.commit_ontology_set(DOC.to_string()).unwrap();
        // The set's force-rotation leaves a closed segment, which schedules
        // a background checkpoint; this explicit checkpoint drains it, so
        // everything below is deterministic. The store now publishes the
        // document and every remaining segment is MDBWAL02.
        db.checkpoint().unwrap();
        assert_checkpoint_format(dir.path(), codec, true, Some(DOC));
        assert!(magics(dir.path()).iter().all(|m| m == b"MDBWAL02"));

        db.commit_ontology_drop().unwrap();
        db.checkpoint().unwrap();
        assert_checkpoint_format(dir.path(), codec, false, None);
        assert_eq!(magics(dir.path()), vec![*b"MDBWAL01"]);

        drop(db);
        let db = Database::open_with(&uri, opts(codec)).unwrap();
        assert_eq!(db.ontology_document().unwrap(), None);
    }
}

/// A clone publishes the source's ontology document into the destination
/// (ADR-0120 §7 through `publish_cut`).
#[test]
fn clone_carries_the_ontology_document() {
    common::preview_ontology();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source").to_str().unwrap().to_string();
    let dest = dir.path().join("dest").to_str().unwrap().to_string();
    {
        let db = Database::open(&source).unwrap();
        db.commit_ontology_set(DOC.to_string()).unwrap();
        db.checkpoint().unwrap();
    }
    Database::clone(&source, &dest, Default::default()).unwrap();
    let db = Database::open(&dest).unwrap();
    assert_eq!(db.ontology_document().unwrap().as_deref(), Some(DOC));
}

/// An invalid document is refused before any WAL write, and the store
/// state is untouched. ADR-0229 §3: validation is ADR-0227 §4–§7's parse,
/// structural validation and version gate; only the derived-graph build
/// check is gone with the derived graph.
#[test]
fn invalid_document_is_refused() {
    common::preview_ontology();
    let db = Database::open(":memory:").unwrap();
    let err = err_text(
        db.commit_ontology_set("{\"schema\":\"wrong\",\"id\":\"x\",\"version\":\"1\"}".to_string()),
    );
    assert!(err.contains("Unsupported ontology spec version"), "{err}");
    // A bare scalar is valid YAML but not a document.
    let err = err_text(db.commit_ontology_set("not a document".to_string()));
    assert!(err.contains("must be an object"), "{err}");
    assert_eq!(db.ontology_document().unwrap(), None);
}

/// ADR-0229 §3: no graph is derived from the stored document, so a
/// `system_ontologies` session sees nothing queryable after a set — the
/// document is reachable only through the `db.ontology.*` procedures.
#[test]
fn stored_document_derives_no_queryable_graph() {
    common::preview_ontology();
    let db = Database::open(":memory:").unwrap();
    db.commit_ontology_set(DOC.to_string()).unwrap();
    let mut conn = db.connect().unwrap();
    for cypher in [
        "USE system_ontologies MATCH (n) RETURN count(n) AS c",
        "USE system_ontologies MATCH (n:Ontology) RETURN n.id AS id",
        "USE system_ontologies SHOW CURRENT GRAPH TYPE",
    ] {
        let err = err_text(conn.query(cypher));
        assert!(
            err.ends_with(
                "This database is not queryable. Use the db.ontology.* procedures on the user database 'neo4j' instead."
            ),
            "{cypher}: {err}"
        );
    }
    // The procedures on `neo4j` still reach it.
    let result = conn
        .query("CALL db.ontology.get('json') YIELD ontology RETURN ontology")
        .unwrap();
    assert_eq!(result.rows[0][0], Value::String(DOC.into()));
}

/// A pre-bump document: the draft's old URI, an integer `version` and the
/// renamed `mustExist` flag — every one rejected by today's `set` gate and
/// shape (ADR-0364 §1/§2), but all of them valid syntax in the value model.
const PRE_BUMP_DOC: &str = concat!(
    "{\"schema\":\"https://neo4j.com/ontology-graph-spec/1.0.0/schema.json\",",
    "\"id\":\"legacy\",\"version\":1,",
    "\"nodes\":{\"Person\":{\"label\":\"Person\",\"properties\":{",
    "\"id\":{\"type\":\"STRING\",\"mustExist\":true}}}}}"
);

/// ADR-0364 §4: the gate and structural validation are `set`-time policy,
/// not a property of stored documents. A store written by a pre-bump binary
/// (seeded through a raw `OntologySet` WAL record rather than the public
/// `set`, which refuses it) opens with only the syntax screen, `get`
/// returns the bytes verbatim and converts them across formats, `apply`
/// fails at the gate as a semantic error naming the supported URI, and
/// `set` still rejects the same document.
#[test]
fn pre_bump_stored_document_opens_reads_and_fails_at_apply() {
    common::preview_ontology();
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    seed_store(&uri, SnapshotCodecKind::Json);

    // Plant the pre-bump document as the next WAL commit: the seeded
    // commits are lsn 1 (checkpointed) and lsn 2 (tailed). An ontology op
    // is only ever written to an `MDBWAL02` segment (ADR-0120 §4).
    {
        let vfs = store_vfs(&uri);
        let segment = active_segment(&*vfs);
        let mut bytes = read_to_vec(&*vfs, &segment).unwrap();
        bytes[..8].copy_from_slice(b"MDBWAL02");
        bytes.extend_from_slice(&ontology_set_frame(3, 3, PRE_BUMP_DOC));
        let mut file = vfs.open(&segment, VfsOpen::create_truncate()).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    }

    // The open path screens syntax only: the pre-bump document is expected
    // at rest and must not be refused.
    let db = Database::open(uri.to_str().unwrap()).expect("a pre-bump stored document opens");
    assert_eq!(
        db.ontology_document().unwrap().as_deref(),
        Some(PRE_BUMP_DOC)
    );
    let mut conn = db.connect().unwrap();

    // Stored as JSON: `get('json')` returns the stored bytes verbatim.
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get('json') YIELD ontology RETURN ontology"
        ),
        Value::String(PRE_BUMP_DOC.into())
    );
    // Cross-format `get` is syntax-only: the old shapes convert to YAML
    // even though `set` would reject them.
    let yaml = get_doc(
        &mut conn,
        "CALL db.ontology.get() YIELD ontology RETURN ontology",
    );
    let Value::String(yaml) = yaml else {
        panic!("expected a string, got {yaml:?}")
    };
    assert!(
        yaml.contains("https://neo4j.com/ontology-graph-spec/1.0.0/schema.json"),
        "{yaml}"
    );
    assert!(yaml.contains("version: 1"), "{yaml}");
    assert!(yaml.contains("mustExist: true"), "{yaml}");

    // `apply` must interpret the document, so the gate rejects it — as a
    // semantic error naming the old URI and the supported one, not an
    // `internal error`.
    let err = err_text(conn.query("CALL db.ontology.apply()"));
    assert!(
        err.contains(
            "semantic error: Unsupported ontology spec version: \
             'https://neo4j.com/ontology-graph-spec/1.0.0/schema.json'"
        ),
        "{err}"
    );
    assert!(
        err.contains("https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json"),
        "{err}"
    );
    assert!(!err.contains("internal error"), "{err}");
    // The failed apply changed nothing, and `get` keeps the bytes.
    let result = conn
        .query("SHOW CURRENT GRAPH TYPE YIELD specification RETURN specification")
        .unwrap();
    assert_eq!(result.rows[0][0], Value::String("{}".into()));
    assert_eq!(
        get_doc(
            &mut conn,
            "CALL db.ontology.get('json') YIELD ontology RETURN ontology"
        ),
        Value::String(PRE_BUMP_DOC.into())
    );

    // `set` still gates the same document: the hard cutover has no
    // migration and no alias, and the stored bytes are untouched.
    let err = err_text(set_doc(&mut conn, PRE_BUMP_DOC));
    assert!(
        err.contains(
            "Unsupported ontology spec version: \
             'https://neo4j.com/ontology-graph-spec/1.0.0/schema.json'"
        ),
        "{err}"
    );
    assert_eq!(
        db.ontology_document().unwrap().as_deref(),
        Some(PRE_BUMP_DOC)
    );
}
