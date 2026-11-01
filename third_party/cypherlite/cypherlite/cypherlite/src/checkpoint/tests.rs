use super::*;
use crate::{Database, OpenOptions, SnapshotCodecKind, Value};
use std::sync::Arc;
use std::time::Instant;

#[test]
fn constraints_survive_wal_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("constraint-db");
    {
        let db = Database::open(path.to_str().unwrap()).unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE CONSTRAINT person_id FOR (n:Person) REQUIRE n.id IS UNIQUE")
            .unwrap();
        c.query("CREATE (:Person {id: 1})").unwrap();
    }
    let db = Database::open(path.to_str().unwrap()).unwrap();
    let mut c = db.connect().unwrap();
    assert!(c.query("CREATE (:Person {id: 1})").is_err());
}
#[test]
fn json_snapshot_survives_read_only_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let json_opts = || crate::OpenOptions {
        snapshot_codec: Some(SnapshotCodecKind::Json),
        ..Default::default()
    };
    {
        let db = Database::open_with(path, json_opts()).unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE (n:Person {name: 'Ada'})").unwrap();
    }
    // Read-only sessions: no commits, so the WAL pin ts is 0 once the
    // first checkpoint recycled the segment. The dump horizon must stay
    // at the graph's commit ts or the full-rewrite JSON codec drops
    // every snapshot-loaded row.
    for _ in 0..2 {
        let db = Database::open_with(path, json_opts()).unwrap();
        let mut c = db.connect().unwrap();
        let r = c.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
        assert_eq!(r.rows[0][0].as_string(), Some("Ada"));
    }
}

#[cfg(feature = "parquet")]
#[test]
fn parquet_snapshot_survives_read_only_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE (n:Person {name: 'Ada'})").unwrap();
    }
    for _ in 0..2 {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        let r = c.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
        assert_eq!(r.rows[0][0].as_string(), Some("Ada"));
    }
}

#[cfg(feature = "parquet")]
#[test]
fn parquet_checkpoint_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE (n:Person {name: 'Ada'})").unwrap();
        db.checkpoint().unwrap();
    }
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("Ada"));
}

#[test]
fn wal_survives_without_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE (n:Person {name: 'Wal'})").unwrap();
        db.leak_without_checkpoint();
    }
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("Wal"));
}

#[test]
fn checkpoint_recycles_closed_wal_segment() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE (n:Person {name: 'Snap'})").unwrap();
        db.checkpoint().unwrap();
    }
    let wal_dir = std::path::Path::new(path).join("wal");
    let mut names: Vec<_> = std::fs::read_dir(&wal_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(
        !names.iter().any(|n| n == "00000001.log"),
        "closed segment should be recycled, got {names:?}"
    );
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("Snap"));
}
#[test]
fn writes_continue_during_checkpoint() {
    use std::sync::{Arc, Barrier};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let db = Arc::new(Database::open(path).unwrap());
    {
        let mut c = db.connect().unwrap();
        c.query("CREATE (n:Seed {n: 0})").unwrap();
    }
    let barrier = Arc::new(Barrier::new(2));
    let writer = {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            let mut c = db.connect().unwrap();
            for i in 0..40 {
                c.query(&format!("CREATE (n:W {{n: {i}}})")).unwrap();
            }
        })
    };
    let check = {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            db.checkpoint().unwrap();
        })
    };
    writer.join().unwrap();
    check.join().unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:W) RETURN count(n) AS n").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(40));
}

