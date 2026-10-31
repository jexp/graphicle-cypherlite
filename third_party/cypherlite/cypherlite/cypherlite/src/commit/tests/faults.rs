//! Commit-path tests for injected I/O faults and uncertain-outcome fencing.

use super::super::*;
use super::harness::*;
use crate::{checkpoint::try_begin_checkpoint, Database, ReadPause, Value, WalRecord};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn uncertain_sync_apply_or_rotate_freezes_both_paths_before_checkpoint() {
    for replica in [false, true] {
        for phase in [Phase::Sync, Phase::Apply, Phase::Rotate] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().to_str().unwrap();
            let db = untimed_file_db(path);
            db.connect().unwrap().query("CREATE (:Prior)").unwrap();
            let shared = db.shared();
            let fault = install_fault_wal(&shared);
            let safe = (
                shared.safe_lsn.load(Ordering::Acquire),
                shared.safe_ts.load(Ordering::Acquire),
            );
            match phase {
                Phase::Sync => fault.failed_barrier.store(true, Ordering::SeqCst),
                Phase::Apply => shared.fail_after_sync.store(true, Ordering::SeqCst),
                Phase::Rotate => {
                    fault.failed_rotate.store(true, Ordering::SeqCst);
                    shared.wal.lock().unwrap().bytes = 64 * 1024 * 1024;
                }
            }
            let error = if replica {
                db.apply_replicated(&WalRecord {
                    lsn: 99,
                    commit_ts: 2,
                    ops: vec![node_op(1)],
                })
                .unwrap_err()
            } else {
                let waiters: Vec<_> = (0..2).map(|_| CommitWaiter::new()).collect();
                let error = flush_group(
                    &shared,
                    vec![slot(1, 2, 1, &waiters[0]), slot(2, 3, 1, &waiters[1])],
                )
                .unwrap_err();
                for waiter in waiters {
                    assert_eq!(waiter.wait().unwrap_err(), error, "{phase:?}");
                }
                error
            };
            assert!(
                error.to_string().contains("uncertain WAL outcome"),
                "{error}"
            );
            assert_eq!(
                (
                    shared.safe_lsn.load(Ordering::Acquire),
                    shared.safe_ts.load(Ordering::Acquire)
                ),
                safe,
                "replica={replica}, phase={phase:?}"
            );
            assert!(shared.poisoned.load(Ordering::Acquire));
            assert!(!dir.path().join("manifest.json").exists());
            assert!(!try_begin_checkpoint(&shared));
            drop(shared);
            drop(db);
            drop(fault);
            assert!(!dir.path().join("manifest.json").exists());
            let reopened = untimed_file_db(path);
            assert_eq!(
                reopened.inner.graph.all_node_ids().len(),
                if replica { 2 } else { 3 },
                "replica={replica}, phase={phase:?}"
            );
        }
    }
}

#[test]
fn admitted_read_finishes_while_replica_rotation_poison_is_published() {
    use std::sync::{mpsc, Barrier};

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(untimed_file_db(dir.path().to_str().unwrap()));
    db.connect().unwrap().query("CREATE (:Prior)").unwrap();
    let shared = db.shared();
    let fault = install_fault_wal(&shared);
    fault.failed_rotate.store(true, Ordering::SeqCst);
    shared.wal.lock().unwrap().bytes = 64 * 1024 * 1024;
    let pause = Arc::new(ReadPause {
        entered: Barrier::new(2),
        release: Barrier::new(2),
    });
    *shared.read_pause.lock().unwrap() = Some(Arc::clone(&pause));
    let mut reader = db.connect().unwrap();
    let read = std::thread::spawn(move || reader.query("MATCH (n) RETURN count(n) AS n"));
    pause.entered.wait();

    let (tx, rx) = mpsc::channel();
    let writer = {
        let db = Arc::clone(&db);
        std::thread::spawn(move || {
            tx.send(db.apply_replicated(&WalRecord {
                lsn: 0,
                commit_ts: 2,
                ops: vec![node_op(1)],
            }))
            .unwrap();
        })
    };
    let published = rx.recv_timeout(Duration::from_secs(10));
    pause.release.wait(); // never strand the admitted reader if the writer stalls
    assert!(published
        .expect("rotation must not wait for admitted read")
        .is_err());
    writer.join().unwrap();
    assert_eq!(
        read.join().unwrap().unwrap().rows,
        vec![vec![Value::Int(1)]]
    );
    assert_eq!(shared.safe_lsn.load(Ordering::Acquire), 1);
    assert_eq!(shared.safe_ts.load(Ordering::Acquire), 1);
    assert!(db.connect().is_err());
}

