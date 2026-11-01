//! The `db.ontology.*` preview gate across a follower reload (ADR-0137 with
//! ADR-0371/ADR-0378).
//!
//! The follower's reload loads a fresh graph from the checkpoint. That graph
//! starts with a default (empty) service bundle, so the reload must re-point
//! it at the store's one bundle (`Graph::share_services_from`) or the gate
//! silently closes on a store whose writer opened with
//! `CYPHERLITE_PREVIEW=ontology`. This crate pins that path: it forces a
//! reload and then asserts discovery and callability still observe the
//! enabled set.
//!
//! `CYPHERLITE_PREVIEW` is process-global and read once per `Database::open`
//! (ADR-0371), so this file is its own test binary: it opts in before the
//! first open and no closed-gate fixture in this package ever sees the
//! variable. The crate must never join a test binary that pins the closed
//! gate.

mod common;

use cypherlite::test_opts::{following, wait_count, writable};
use cypherlite::{Database, Value};

/// The four rows the enabled gate lists (ADR-0121 §2, ADR-0227), in the
/// order the discovery query below returns them.
const ONTOLOGY_ROWS: [&str; 4] = [
    "db.ontology.apply",
    "db.ontology.drop",
    "db.ontology.get",
    "db.ontology.set",
];

/// Both sides of the gate on one follower state: `SHOW PROCEDURES` lists the
/// family (discovery) and `CALL db.ontology.get()` answers instead of the
/// authored disabled-feature error (callability). `when` names the point of
/// the reload in the failure message.
fn assert_gate_open(db: &Database, when: &str) {
    let mut conn = db.connect().unwrap();
    let listed: Vec<Value> = conn
        .query(
            "SHOW PROCEDURES YIELD name \
             WHERE name STARTS WITH 'db.ontology.' RETURN name ORDER BY name",
        )
        .unwrap()
        .rows
        .into_iter()
        .map(|row| row.into_iter().next().expect("the RETURN projects `name`"))
        .collect();
    let expected: Vec<Value> = ONTOLOGY_ROWS
        .iter()
        .map(|name| Value::String((*name).into()))
        .collect();
    assert_eq!(
        listed, expected,
        "{when}: the reload closed the preview gate — SHOW PROCEDURES must \
         still list every db.ontology.* row"
    );

    let rows = conn
        .query("CALL db.ontology.get() YIELD ontology RETURN ontology")
        .unwrap_or_else(|err| {
            panic!("{when}: the reload closed the preview gate — CALL db.ontology.get(): {err}")
        })
        .rows;
    assert_eq!(
        rows,
        vec![vec![Value::Null]],
        "{when}: an empty store's db.ontology.get() returns one null row"
    );
}

/// Cut the active WAL segment back to its magic, the on-disk shape a crashed
/// rotate leaves behind. The follower's cursor then sits past the segment's
/// end, so its next poll reports `WalCatchUp::Gap` and `follow::reload`
/// replaces the graph from the checkpoint. This is the in-crate follow
/// suite's reload fixture (`src/tests/follow.rs`) driven through the store
/// instead of the crate-private pause/park controls.
fn truncate_active_segment(root: &std::path::Path) {
    let vfs = common::store_vfs(root);
    let segment = common::active_segment(&*vfs);
    vfs.open(&segment, cypherlite_storage::VfsOpen::read_write())
        .expect("the active WAL segment must open for writing")
        .set_len(cypherlite_storage::MAGIC.len() as u64)
        .expect("truncating the active WAL segment to its magic must succeed");
}

/// A writer opened with the `ontology` opt-in, followed by a reader that
/// first sees the gate open, then reloads across a WAL gap, then must still
/// see it open. `Tail` lives only in the WAL tail, never in the checkpoint,
/// so it disappearing is the observable proof the reload installed the
/// snapshot — the only path that replaces the follower's graph.
#[test]
fn follow_reload_keeps_the_ontology_preview_gate_open() {
    // Before the first open in this process (ADR-0371 reads the variable
    // once per open); `Once` makes it idempotent, so a future sibling test
    // in this crate may call it again.
    common::preview_ontology();

    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();

    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    writer.connect().unwrap().execute("CREATE (:Tail)").unwrap();

    let follower = Database::open_with(uri, following()).unwrap();
    // The follower replays the un-checkpointed tail at open, so this also
    // pins that it runs against the writer's store before the reload.
    wait_count(&follower, "Tail", 1);
    assert_gate_open(&follower, "before the reload");

    // Close the writer without a checkpoint (so `Tail` stays WAL-only), then
    // destroy the frames the follower's cursor still needs.
    writer.leak_without_checkpoint();
    truncate_active_segment(dir.path());

    wait_count(&follower, "Tail", 0);
    assert_gate_open(&follower, "after the reload");
}
