//! The ontology document depth cap at the public `Database` boundary.
//!
//! (a) `CALL db.ontology.set($doc)` with a document nesting deeper than
//!     128 levels fails with a typed semantic error and leaves the store
//!     unchanged.
//! (b) A store whose WAL carries a too-deep `OntologySet` record fails
//!     `Database::open` with a typed error — not a stack-overflow abort.
//!     saphyr recurses per nesting level with no cap, so a `"- "` ×
//!     100_000 document (well under the 16 MiB envelope) aborts the
//!     process without the pre-screen in `cypherlite-ontology`.
//!
//! Frame-crafting: the shared `ontology_set_frame` helper in
//! `common/mod.rs` (same CRC/length framing as `unknown_op_frame` in
//! `recovery_fails_closed.rs`; here the record decodes fine — the planted
//! document is the payload).

mod common;

use std::collections::BTreeMap;

use cypherlite::{Database, Value};
use cypherlite_storage::{read_to_vec, SnapshotCodecKind, VfsOpen};

use common::{active_segment, err_text, ontology_set_frame, seed_store, store_vfs};

/// The typed error text the depth pre-screen raises (`OntologyError`).
const DEPTH_ERROR: &str = "nesting exceeds the 128-level depth limit";

/// The attack payload: 100_000 nested block sequences on one line
/// (saphyr's compact `- `-run notation) — ~200 KB, far under the 16 MiB
/// document cap; only the depth screen stops it.
fn deep_yaml() -> String {
    format!("{}1", "- ".repeat(100_000))
}

#[test]
fn ontology_set_rejects_deep_document_and_store_stays_unchanged() {
    common::preview_ontology();
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();

    let err = err_text(conn.query_with_params(
        "CALL db.ontology.set($ontology)",
        BTreeMap::from([("ontology".to_string(), Value::String(deep_yaml().into()))]),
    ));
    assert!(err.contains(DEPTH_ERROR), "{err}");

    // The failed set changed nothing: the store still holds no ontology.
    let result = conn
        .query("CALL db.ontology.get() YIELD ontology RETURN ontology")
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Null]]);
}

#[test]
fn deep_ontology_in_wal_fails_open_with_typed_error() {
    common::preview_ontology();
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    seed_store(&uri, SnapshotCodecKind::Json);

    {
        let vfs = store_vfs(&uri);
        let segment = active_segment(&*vfs);
        let mut bytes = read_to_vec(&*vfs, &segment).unwrap();
        // ADR-0120 §4: an ontology op is only ever written to an
        // `MDBWAL02` segment — the scanner rejects it otherwise — so the
        // hand-crafted segment carries the ontology magic.
        bytes[..8].copy_from_slice(b"MDBWAL02");
        // The seeded commits are lsn 1 (checkpointed) and lsn 2 (tailed);
        // the planted ontology commit is the next record.
        bytes.extend_from_slice(&ontology_set_frame(3, 3, &deep_yaml()));
        let mut file = vfs.open(&segment, VfsOpen::create_truncate()).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    }

    // Open must fail with the typed depth error from replaying the
    // planted document — not a stack-overflow abort. Two attempts: the
    // failed open must leave the WAL byte-identical for the next one.
    for _ in 0..2 {
        let err = Database::open(uri.to_str().unwrap())
            .err()
            .expect("a store with a too-deep ontology in the WAL must fail open");
        assert!(err.to_string().contains(DEPTH_ERROR), "{err}");
    }
}