#[test]
fn publication_drains_admitted_checkpoint_before_any_wal_frame() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(Database::open(dir.path().to_str().unwrap()).unwrap());
    db.connect().unwrap().query("CREATE (:Prior)").unwrap();
    let shared = db.shared();
    let mut wal = shared.wal.lock().unwrap();
    wal.rotate().unwrap();
    let (pin_lsn, pin_ts) = wal.checkpoint_pin();
    assert_eq!(pin_lsn, 1);
    assert!(try_begin_checkpoint(&shared));
    // Hold the WAL lock so the real worker can publish its manifest but
    // cannot complete recycle. Publication must drain that in-flight work.
    schedule_checkpoint(&shared, pin_lsn, pin_ts);
    assert_eq!(
        wait_checkpoint_lsn(dir.path(), Duration::from_secs(5)),
        Some(1)
    );
    assert!(shared.checkpoint_busy.lock().unwrap().busy);
    let finished = Arc::new(AtomicBool::new(false));
    let worker = {
        let db = Arc::clone(&db);
        let finished = Arc::clone(&finished);
        std::thread::spawn(move || {
            db.apply_replicated(&crate::WalRecord {
                lsn: 0,
                commit_ts: 2,
                ops: vec![cypherlite_executor::RedoOp::CreateNode {
                    id: 1,
                    labels: Arc::new(vec![]),
                    properties: std::sync::Arc::new(cypherlite_core::PropertyMap::new()),
                }],
            })
            .unwrap();
            finished.store(true, Ordering::Release);
        })
    };
    let start = Instant::now();
    loop {
        if shared.checkpoint_busy.lock().unwrap().blocked {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::yield_now();
    }
    assert_eq!(wal.lsn, 1);
    assert!(!finished.load(Ordering::Acquire));
    drop(wal); // real worker now recycles and calls end_checkpoint
    worker.join().unwrap();
    assert!(finished.load(Ordering::Acquire));
    assert_eq!(shared.wal.lock().unwrap().lsn, 2);
    assert_eq!(manifest_checkpoint_lsn(dir.path()), Some(1));
}

#[test]
fn drop_joins_rotation_checkpoint_before_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    for _ in 0..8 {
        let db = Database::open_with(
            path,
            OpenOptions {
                checkpoint_interval: Some(std::time::Duration::ZERO),
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let shared = db.shared();
        // One commit past the rotation threshold schedules the detached
        // checkpoint that holds the writer lease until it exits.
        shared.wal.lock().unwrap().bytes = 64 * 1024 * 1024;
        db.connect().unwrap().query("CREATE ()").unwrap();
        assert_eq!(shared.background.lock().unwrap().len(), 1);
        drop(shared);
        db.leak_without_checkpoint();
    }
}

#[test]
fn standalone_failed_checkpoint_does_not_fence_later_writes() {
    let db = Database::open(":memory:").unwrap();
    let shared = db.shared();
    // A checkpoint that failed while no writer was blocked on its
    // admission drain must not leave a sticky fence (ADR-0094).
    end_checkpoint(&shared, &Err(Error::io("checkpoint failed")));
    assert!(shared.checkpoint_busy.lock().unwrap().last_error.is_none());
    // Later writes still succeed.
    db.connect()
        .unwrap()
        .query("CREATE (:AfterFailedCheckpoint)")
        .unwrap();
}

#[test]
fn failed_admitted_checkpoint_prevents_append() {
    let db = Database::open(":memory:").unwrap();
    let shared = db.shared();
    shared.checkpoint_busy.lock().unwrap().last_error = Some(Error::io("checkpoint failed"));
    let err = db
        .apply_replicated(&crate::WalRecord {
            lsn: 0,
            commit_ts: 1,
            ops: vec![],
        })
        .unwrap_err();
    assert!(err.to_string().contains("checkpoint failed"));
    assert_eq!(shared.wal.lock().unwrap().lsn, 0);
}

#[test]
fn checkpoint_gc_keeps_latest_and_drops_old_prop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:A {n: 1})").unwrap();
    c.query("MATCH (n:A) SET n.n = 2").unwrap();
    db.checkpoint().unwrap();
    let old = c.query("MATCH (n:A {n: 1}) RETURN n").unwrap();
    assert!(old.rows.is_empty());
    let new = c.query("MATCH (n:A {n: 2}) RETURN n.n AS n").unwrap();
    assert_eq!(new.rows, vec![vec![Value::Int(2)]]);
}
#[test]
fn s3_uri_is_rejected() {
    let err = Database::open("s3://bucket/prefix")
        .err()
        .expect("s3 rejected");
    assert!(err.to_string().contains("s3://"), "{err}");
    assert!(err.to_string().contains("memorydb-cloud"), "{err}");
}

#[test]
fn memory_uri_uses_wal_then_snapshot() {
    let db = Database::open(":memory:").unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:Person {name: 'Ada'})").unwrap();
    c.query("MATCH (n:Person) SET n.name = 'Bob'").unwrap();
    db.checkpoint().unwrap();
    let r = c.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("Bob"));
}