#[test]
fn physical_uncertainty_fences_waiters_reads_cdc_checkpoint_and_drop() {
    for after_sync in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let db = Database::open(path.to_str().unwrap()).unwrap();
        db.connect().unwrap().query("CREATE (:Prior)").unwrap();
        let shared = db.shared();
        let wal_path = path.join("wal/00000001.log");
        let prior_len = std::fs::metadata(&wal_path).unwrap().len();
        let before = (
            shared.safe_lsn.load(Ordering::Acquire),
            shared.safe_ts.load(Ordering::Acquire),
        );
        if after_sync {
            shared.fail_after_sync.store(true, Ordering::SeqCst);
        } else {
            shared.fail_after_append.store(true, Ordering::SeqCst);
        }
        let waiters: Vec<_> = (0..2).map(|_| CommitWaiter::new()).collect();
        let batch: Vec<_> = (0..2)
            .map(|i| CommitSlot {
                snapshot_ts: 1,
                txn_id: i + 2,
                ops: vec![RedoOp::CreateNode {
                    id: i as i64 + 1,
                    labels: Arc::new(vec!["Uncertain".into()]),
                    properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
                }],
                parse_plan_ns: 0,
                execute_ns: 0,
                enqueued: Instant::now(),
                waiter: Arc::clone(&waiters[i as usize]),
            })
            .collect();
        let error = flush_group(&shared, batch).unwrap_err();
        assert!(
            error.to_string().contains("uncertain WAL outcome"),
            "{error}"
        );
        for waiter in waiters {
            assert_eq!(waiter.wait().unwrap_err(), error);
        }
        assert_eq!(
            (
                shared.safe_lsn.load(Ordering::Acquire),
                shared.safe_ts.load(Ordering::Acquire)
            ),
            before
        );
        assert!(shared.poisoned.load(Ordering::Acquire));
        assert!(db.connect().is_err());
        assert!(db.cdc_records_after(0, 1).is_err());
        assert!(db.register_cdc_slot("s").is_err());
        assert!(db.checkpoint().is_err());
        assert!(db
            .apply_replicated(&crate::WalRecord {
                lsn: 0,
                commit_ts: 4,
                ops: vec![]
            })
            .is_err());
        let mut c = raw_connection(&shared);
        assert!(c.query("MATCH (n) RETURN count(n)").is_err());
        assert!(c.query("CREATE (:Retry)").is_err());
        assert!(c.pull_lookup("s", crate::PullKind::Node, "r").is_err());
        drop(c);
        if !after_sync {
            // Model a torn first frame after the injected physical append
            // error; recovery must retain only the safe preceding prefix.
            std::fs::OpenOptions::new()
                .write(true)
                .open(&wal_path)
                .unwrap()
                .set_len(prior_len + 2)
                .unwrap();
        }
        drop(shared);
        drop(db); // poisoned handles must not persist a partial graph
        assert!(!path.join("manifest.json").exists());
        let reopened = Database::open(path.to_str().unwrap()).unwrap();
        assert_eq!(
            reopened.inner.graph.all_node_ids().len(),
            if after_sync { 3 } else { 1 }
        );
    }
}

#[test]
fn actual_vfs_write_and_barrier_faults_fence_shared_publication() {
    for short_write in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let db = Database::open(path).unwrap();
        db.connect().unwrap().query("CREATE (:Prior)").unwrap();
        let shared = db.shared();
        let fault = install_fault_wal(&shared);
        let safe = (
            shared.safe_lsn.load(Ordering::Acquire),
            shared.safe_ts.load(Ordering::Acquire),
        );
        let manifest = dir.path().join("manifest.json");
        let manifest_before = std::fs::read(&manifest).ok();
        if short_write {
            fault.short_write.store(true, Ordering::SeqCst);
        } else {
            fault.failed_barrier.store(true, Ordering::SeqCst);
        }
        let waiters: Vec<_> = (0..2).map(|_| CommitWaiter::new()).collect();
        let batch: Vec<_> = (0..2)
            .map(|i| CommitSlot {
                snapshot_ts: 1,
                txn_id: i + 2,
                ops: vec![RedoOp::CreateNode {
                    id: i as i64 + 1,
                    labels: Arc::new(vec!["Uncertain".into()]),
                    properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
                }],
                parse_plan_ns: 0,
                execute_ns: 0,
                enqueued: Instant::now(),
                waiter: Arc::clone(&waiters[i as usize]),
            })
            .collect();
        let error = flush_group(&shared, batch).unwrap_err();
        assert!(
            error.to_string().contains("uncertain WAL outcome"),
            "{error}"
        );
        for waiter in waiters {
            assert_eq!(waiter.wait().unwrap_err(), error);
        }
        assert_eq!(
            (
                shared.safe_lsn.load(Ordering::Acquire),
                shared.safe_ts.load(Ordering::Acquire)
            ),
            safe
        );
        assert!(shared.poisoned.load(Ordering::Acquire));
        let mut c = raw_connection(&shared);
        assert!(c.query("MATCH (n) RETURN count(n)").is_err());
        assert!(c.query("CREATE (:Retry)").is_err());
        assert!(db.cdc_records_after(0, 10).is_err());
        assert!(db.checkpoint().is_err());
        crate::checkpoint::timed_checkpoint(&shared);
        assert!(!try_begin_checkpoint(&shared));
        assert!(shared.wal.lock().unwrap().rotate().is_err());
        assert!(shared.wal.lock().unwrap().recycle(1).is_err());
        assert_eq!(std::fs::read(&manifest).ok(), manifest_before);
        drop(c);
        drop(shared);
        drop(db);
        drop(fault);
        assert_eq!(std::fs::read(&manifest).ok(), manifest_before);
        let reopened = Database::open(path).unwrap();
        let count = reopened
            .connect()
            .unwrap()
            .query("MATCH (n) RETURN count(n) AS n")
            .unwrap()
            .rows[0][0]
            .clone();
        assert_eq!(count, Value::Int(if short_write { 1 } else { 3 }));
    }
}
