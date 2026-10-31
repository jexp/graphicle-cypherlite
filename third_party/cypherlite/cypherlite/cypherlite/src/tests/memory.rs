use std::time::Duration;

use crate::{Database, Error, OpenOptions, Value};

#[test]
fn range_index_moves_memory_usage() {
    let plain = {
        let db = Database::open(":memory:").unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE (:Flat {id: 1})").unwrap();
        db.memory_usage().unwrap().0
    };
    let indexed = {
        let db = Database::open(":memory:").unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE RANGE INDEX Flat_id FOR (n:Flat) ON (n.id)")
            .unwrap();
        c.query("CREATE (:Flat {id: 1})").unwrap();
        db.memory_usage().unwrap().0
    };
    assert!(
        indexed > plain,
        "plain={plain} indexed={indexed} delta={}",
        indexed as i64 - plain as i64
    );
}

#[test]
fn logical_graph_size_limit_rejects_growing_writes_reads_ok() {
    let db = Database::open_with(
        ":memory:",
        OpenOptions {
            wal_dir: None,
            checkpoint_interval: None,
            durability_mode: Default::default(),
            logical_graph_size_limit: Some(1),
            snapshot_codec: None,
            load_csv_dir: None,
            load_csv_max_bytes: None,
            mode: crate::OpenMode::Writable,

            token_admission: false,
            concurrent_logical_workspace_limit: None,
            follow_gap: Default::default(),
        },
    )
    .unwrap();
    let mut c = db.connect().unwrap();
    let err = c.query("CREATE (n:Person {name: 'Ada'})").unwrap_err();
    assert!(
        matches!(
            err,
            Error::Capacity {
                used: 258,
                limit: 1
            }
        ),
        "{err}"
    );
    let r = c.query("RETURN 1 AS n").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(1));
    let (used, limit) = db.memory_usage().unwrap();
    // Two namespace graphs (ADR-0229 dropped the `system_ontologies`
    // slot), each interning "*" and "".
    assert_eq!(used, 258);
    assert_eq!(limit, Some(1));
}

/// Fill until the cap, delete every node, then create one more. `index` is
/// run first when set, so the fill charges the indexed path. `expected_nodes`
/// is how many creates succeed before `Error::Capacity`; a change in the
/// per-node account moves this count.
fn logical_graph_size_limit_delete_then_create_recovers_with(
    limit_bytes: u64,
    index: Option<&str>,
    expected_nodes: u32,
) {
    let db = Database::open_with(
        ":memory:",
        OpenOptions {
            wal_dir: None,
            checkpoint_interval: None,
            durability_mode: Default::default(),
            logical_graph_size_limit: Some(limit_bytes),
            snapshot_codec: None,
            load_csv_dir: None,
            load_csv_max_bytes: None,
            mode: crate::OpenMode::Writable,

            token_admission: false,
            concurrent_logical_workspace_limit: None,
            follow_gap: Default::default(),
        },
    )
    .unwrap();
    let mut c = db.connect().unwrap();
    if let Some(index) = index {
        c.query(index).unwrap();
    }
    let mut created = 0u32;
    loop {
        match c.query("CREATE (n:Person {name: 'xxxxxxxx'})") {
            Ok(_) => created += 1,
            Err(Error::Capacity { used, limit }) => {
                assert!(used <= limit, "used={used} limit={limit}");
                break;
            }
            Err(e) => panic!("{e}"),
        }
        assert!(created < 200, "should hit the cap");
    }
    assert_eq!(created, expected_nodes, "used bytes per node changed");
    c.query("MATCH (n:Person) RETURN count(n) AS n").unwrap();
    c.query("MATCH (n:Person) DETACH DELETE n").unwrap();
    // Tombstones stay in the floor until checkpoint GC.
    let held = c.query("CREATE (n:Person {name: 'xxxxxxxx'})");
    assert!(
        matches!(held, Err(Error::Capacity { .. })),
        "delete must not free the floor before GC: {held:?}"
    );
    db.checkpoint().unwrap();
    c.query("CREATE (n:Person {name: 'ok'})").unwrap();
    let (used, limit) = db.memory_usage().unwrap();
    assert_eq!(limit, Some(limit_bytes));
    assert!(used > 0 && used <= limit_bytes, "used={used}");
}

#[test]
fn logical_graph_size_limit_delete_then_create_recovers() {
    // Fifteen unindexed string nodes fit in 5_000; the sixteenth does not.
    // The shared :Person label vector is counted once, so one more node
    // fits than when every node owned its label list.
    logical_graph_size_limit_delete_then_create_recovers_with(5_000, None, 15);
}

#[test]
fn logical_graph_size_limit_delete_then_create_recovers_text_index() {
    // Width-3 postings on `xxxxxxxx` (6 trigrams): 144 fit in 50_000, the 145th
    // does not. Was 145 before the exact label-count map (LABS-370 v1 follow-up):
    // the first :Person create charges its LABEL_COUNT_ENTRY_BYTES entry, so
    // one fewer node fits under the same cap.
    logical_graph_size_limit_delete_then_create_recovers_with(
        50_000,
        Some("CREATE TEXT INDEX person_name FOR (n:Person) ON (n.name)"),
        144,
    );
}

