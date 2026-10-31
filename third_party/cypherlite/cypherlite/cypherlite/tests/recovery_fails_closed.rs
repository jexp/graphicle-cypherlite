//! ADR-0114 at the public `Database::open` boundary: recovery fails closed.
//!
//! Decision 1: a CRC-valid WAL frame whose payload does not decode as
//! `WalRecord` (here an `op` variant only a newer CypherLite knows) fails
//! `Database::open` with a storage error instead of being trimmed as a
//! tolerated tail. Trimming it would silently drop a committed transaction.
//!
//! Decision 2: an unsupported snapshot version fails open before WAL
//! recovery may trim the torn tail on the active segment.
//!
//! In both cases the error names the offending WAL segment, and the active
//! WAL segment bytes stay unchanged across repeated open attempts.

mod common;

use std::path::Path;

use cypherlite::Database;
use cypherlite_core::Error;
use cypherlite_storage::{read_to_vec, SnapshotCodecKind, Vfs, VfsOpen};

use common::{active_segment, seed_store, store_vfs, wal_frame};

const UNDECODABLE_PHRASE: &str = "may have been written by a newer CypherLite";

/// A minimal valid main-shape ontology document (ADR-0364's gate and
/// schema): the open path validates the persisted document (ADR-0120 §1 —
/// no graph is derived from it, ADR-0229 §3, but open still surfaces a
/// corrupt record instead of deferring the failure), so a planted document
/// must parse.
const VALID_ONTOLOGY: &str = concat!(
    "{\"schema\":\"https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json\",",
    "\"id\":\"test\",\"version\":\"1\"}"
);

/// An incomplete final frame (a torn tail recovery would otherwise trim).
const TORN_TAIL: &[u8] = &[0xDE, 0xAD, 0xBE];

/// A complete WAL frame whose CRC matches but whose payload names an unknown
/// `RedoOp` variant, as a newer CypherLite would write it. The tag must be
/// genuinely unknown: `OntologySet` is a real op since ADR-0120.
fn unknown_op_frame(lsn: u64, commit_ts: u64) -> Vec<u8> {
    let payload = serde_json::json!({
        "lsn": lsn,
        "commit_ts": commit_ts,
        "ops": [{"op": "MdbFutureOp", "database": "neo4j", "spec": "{}"}],
    })
    .to_string();
    wal_frame(payload.as_bytes())
}

/// The bytes of a store-relative object (a WAL segment, a snapshot file,
/// an ontology document — anything [`read_to_vec`] can read).
fn read_object(vfs: &dyn Vfs, name: &str) -> Vec<u8> {
    read_to_vec(vfs, name).unwrap()
}

/// Append bytes to a store-relative file.
fn append_to_segment(vfs: &dyn Vfs, segment: &str, bytes: &[u8]) {
    let mut file = vfs.open(segment, VfsOpen::append_create()).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

/// Read a store-relative snapshot JSON object, apply `edit`, and write it
/// back. Works for Native directory stores and `.cl` bundles alike.
fn edit_store_json(root: &Path, virt_path: &str, edit: impl FnOnce(&mut serde_json::Value)) {
    let vfs = store_vfs(root);
    let mut data: serde_json::Value =
        serde_json::from_slice(&read_to_vec(&*vfs, virt_path).unwrap()).unwrap();
    edit(&mut data);
    let mut file = vfs.open(virt_path, VfsOpen::create_truncate()).unwrap();
    file.write_all(&serde_json::to_vec(&data).unwrap()).unwrap();
    file.sync_all().unwrap();
}

/// Opening the store must fail twice with `Error::Storage` containing every
/// string in `messages`, and the WAL segment bytes must stay unchanged
/// throughout.
fn assert_open_fails_closed(root: &Path, segment: &str, messages: &[&str]) {
    let before = read_object(&*store_vfs(root), segment);
    for _ in 0..2 {
        let err = Database::open(root.to_str().unwrap())
            .err()
            .expect("open must fail closed");
        assert!(matches!(err, Error::Storage(_)), "{err}");
        for message in messages {
            assert!(err.to_string().contains(message), "{err}");
        }
        assert_eq!(
            read_object(&*store_vfs(root), segment),
            before,
            "WAL segment must stay byte-identical"
        );
    }
}

/// Decision 1 against a seeded store: append the unknown-op frame to the
/// active segment, then assert open fails closed with an error that names
/// the segment holding the undecodable record.
fn assert_undecodable_record_fails_closed(root: &Path) {
    let vfs = store_vfs(root);
    let segment = active_segment(&*vfs);
    append_to_segment(&*vfs, &segment, &unknown_op_frame(2, 2));
    drop(vfs);
    let segment_name = segment
        .rsplit('/')
        .next()
        .expect("a segment path has a file name");
    assert_open_fails_closed(root, &segment, &[UNDECODABLE_PHRASE, segment_name]);
}

#[test]
fn native_json_store_rejects_undecodable_wal_record() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    seed_store(&uri, SnapshotCodecKind::Json);
    assert_undecodable_record_fails_closed(&uri);
}

