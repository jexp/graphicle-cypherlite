//! Commit-path tests for group flushing, preflight, and safe horizons.

use super::super::*;
use super::harness::*;
use crate::{Database, Value, WalRecord};
use cypherlite_executor::BatchCommit;
use std::collections::BTreeMap;

#[test]
fn group_flush_shares_one_fsync() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let db = Database::open(path).unwrap();
    db.enable_commit_timing();
    db.clear_commit_timing();
    let shared = db.shared();
    let n = 12usize;
    let waiters: Vec<Arc<CommitWaiter>> = (0..n).map(|_| CommitWaiter::new()).collect();
    let batch: Vec<_> = (0..n)
        .map(|i| slot(i as i64, i as u64 + 1, 0, &waiters[i]))
        .collect();
    flush_group(&shared, batch).unwrap();
    for waiter in &waiters {
        waiter.wait().unwrap();
    }
    let buf = shared.timing.lock().unwrap();
    assert_eq!(
        buf.sync_calls, 1,
        "one group must issue exactly one sync_all"
    );
    assert_eq!(buf.samples.len(), n, "one sync sample per record");
    drop(buf);
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:N) RETURN count(n) AS n").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(n as i64));
}

/// Disjoint creates from racing connections must all commit. The exact
/// number of fsyncs depends on how threads happen to batch, so this only
/// asserts deterministic outcomes: no thread panics and the exact final
/// node count.
#[test]
fn concurrent_disjoint_creates_all_commit() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let db = Arc::new(Database::open(path).unwrap());
    let n = 12usize;
    spawn_person_create_storm(&db, n);
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:Person) RETURN count(n) AS n").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(n as i64));
}