#[test]
fn rematerialize_from_wal_matches_live() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE (a:Person {name: 'Ada'})-[:KNOWS]->(b:Person {name: 'Bob'})")
            .unwrap();
        c.query("MATCH (n:Person {name: 'Ada'}) SET n.age = 36")
            .unwrap();
        db.leak_without_checkpoint();
    }
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    let r = c
        .query("MATCH (n:Person {name: 'Ada'}) RETURN n.age AS age, n.name AS name")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Int(36));
    assert_eq!(r.rows[0][1].as_string(), Some("Ada"));
    let r = c
        .query("MATCH (a:Person)-[:KNOWS]->(b) RETURN b.name AS n")
        .unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("Bob"));
}
#[test]
fn named_index_checkpoint_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE INDEX person_name FOR (n:Person) ON (n.name)")
            .unwrap();
        db.checkpoint().unwrap();
    }
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    let shown = c.query("SHOW INDEXES YIELD name RETURN name").unwrap();
    assert_eq!(shown.rows[0][0].as_string(), Some("person_name"));
}

#[test]
fn named_index_wal_without_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE INDEX person_name FOR (n:Person) ON (n.name)")
            .unwrap();
        db.leak_without_checkpoint();
    }
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    let shown = c.query("SHOW INDEXES YIELD name RETURN name").unwrap();
    assert_eq!(shown.rows[0][0].as_string(), Some("person_name"));
}
fn manifest_checkpoint_lsn(dir: &std::path::Path) -> Option<u64> {
    let bytes = std::fs::read(dir.join("manifest.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    manifest["checkpoint_lsn"].as_u64()
}

fn wait_checkpoint_lsn(dir: &std::path::Path, timeout: Duration) -> Option<u64> {
    let start = Instant::now();
    loop {
        if let Some(lsn) = manifest_checkpoint_lsn(dir) {
            if lsn > 0 {
                return Some(lsn);
            }
        }
        if start.elapsed() >= timeout {
            return manifest_checkpoint_lsn(dir);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn timed_checkpoint_dumps_dirty_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let db = Database::open_with(
        path,
        OpenOptions {
            checkpoint_interval: Some(Duration::from_millis(200)),
            ..Default::default()
        },
    )
    .unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:Person {name: 'Timer'})").unwrap();
    let lsn = wait_checkpoint_lsn(dir.path(), Duration::from_secs(3));
    assert!(lsn.unwrap_or(0) > 0, "timer should write manifest");
    drop(c);
    db.leak_without_checkpoint();
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("Timer"));
}

#[test]
fn timed_checkpoint_zero_disables_timer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let db = Database::open_with(
        path,
        OpenOptions {
            checkpoint_interval: Some(Duration::ZERO),
            ..Default::default()
        },
    )
    .unwrap();
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:Person {name: 'NoTimer'})").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        manifest_checkpoint_lsn(dir.path()).is_none(),
        "interval 0 must not dump"
    );
    db.checkpoint().unwrap();
    assert!(manifest_checkpoint_lsn(dir.path()).unwrap_or(0) > 0);
}

#[test]
fn timed_checkpoint_idle_does_not_dump() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let _db = Database::open_with(
        path,
        OpenOptions {
            checkpoint_interval: Some(Duration::from_millis(200)),
            ..Default::default()
        },
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        manifest_checkpoint_lsn(dir.path()).is_none(),
        "idle WAL must not write Parquet"
    );
}
#[test]
fn json_snapshot_codec_roundtrip_via_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open_with(
            path,
            OpenOptions {
                checkpoint_interval: Some(Duration::ZERO),
                snapshot_codec: Some(SnapshotCodecKind::Json),
                ..Default::default()
            },
        )
        .unwrap();
        let mut c = db.connect().unwrap();
        c.query("CREATE (n:Person {name: 'Ada'})").unwrap();
        db.checkpoint().unwrap();
    }
    assert!(dir.path().join("graph.json").exists());
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
    assert_eq!(r.rows[0][0].as_string(), Some("Ada"));
}

#[test]
fn snapshot_data_bytes_after_checkpoint() {
    let db = Database::open(":memory:").unwrap();
    assert_eq!(db.snapshot_data_bytes().unwrap(), 0);
    let mut c = db.connect().unwrap();
    c.query("CREATE (n:Person {name: 'Ada'})").unwrap();
    db.checkpoint().unwrap();
    assert!(db.snapshot_data_bytes().unwrap() > 0);
}