#[cfg(feature = "parquet")]
#[test]
fn native_parquet_store_rejects_undecodable_wal_record() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    seed_store(&uri, SnapshotCodecKind::Parquet);
    assert_undecodable_record_fails_closed(&uri);
}

#[cfg(feature = "parquet")]
#[test]
fn bundle_store_rejects_undecodable_wal_record() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("graph.cl");
    seed_store(&uri, SnapshotCodecKind::Parquet);
    assert_undecodable_record_fails_closed(&uri);
}

#[test]
fn json_value_encoding_gate_precedes_wal_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    seed_store(&uri, SnapshotCodecKind::Json);

    let vfs = store_vfs(&uri);
    let segment = active_segment(&*vfs);
    append_to_segment(&*vfs, &segment, TORN_TAIL);
    drop(vfs);

    edit_store_json(&uri, "graph.json", |graph| {
        graph["value_encoding"] = serde_json::Value::from(4);
    });

    assert_open_fails_closed(&uri, &segment, &["unsupported graph value encoding: 4"]);
}

#[cfg(feature = "parquet")]
#[test]
fn parquet_format_version_gate_precedes_wal_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    seed_store(&uri, SnapshotCodecKind::Parquet);

    let vfs = store_vfs(&uri);
    let segment = active_segment(&*vfs);
    append_to_segment(&*vfs, &segment, TORN_TAIL);
    drop(vfs);

    edit_store_json(&uri, "manifest.json", |manifest| {
        manifest["format_version"] = serde_json::Value::from(5);
    });

    assert_open_fails_closed(&uri, &segment, &["unsupported snapshot version 5"]);
}

/// A read-only open of `root` must succeed, serve the seeded rows, and
/// leave `objects` and the active WAL segment byte-identical (ADR-0120 §8:
/// a store holding an ontology opens without being rewritten).
fn assert_readonly_open_preserves_store(root: &Path, objects: &[&str]) {
    let (before, segment, segment_before) = {
        let vfs = store_vfs(root);
        let before: Vec<Vec<u8>> = objects
            .iter()
            .map(|name| read_object(&*vfs, name))
            .collect();
        let segment = active_segment(&*vfs);
        let segment_before = read_object(&*vfs, &segment);
        (before, segment, segment_before)
    };
    {
        let db = Database::open_with(root.to_str().unwrap(), cypherlite::test_opts::read_only())
            .expect("an ontology-carrying store opens read-only");
        let mut conn = db.connect().unwrap();
        let r = conn.query("MATCH (n:T) RETURN count(n)").unwrap();
        assert_eq!(r.rows, vec![vec![cypherlite::Value::Int(2)]]);
    }
    let vfs = store_vfs(root);
    for (name, bytes) in objects.iter().zip(&before) {
        assert_eq!(
            &read_object(&*vfs, name),
            bytes,
            "{name} must stay byte-identical"
        );
    }
    assert_eq!(
        read_object(&*vfs, &segment),
        segment_before,
        "WAL segment must stay byte-identical"
    );
}

/// ADR-0120 §6: a `graph.json` at `value_encoding` 3 with the inline
/// ontology is the current format, not a future one.
#[test]
fn json_value_encoding_3_with_ontology_opens_read_only_unmodified() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    seed_store(&uri, SnapshotCodecKind::Json);

    edit_store_json(&uri, "graph.json", |graph| {
        graph["value_encoding"] = serde_json::Value::from(3);
        graph["ontology"] = serde_json::Value::from(VALID_ONTOLOGY);
    });

    assert_readonly_open_preserves_store(&uri, &["graph.json", "manifest.json"]);
}

/// ADR-0120 §5: a v4 manifest selecting an immutable ontology object is
/// the current format, not a future one.
#[cfg(feature = "parquet")]
#[test]
fn parquet_v4_manifest_with_ontology_opens_read_only_unmodified() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    seed_store(&uri, SnapshotCodecKind::Parquet);

    let ontology_path = "ontology/0123456789abcdef0123456789abcdef.json";
    {
        let vfs = store_vfs(&uri);
        vfs.create_dir_all("ontology").unwrap();
        let mut file = vfs.open(ontology_path, VfsOpen::create_truncate()).unwrap();
        file.write_all(VALID_ONTOLOGY.as_bytes()).unwrap();
        file.sync_all().unwrap();
        vfs.sync_dir("ontology").unwrap();
    }
    edit_store_json(&uri, "manifest.json", |manifest| {
        manifest["format_version"] = serde_json::Value::from(4);
        manifest["ontology_path"] = serde_json::Value::from(ontology_path);
    });

    assert_readonly_open_preserves_store(&uri, &["manifest.json", ontology_path]);
}