#[test]
fn group_committed_records_replay_after_crash() {
    use std::sync::{Arc, Barrier};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let n = 8usize;
    {
        let db = Arc::new(Database::open(path).unwrap());
        let barrier = Arc::new(Barrier::new(n));
        let mut hs = Vec::new();
        for i in 0..n {
            let db = Arc::clone(&db);
            let barrier = Arc::clone(&barrier);
            hs.push(std::thread::spawn(move || {
                barrier.wait();
                let mut c = db.connect().unwrap();
                c.query(&format!("CREATE (n:Person {{name: 'p{i}'}})"))
                    .unwrap();
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        match Arc::try_unwrap(db) {
            Ok(db) => db.leak_without_checkpoint(),
            Err(_) => panic!("connections still live"),
        }
    }
    let db = Database::open(path).unwrap();
    let mut c = db.connect().unwrap();
    let r = c.query("MATCH (n:Person) RETURN count(n) AS n").unwrap();
    assert_eq!(r.rows[0][0], Value::Int(n as i64));
}

/// An invalid later slot rejects all eligible waiters before any frame.
#[test]
fn applicability_preflight_completes_all_waiters_in_batch() {
    use std::collections::BTreeMap;

    let db = Database::open(":memory:").unwrap();
    let shared = db.shared();

    // A bad op: CreateRel references node ids that don't exist, so
    // ordered validation rejects it before WAL append.
    let bad = RedoOp::CreateRel {
        id: 1,
        start: 9_999,
        end: 9_998,
        rel_type: "KNOWS".into(),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    };
    let good = RedoOp::CreateNode {
        id: 1,
        labels: Arc::new(vec!["P".into()]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    };

    // Batch: good, bad, good — the middle one fails preflight.
    let waiters: Vec<Arc<CommitWaiter>> = (0..3).map(|_| CommitWaiter::new()).collect();
    let mut batch = Vec::new();
    for (i, op) in [good.clone(), bad, good].into_iter().enumerate() {
        batch.push(CommitSlot {
            snapshot_ts: 0,
            txn_id: (i + 1) as u64,
            ops: vec![op],
            parse_plan_ns: 0,
            execute_ns: 0,
            enqueued: Instant::now(),
            waiter: Arc::clone(&waiters[i]),
        });
    }

    let res = flush_group(&shared, batch);
    assert!(res.is_err(), "the bad CreateRel must fail preflight");
    assert_eq!(shared.wal.lock().unwrap().lsn, 0);
    assert!(shared.graph.all_node_ids().is_empty());

    // Every waiter's result slot must be filled (no hang on `wait()`).
    for (i, w) in waiters.iter().enumerate() {
        let filled = w.result.lock().unwrap().is_some();
        assert!(filled, "waiter {i} was not completed — would hang");
    }
}

#[test]
fn oversized_second_candidate_leaves_wal_and_graph_intact_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Database::open(path.to_str().unwrap()).unwrap();
    let shared = db.shared();
    let wal_file = path.join("wal/00000001.log");
    let before = std::fs::read(&wal_file).unwrap();
    let waiters: Vec<_> = (0..2).map(|_| CommitWaiter::new()).collect();
    let mut batch = Vec::new();
    for (i, properties) in [
        BTreeMap::new(),
        BTreeMap::from([(
            "big".into(),
            Value::String("x".repeat(64 * 1024 * 1024).into()),
        )]),
    ]
    .into_iter()
    .enumerate()
    {
        batch.push(CommitSlot {
            snapshot_ts: 0,
            txn_id: i as u64 + 1,
            ops: vec![RedoOp::CreateNode {
                id: i as i64,
                labels: Arc::new(vec!["N".into()]),
                properties: cypherlite_core::PropertyMap::share(properties.clone()),
            }],
            parse_plan_ns: 0,
            execute_ns: 0,
            enqueued: Instant::now(),
            waiter: Arc::clone(&waiters[i]),
        });
    }
    let error = flush_group(&shared, batch).unwrap_err();
    assert!(error.to_string().contains("64 MiB"), "{error}");
    for waiter in waiters {
        assert_eq!(waiter.wait().unwrap_err(), error);
    }
    assert_eq!(shared.wal.lock().unwrap().lsn, 0);
    assert_eq!(std::fs::read(&wal_file).unwrap(), before);
    assert!(shared.graph.all_node_ids().is_empty());
    drop(shared);
    db.leak_without_checkpoint();
    let reopened = Database::open(path.to_str().unwrap()).unwrap();
    assert!(reopened.inner.graph.all_node_ids().is_empty());
}

#[test]
fn two_valid_candidates_share_exactly_one_barrier_and_keep_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().to_str().unwrap()).unwrap();
    db.enable_commit_timing();
    let shared = db.shared();
    let waiters: Vec<_> = (0..2).map(|_| CommitWaiter::new()).collect();
    let batch: Vec<_> = (0..2)
        .map(|i| CommitSlot {
            snapshot_ts: 0,
            txn_id: i + 1,
            ops: vec![RedoOp::CreateNode {
                id: i as i64,
                labels: Arc::new(vec!["N".into()]),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            }],
            parse_plan_ns: 0,
            execute_ns: 0,
            enqueued: Instant::now(),
            waiter: Arc::clone(&waiters[i as usize]),
        })
        .collect();
    flush_group(&shared, batch).unwrap();
    // Each waiter learns the commit timestamp its own slot was assigned.
    for (i, waiter) in waiters.iter().enumerate() {
        assert_eq!(waiter.wait().unwrap(), i as u64 + 1);
    }
    assert_eq!(shared.timing.lock().unwrap().sync_calls, 1);
    assert_eq!(
        shared
            .wal
            .lock()
            .unwrap()
            .replay(0)
            .unwrap()
            .iter()
            .map(|r| r.commit_ts)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert_eq!(shared.graph.all_node_ids(), [0, 1]);
}

#[test]
fn group_and_replica_share_safe_horizon_allocators_and_checkpoint_pin() {
    for replica in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let db = untimed_file_db(path);
        let shared = db.shared();
        let ops = vec![
            node_op(0),
            node_op(1),
            RedoOp::CreateRel {
                id: 0,
                start: 0,
                end: 1,
                rel_type: "LINK".into(),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            },
        ];
        // Force this one commit to close its segment and schedule the
        // checkpoint, without writing thousands of unrelated records.
        shared.wal.lock().unwrap().bytes = 64 * 1024 * 1024;
        if replica {
            assert_eq!(
                db.apply_replicated(&WalRecord {
                    lsn: 99,
                    commit_ts: 7,
                    ops,
                })
                .unwrap(),
                1
            );
        } else {
            let waiter = CommitWaiter::new();
            let mut candidate = slot(0, 7, 0, &waiter);
            candidate.ops = ops;
            flush_group(&shared, vec![candidate]).unwrap();
            waiter.wait().unwrap();
        }
        assert_eq!(shared.safe_lsn.load(Ordering::Acquire), 1);
        assert_eq!(shared.safe_ts.load(Ordering::Acquire), 7);
        assert_eq!(shared.graph.commit_ts(), 7);
        assert!(shared.next_node_id.load(Ordering::Relaxed) >= 2);
        assert!(shared.next_rel_id.load(Ordering::Relaxed) >= 1);
        assert!(shared.next_txn_id.load(Ordering::Relaxed) >= 8);
        crate::checkpoint::wait_checkpoint_idle(&shared).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["checkpoint_lsn"], 1);
        assert_eq!(manifest["commit_ts"], 7);
        drop(shared);
        db.leak_without_checkpoint();
        let reopened = untimed_file_db(path);
        assert_eq!(
            reopened
                .connect()
                .unwrap()
                .query("MATCH (n:N)-[:LINK]->(m:N) RETURN n, m")
                .unwrap()
                .rows
                .len(),
            1
        );
    }
}

/// The per-batch commit sink (ADR-0210) must hand back the commit
/// timestamp its own slot was assigned — never a re-read of the live
/// commit clock, which a foreign commit can advance after the publication
/// guard is released (the scratch would then rebase past another
/// transaction's writes). Pinned here at the source: successive batch
/// commits return exactly the assigned, strictly increasing timestamps.
#[test]
fn batch_commit_returns_its_own_assigned_commit_ts() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open(dir.path().to_str().unwrap()).unwrap();
    let shared = db.shared();
    let sink = GroupBatchCommit {
        shared: Arc::clone(&shared),
    };
    let ts1 = sink.commit_batch(vec![node_op(0)], 0).unwrap();
    assert_eq!(ts1, shared.graph.commit_ts());
    // A foreign commit in between must not leak into the next batch's
    // returned timestamp base: the sink returns what ITS slot was
    // assigned, whatever the clock reads afterwards.
    let ts2 = sink.commit_batch(vec![node_op(1)], ts1).unwrap();
    assert_eq!(ts2, ts1 + 1);
    assert_eq!(ts2, shared.graph.commit_ts());
    assert_eq!(shared.graph.all_node_ids(), [0, 1]);
}
