//! Relationship uniqueness across a close/reopen of the directory store,
//! from the WAL and from a checkpoint.

use cypherlite::Value;

use crate::common::{err_text, scalar, uniqueness};
use crate::support::REL_UNIQUE;

/// Close and reopen the directory store at `uri` — the earlier relationship
/// in a checkpoint taken through the public API when `checkpoint` is set,
/// only in the WAL otherwise — and check that relationship `unique` still
/// rejects its duplicate and accepts a distinct value.
fn relationship_uniqueness_survives_reopen(uri: &str, checkpoint: bool) {
    {
        let db = cypherlite::Database::open(uri).unwrap();
        let mut c = db.connect().unwrap();
        c.query(REL_UNIQUE).unwrap();
        c.query("CREATE ()-[:R {id: 1}]->()").unwrap();
        if checkpoint {
            db.checkpoint().unwrap();
        }
        drop(c);
        db.leak_without_checkpoint();
    }
    let db = cypherlite::Database::open(uri).unwrap();
    let mut c = db.connect().unwrap();
    assert_eq!(
        err_text(c.query("CREATE ()-[:R {id: 1}]->()")),
        uniqueness("r_id"),
        "checkpoint={checkpoint}"
    );
    c.query("CREATE ()-[:R {id: 2}]->()").unwrap();
    assert_eq!(
        scalar(&mut c, "MATCH ()-[r:R]->() RETURN count(r)"),
        Value::Int(2),
        "checkpoint={checkpoint}"
    );
}

#[test]
fn relationship_uniqueness_survives_reopen_from_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    relationship_uniqueness_survives_reopen(dir.path().to_str().unwrap(), false);
}

#[test]
fn relationship_uniqueness_survives_reopen_from_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    relationship_uniqueness_survives_reopen(dir.path().to_str().unwrap(), true);
}