#[test]
fn logical_graph_size_limit_concurrent_reserve() {
    use std::sync::{Arc, Barrier};
    let probe = Database::open(":memory:").unwrap();
    let empty = probe.memory_usage().unwrap().0;
    {
        let mut c = probe.connect().unwrap();
        c.query("CREATE (n:T {s: 'abcdefghijklmnopqrstuvwxyz0123456789'})")
            .unwrap();
    }
    let (one, _) = probe.memory_usage().unwrap();
    drop(probe);
    // Room for the resident node plus half of another. The empty-server
    // floor is already inside `one`, so the slack is half the marginal node.
    let marginal = one - empty;
    let limit = one + marginal / 2;
    let db = Arc::new(
        Database::open_with(
            ":memory:",
            OpenOptions {
                wal_dir: None,
                checkpoint_interval: None,
                durability_mode: Default::default(),
                logical_graph_size_limit: Some(limit),
                snapshot_codec: None,
                load_csv_dir: None,
                load_csv_max_bytes: None,
                mode: crate::OpenMode::Writable,

                token_admission: false,
                concurrent_logical_workspace_limit: None,
                follow_gap: Default::default(),
            },
        )
        .unwrap(),
    );
    let barrier = Arc::new(Barrier::new(2));
    let mut hs = Vec::new();
    for _ in 0..2 {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        hs.push(std::thread::spawn(move || {
            barrier.wait();
            let mut c = db.connect().unwrap();
            c.query("CREATE (n:T {s: 'abcdefghijklmnopqrstuvwxyz0123456789'})")
        }));
    }
    let results: Vec<_> = hs.into_iter().map(|h| h.join().unwrap()).collect();
    let oks = results.iter().filter(|r| r.is_ok()).count();
    let caps = results
        .iter()
        .filter(|r| matches!(r, Err(Error::Capacity { .. })))
        .count();
    assert_eq!(oks, 1, "exactly one commit: {results:?}");
    assert_eq!(caps, 1, "the other is Capacity: {results:?}");
}

#[test]
fn logical_graph_size_limit_open_over_limit_replay_then_reject() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE (n:Person {name: 'Ada'})").unwrap();
    }
    let db = Database::open_with(
        path,
        OpenOptions {
            wal_dir: None,
            checkpoint_interval: Some(Duration::ZERO),
            durability_mode: Default::default(),
            logical_graph_size_limit: Some(1),
            snapshot_codec: None,
            load_csv_dir: None,
            load_csv_max_bytes: None,
            mode: crate::OpenMode::Writable,

            token_admission: false,
            concurrent_logical_workspace_limit: None,
            follow_gap: Default::default(),
        },
    )
    .unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("Ada"));
    let (used, limit) = db.memory_usage().unwrap();
    assert!(used > 1, "replayed snapshot used={used}");
    assert_eq!(limit, Some(1));
    let err = c.query("CREATE (n:Person {name: 'Bob'})").unwrap_err();
    assert!(matches!(err, Error::Capacity { limit: 1, .. }), "{err}");
}

#[test]
fn logical_graph_size_limit_zero_is_unlimited() {
    let db = Database::open_with(
        ":memory:",
        OpenOptions {
            wal_dir: None,
            checkpoint_interval: None,
            durability_mode: Default::default(),
            logical_graph_size_limit: Some(0),
            snapshot_codec: None,
            load_csv_dir: None,
            load_csv_max_bytes: None,
            mode: crate::OpenMode::Writable,

            token_admission: false,
            concurrent_logical_workspace_limit: None,
            follow_gap: Default::default(),
        },
    )
    .unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:Person {name: 'Ada'})").unwrap();
    let (used, limit) = db.memory_usage().unwrap();
    assert!(used > 0);
    assert_eq!(limit, None);
}

/// LABS-370 (security follow-up): the commit admission estimate includes
/// the count family's stats-map entry growth — one rel between two
/// multi-label nodes materializes labels(start) × labels(end) pair entries,
/// and that growth is charged BEFORE the commit applies, so the write is
/// rejected with `Capacity` at admission instead of landing past the cap.
#[test]
fn logical_graph_size_limit_rejects_stats_entry_growth_pre_commit() {
    let open = |limit: u64| {
        Database::open_with(
            ":memory:",
            OpenOptions {
                wal_dir: None,
                checkpoint_interval: None,
                durability_mode: Default::default(),
                logical_graph_size_limit: Some(limit),
                snapshot_codec: None,
                load_csv_dir: None,
                load_csv_max_bytes: None,
                mode: crate::OpenMode::Writable,
                token_admission: false,
                concurrent_logical_workspace_limit: None,
                follow_gap: Default::default(),
            },
        )
        .unwrap()
    };
    // Measure the fill phase under a generous cap: two 8-label nodes.
    let fill = "CREATE (a:X1:X2:X3:X4:X5:X6:X7:X8), (b:Y1:Y2:Y3:Y4:Y5:Y6:Y7:Y8)";
    let probe = open(u64::MAX / 4);
    let mut c = probe.connect().unwrap();
    c.query(fill).unwrap();
    let (used, limit) = probe.memory_usage().unwrap();
    assert_eq!(limit, Some(u64::MAX / 4));

    // Reopen with the cap set between the fill and the rel: the rel's own
    // bytes fit the headroom, its 8×8 pair-entry (plus 8 histogram-entry)
    // growth does not — pre-commit admission rejects it.
    let db = open(used + 4_000);
    let mut c = db.connect().unwrap();
    c.query(fill).unwrap();
    let rel = "MATCH (a:X1) MATCH (b:Y1) CREATE (a)-[:R]->(b)";
    let rejected = c.query(rel);
    assert!(
        matches!(rejected, Err(Error::Capacity { .. })),
        "the stats entry growth must reject the rel at admission: {rejected:?}"
    );
    // The cap is not merely exhausted: a small write still fits.
    c.query("CREATE (z:Z1)").unwrap();
    // And under a headroom that covers the growth, the same rel lands.
    let db = open(used + 40_000);
    let mut c = db.connect().unwrap();
    c.query(fill).unwrap();
    c.query(rel).unwrap();
}