fn memory_wal_segments(db: &Database) -> Vec<String> {
    let mut names = db.shared().store.vfs().list_dir("wal").unwrap();
    names.sort();
    names
}

fn assert_checkpoint_idle(shared: &Shared) {
    let state = shared.checkpoint_busy.lock().unwrap();
    assert!(!state.busy && state.last_error.is_none());
}

#[test]
fn memory_rotation_checkpoint_recycles_without_snapshot() {
    let db = Database::open(":memory:").unwrap();
    db.connect().unwrap().query("CREATE (:N)").unwrap();
    // Empty replicated commits reach the WAL's commit threshold cheaply.
    let rotated = (2..=50_000u64).any(|commit_ts| {
        db.apply_replicated(&crate::WalRecord {
            lsn: 0,
            commit_ts,
            ops: vec![],
        })
        .unwrap();
        memory_wal_segments(&db).iter().any(|n| n == "00000002.log")
    });
    assert!(rotated, "WAL never rotated");
    let shared = db.shared();
    assert_checkpoint_idle(&shared);
    assert!(!shared.wal.lock().unwrap().has_closed_segments());
    assert_eq!(memory_wal_segments(&db), vec!["00000002.log".to_string()]);
    assert_eq!(db.snapshot_data_bytes().unwrap(), 0);
}

#[test]
fn memory_automatic_run_checkpoint_recycles_without_snapshot() {
    let db = Database::open(":memory:").unwrap();
    db.connect().unwrap().query("CREATE (:N)").unwrap();
    let shared = db.shared();
    run_checkpoint(&shared, Trigger::Automatic).unwrap();
    assert_checkpoint_idle(&shared);
    assert!(!shared.wal.lock().unwrap().has_closed_segments());
    assert_eq!(memory_wal_segments(&db), vec!["00000002.log".to_string()]);
    assert_eq!(db.snapshot_data_bytes().unwrap(), 0);
}

#[test]
fn memory_rotation_checkpoint_keeps_cdc_records_after_slot() {
    let db = Database::open(":memory:").unwrap();
    let shared = db.shared();
    db.register_cdc_slot("primary").unwrap();
    let mut c = db.connect().unwrap();
    for _ in 0..3 {
        c.query("CREATE (:N)").unwrap();
    }
    let slot = db.ack_cdc_slot("primary", 1).unwrap().acked_lsn;
    // Close the open segment as the WAL threshold would; the next commit
    // schedules the rotation checkpoint.
    shared.wal.lock().unwrap().rotate().unwrap();
    c.query("CREATE (:N)").unwrap();
    assert_checkpoint_idle(&shared);
    assert_eq!(db.snapshot_data_bytes().unwrap(), 0);
    let lsns: Vec<u64> = db
        .cdc_records_after(slot, 100)
        .unwrap()
        .iter()
        .map(|r| r.lsn)
        .collect();
    assert_eq!(lsns, vec![2, 3, 4]);

    db.ack_cdc_slot("primary", 4).unwrap();
    shared.wal.lock().unwrap().rotate().unwrap();
    c.query("CREATE (:N)").unwrap();
    assert!(!shared.wal.lock().unwrap().has_closed_segments());
    assert_eq!(memory_wal_segments(&db), vec!["00000003.log".to_string()]);
}
#[test]
fn vector_index_metadata_survives_wal_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    {
        let db = Database::open(path).unwrap();
        let mut c = db.connect().unwrap();
        c.query(
            "CREATE VECTOR INDEX docs FOR (n:Doc|Article) ON n.embedding \
             WITH [n.year] OPTIONS {indexConfig: {`vector.dimensions`: 2}}",
        )
        .unwrap();
        db.leak_without_checkpoint();
    }
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    let result = c
        .query(
            "SHOW VECTOR INDEXES YIELD name, labelsOrTypes, properties \
             WHERE name = 'docs' RETURN name, labelsOrTypes, properties",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], Value::String("docs".into()));
    assert_eq!(
        result.rows[0][1],
        Value::List(vec![Value::String("Doc".into()), Value::String("Article".into())].into())
    );
}
