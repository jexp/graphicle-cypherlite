//! The raw-WAL duplicate seeding: a replay duplicate is the only state that
//! can carry two holders now that admission rejects the second (ADR-0439);
//! its report-on-touch behavior is ADR-0172's.

use std::collections::BTreeMap;
use std::path::Path;

use cypherlite::{Database, Value, WalRecord};
use cypherlite_executor::RedoOp;
use cypherlite_storage::{SnapshotCodecKind, VfsOpen};

use crate::common::{
    active_segment, err_text, scalar, seed_store_with, store_vfs, uniqueness, wal_frame,
};
use crate::support::UNIQUE;

/// Seed a legacy/replay duplicate: two committed entities holding the same
/// constrained tuple, a state only the replay path can produce now that the
/// commit loop rejects second holders. The constraint is checkpointed first;
/// a raw WAL frame carrying two `CREATE (:P {id: 77})` nodes is then
/// appended, so `Database::open` records both (replay is a state recorder,
/// ADR-0439) and `rebuild_indexes` backfills both holders — the executor's
/// `rebuild_carries_replay_duplicates_forward` fixture at the store level.
fn seed_replay_duplicate(root: &Path) {
    seed_store_with(root, SnapshotCodecKind::Json, &[UNIQUE]);

    let duplicate = |id: i64| {
        RedoOp::create_node(
            id,
            vec!["P".to_string()],
            BTreeMap::from([("id".to_string(), Value::Int(77))]),
        )
    };
    // `seed_store_with` commits three times (the checkpointed node, the
    // constraint, the tailed node), so the frame sits ABOVE the tailed
    // commit's lsn/commit_ts 3; both must be strictly above it, which the
    // WAL scanner enforces for `commit_ts`.
    let record = WalRecord {
        lsn: 4,
        commit_ts: 4,
        // Above the checkpointed `:T` node (id 0) and the WAL-tailed one (id 1).
        ops: vec![duplicate(2), duplicate(3)],
    };
    let frame = wal_frame(&serde_json::to_vec(&record).unwrap());
    let vfs = store_vfs(root);
    let segment = active_segment(&*vfs);
    let mut file = vfs.open(&segment, VfsOpen::append_create()).unwrap();
    file.write_all(&frame).unwrap();
    file.sync_all().unwrap();
}

/// ADR-0172's report-on-touch behavior, unchanged by ADR-0439: a duplicate
/// that only the replay path can produce is reported to writes that touch
/// an involved entity, while unrelated writes succeed. The old test seeded
/// the duplicate through two concurrent commits; ADR-0439 now rejects the
/// second at admission, so the duplicate is replayed from a WAL tail.
#[test]
fn pre_existing_violation_fails_only_writes_that_touch_it() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    seed_replay_duplicate(&uri);
    let db = Database::open(uri.to_str().unwrap()).unwrap();
    let mut c = db.connect().unwrap();
    // Both holders arrived through replay; admission never installs one.
    assert_eq!(
        scalar(&mut c, "MATCH (n:P {id: 77}) RETURN count(n)"),
        Value::Int(2)
    );
    // Unrelated writes succeed (the old full scan failed these).
    c.query("CREATE (:Other)").unwrap();
    c.query("CREATE (:P {id: 78})").unwrap();
    // Writes touching an involved entity still fail with the precise
    // violation.
    assert_eq!(
        err_text(c.query("MATCH (n:P {id: 77}) SET n.touched = true")),
        uniqueness("p_id")
    );
    assert_eq!(
        err_text(c.query("CREATE (:P {id: 77})")),
        uniqueness("p_id")
    );
}
