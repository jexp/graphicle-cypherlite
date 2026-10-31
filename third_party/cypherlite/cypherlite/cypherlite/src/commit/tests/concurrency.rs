//! Commit-path tests for reader/writer races and the durability modes.

use super::super::*;
use super::harness::spawn_person_create_storm;
use crate::{Database, DurabilityMode, OpenOptions, Value};
use std::time::Duration;

#[test]
fn concurrent_readers_see_committed() {
    use std::sync::Arc;
    let db = Database::open(":memory:").unwrap();
    {
        let mut c = db.connect().unwrap();
        c.query("CREATE (n:Person {name: 'Ada'})").unwrap();
    }
    let db = Arc::new(db);
    let mut hs = Vec::new();
    for _ in 0..4 {
        let db = Arc::clone(&db);
        hs.push(std::thread::spawn(move || {
            let mut c = db.connect().unwrap();
            let r = c.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
            assert_eq!(r.rows[0][0].as_string(), Some("Ada"));
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
}

#[test]
fn concurrent_disjoint_creates() {
    use std::sync::Arc;
    let db = Arc::new(Database::open(":memory:").unwrap());
    let n = 8;
    spawn_person_create_storm(&db, n);
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:Person) RETURN n.name AS name").unwrap();
    assert_eq!(r.rows.len(), n);
}

#[test]
fn concurrent_match_during_creates() {
    use std::sync::{Arc, Barrier};
    let db = Arc::new(Database::open(":memory:").unwrap());
    {
        let mut c = db.connect().unwrap();
        for i in 0..40 {
            c.query(&format!("CREATE (n:Seed {{n: {i}}})")).unwrap();
        }
    }
    let writers = 8;
    let readers = 4;
    let barrier = Arc::new(Barrier::new(writers + readers));
    let mut hs = Vec::new();
    for i in 0..writers {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        hs.push(std::thread::spawn(move || {
            barrier.wait();
            let mut c = db.connect().unwrap();
            c.query(&format!("CREATE (n:W {{n: {i}}})")).unwrap();
        }));
    }
    for _ in 0..readers {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        hs.push(std::thread::spawn(move || {
            barrier.wait();
            let mut c = db.connect().unwrap();
            for _ in 0..20 {
                let r = c.query("MATCH (n) RETURN count(n) AS n").unwrap();
                let Value::Int(n) = r.rows[0][0] else {
                    panic!("count");
                };
                assert!((40..=48).contains(&n), "n={n}");
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n) RETURN count(n) AS n").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(48));
}

#[test]
fn concurrent_conflicting_sets_last_committer_wins() {
    use std::sync::{Arc, Barrier};
    let db = Database::open(":memory:").unwrap();
    {
        let mut c = db.connect().unwrap();
        c.query("CREATE (n:Person {name: 'Ada', n: 0})").unwrap();
    }
    let db = Arc::new(db);
    let n = 8;
    let barrier = Arc::new(Barrier::new(n));
    let mut hs = Vec::new();
    for i in 0..n {
        let db = Arc::clone(&db);
        let barrier = Arc::clone(&barrier);
        hs.push(std::thread::spawn(move || {
            barrier.wait();
            let mut c = db.connect().unwrap();
            let q = format!("MATCH (n:Person {{name: 'Ada'}}) SET n.n = {i}");
            loop {
                match c.query(&q) {
                    Ok(_) => break,
                    Err(Error::Conflict) => continue,
                    Err(e) => panic!("{e}"),
                }
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:Person) RETURN n.n AS n").unwrap();
    assert_eq!(r.rows.len(), 1);
    let Value::Int(v) = r.rows[0][0] else {
        panic!("expected int");
    };
    assert!((0..n as i64).contains(&v), "n={v}");
}

#[test]
fn conflict_display_is_stable() {
    assert_eq!(Error::conflict().to_string(), "write-write conflict");
    assert!(matches!(Error::conflict(), Error::Conflict));
}

#[test]
fn file_wal_commit_timing_splits_sync_from_apply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let db = Database::open(path).unwrap();
    db.enable_commit_timing();
    let mut c = db.connect().unwrap();
    c.query("CREATE (a:Account {id: 'a'}), (b:Account {id: 'b'})")
        .unwrap();
    db.clear_commit_timing();
    for i in 0..40 {
        c.query(&format!(
            "MATCH (a:Account {{id: 'a'}}), (b:Account {{id: 'b'}}) \
             CREATE (a)-[:TRANSFER {{id: 't{i}', amount: 1.0}}]->(b)"
        ))
        .unwrap();
    }
    let buf = db.inner.timing.lock().unwrap();
    assert_eq!(buf.samples.len(), 40);
    assert_eq!(buf.sync_calls, 40, "serial commits do not share a fsync");
    assert!(
        buf.samples.iter().all(|s| s.sync_ns > 0),
        "file WAL should fsync each serial commit"
    );
    assert!(buf.samples.iter().all(|s| s.encode_write_ns > 0));
    assert!(buf.samples.iter().all(|s| s.apply_ns > 0));
    drop(buf);
    eprint!("{}", db.commit_timing_report());
}

/// Group commit is a property of `flush_group` itself, not of thread
/// scheduling. A hand-built batch of independent records must issue
/// exactly one `sync_all` while still applying every record.
#[test]
fn snapshot_mode_acknowledges_with_dirty_wal() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with(
        dir.path().to_str().unwrap(),
        OpenOptions {
            checkpoint_interval: Some(Duration::ZERO),
            durability_mode: DurabilityMode::Snapshot,
            ..Default::default()
        },
    )
    .unwrap();
    db.connect().unwrap().query("CREATE (:N)").unwrap();
    assert!(db.inner.wal.lock().unwrap().needs_sync());
    let count = db
        .connect()
        .unwrap()
        .query("MATCH (n:N) RETURN count(n) AS n")
        .unwrap();
    assert_eq!(count.rows[0][0], Value::Int(1));
}

#[test]
fn everysecond_worker_barriers_dirty_wal() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with(
        dir.path().to_str().unwrap(),
        OpenOptions {
            checkpoint_interval: Some(Duration::ZERO),
            durability_mode: DurabilityMode::EverySecond,
            ..Default::default()
        },
    )
    .unwrap();
    db.connect().unwrap().query("CREATE (:N)").unwrap();
    assert!(db.inner.wal.lock().unwrap().needs_sync());
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(3))
        .expect("a three-second deadline is representable");
    while db.inner.wal.lock().unwrap().needs_sync() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        !db.inner.wal.lock().unwrap().needs_sync(),
        "one-second worker did not barrier the WAL"
    );
}

#[test]
fn background_durability_error_is_sticky_for_later_writes() {
    let db = Database::open(":memory:").unwrap();
    *db.inner.durability_error.lock().unwrap() =
        Some(Error::storage("background WAL barrier failed"));
    let error = db.connect().unwrap().query("CREATE (:N)").unwrap_err();
    assert!(error.to_string().contains("background WAL barrier failed"));
}
