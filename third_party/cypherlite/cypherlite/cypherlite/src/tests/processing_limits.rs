//! Token admission through the public API.

use std::sync::Arc;
use std::time::Duration;

use cypherlite_core::tokens::CHARGE_QUANTUM_BYTES;

use crate::{Database, Error, OpenOptions, ReadPause, Value};

const Q: u64 = CHARGE_QUANTUM_BYTES;

/// Smallest pool that admits one quantum. A former 1-token pool had a zero
/// tail (`1 / 8 == 0`). One quantum of bytes leaves `capacity / 8` free, so
/// the charge does not fit until the ceiling is at least one quantum.
fn pool_for_one_quantum() -> u64 {
    let mut capacity = Q;
    while capacity - capacity / 8 < Q {
        capacity += 1;
    }
    capacity
}

fn open_tokens(capacity: u64) -> Database {
    Database::open_with(
        ":memory:",
        OpenOptions {
            checkpoint_interval: Some(Duration::ZERO),
            token_admission: true,
            concurrent_logical_workspace_limit: Some(capacity),
            ..OpenOptions::default()
        },
    )
    .unwrap()
}

fn resource(err: &Error) -> (&'static str, u64, bool) {
    match err {
        Error::ResourceLimit {
            name,
            limit,
            retryable,
            ..
        } => (*name, *limit, *retryable),
        other => panic!("expected a resource limit, got {other}"),
    }
}

#[test]
fn admission_off_runs_overlapping_queries_without_a_receipt() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    conn.query("UNWIND range(1, 32) AS i CREATE (:N {i: i})")
        .unwrap();
    let rows = conn
        .query("MATCH (n:N) RETURN n.i AS i ORDER BY i")
        .unwrap();
    assert_eq!(rows.rows.len(), 32);
    assert!(rows.receipt.is_none());
    assert!(rows.plan.is_none());
}

#[test]
fn token_admission_requires_positive_limits() {
    let err = match Database::open_with(
        ":memory:",
        OpenOptions {
            token_admission: true,
            ..OpenOptions::default()
        },
    ) {
        Err(err) => err,
        Ok(_) => panic!("open should fail without token limits"),
    };
    assert!(
        err.to_string()
            .contains("CYPHERLITE_CONCURRENT_LOGICAL_WORKSPACE_LIMIT"),
        "{err}"
    );
}

#[test]
fn concurrent_logical_workspace_limit_rejects_the_overlap_and_keeps_the_session() {
    let capacity = pool_for_one_quantum();
    let db = open_tokens(capacity);
    db.connect().unwrap().query("CREATE (:N {v: 1})").unwrap();
    let pause = Arc::new(ReadPause {
        entered: std::sync::Barrier::new(2),
        release: std::sync::Barrier::new(2),
    });
    *db.inner.read_pause.lock().unwrap() = Some(Arc::clone(&pause));

    let mut reader = db.connect().unwrap();
    let read = std::thread::spawn(move || reader.query("MATCH (n:N) RETURN n.v AS v"));
    pause.entered.wait();

    let mut other = db.connect().unwrap();
    let err = other.query("RETURN 1").unwrap_err();
    assert_eq!(
        resource(&err),
        ("concurrent-logical-workspace-limit", capacity, true),
        "{err}"
    );
    assert!(err.receipt().is_some());
    pause.release.wait();
    let finished = read.join().unwrap().unwrap();
    assert_eq!(finished.rows, vec![vec![Value::Int(1)]]);
    assert!(finished.receipt.is_some());
    drop(finished);
    let retried = other.query("RETURN 1").unwrap();
    assert_eq!(retried.rows, vec![vec![Value::Int(1)]]);
    assert!(retried.receipt.is_some());
}

#[test]
fn concurrent_logical_workspace_limit_refusal_leaves_an_explicit_transaction_usable() {
    let capacity = pool_for_one_quantum();
    let db = open_tokens(capacity);
    db.connect().unwrap().query("CREATE (:N {v: 1})").unwrap();
    let pause = Arc::new(ReadPause {
        entered: std::sync::Barrier::new(2),
        release: std::sync::Barrier::new(2),
    });
    *db.inner.read_pause.lock().unwrap() = Some(Arc::clone(&pause));

    let mut reader = db.connect().unwrap();
    let read = std::thread::spawn(move || reader.query("MATCH (n:N) RETURN n.v AS v"));
    pause.entered.wait();

    let mut txn = db.connect().unwrap();
    txn.begin().unwrap();
    let err = txn.query("RETURN 1").unwrap_err();
    assert_eq!(
        resource(&err),
        ("concurrent-logical-workspace-limit", capacity, true),
        "{err}"
    );
    let retry = txn.query("RETURN 1").unwrap_err();
    assert_eq!(
        resource(&retry),
        ("concurrent-logical-workspace-limit", capacity, true),
        "{retry}"
    );

    pause.release.wait();
    assert_eq!(
        read.join().unwrap().unwrap().rows,
        vec![vec![Value::Int(1)]]
    );
    assert_eq!(
        txn.query("RETURN 1").unwrap().rows,
        vec![vec![Value::Int(1)]]
    );
    txn.rollback().unwrap();
}

#[test]
fn explain_does_not_take_a_reservation() {
    let db = open_tokens(pool_for_one_quantum());
    let mut conn = db.connect().unwrap();
    let explained = conn.query("EXPLAIN RETURN 1 AS n").unwrap();
    assert!(explained
        .plan
        .as_ref()
        .and_then(|plan| plan.root.token_estimate)
        .is_none());
    assert!(explained.receipt.is_none());
    let ran = conn.query("RETURN 1 AS n").unwrap();
    assert_eq!(ran.rows, vec![vec![Value::Int(1)]]);
    assert!(ran.receipt.is_some());
}

#[test]
fn profile_reports_the_peak_beside_the_plan_metrics() {
    let db = open_tokens(10 * Q);
    let mut conn = db.connect().unwrap();
    let profiled = conn.query("PROFILE RETURN 1 AS n").unwrap();
    let receipt = profiled.receipt.expect("profile carries a receipt");
    assert_eq!(receipt.outcome, crate::TokenOutcome::Success);
    assert_eq!(receipt.token_schedule, "v1");
    let plan = profiled.plan.expect("profile carries a plan");
    let actual = plan
        .root
        .token_estimate
        .expect("profile carries token actuals");
    assert_eq!(actual.peak_logical_bytes, receipt.peak_logical_bytes);
    assert_eq!(actual.token_schedule, receipt.token_schedule);
    assert!(plan.root.metrics.is_some());
}

#[test]
fn materialized_result_rows_are_inside_the_token_limit() {
    let db = open_tokens(4 * Q);
    let mut conn = db.connect().unwrap();
    let err = conn
        .query("UNWIND range(1, 1000) AS i RETURN toString(i) AS i")
        .unwrap_err();
    assert_eq!(
        resource(&err),
        ("concurrent-logical-workspace-limit", 4 * Q, true),
        "{err}"
    );
}

#[test]
fn write_result_token_refusal_happens_before_commit() {
    let db = open_tokens(4 * Q);
    let mut conn = db.connect().unwrap();
    let err = conn
        .query("CREATE (:N) RETURN range(1, 4096) AS rows")
        .unwrap_err();
    assert_eq!(
        resource(&err),
        ("concurrent-logical-workspace-limit", 4 * Q, true),
        "{err}"
    );
    let count = conn.query("MATCH (n:N) RETURN count(n)").unwrap();
    assert_eq!(count.rows, vec![vec![Value::Int(0)]]);
}

#[test]
fn wal_commit_workspace_refusal_happens_before_commit() {
    // The write overlay itself fits in one quantum. Materializing its final
    // redo vector and WAL payload requires a second quantum, which must be
    // reserved before this write is admitted to group commit.
    let capacity = pool_for_one_quantum();
    let db = open_tokens(capacity);
    let mut conn = db.connect().unwrap();
    let payload = "x".repeat(2_000);
    let err = conn
        .query(&format!("CREATE (:N {{payload: '{payload}'}})"))
        .unwrap_err();
    assert_eq!(
        resource(&err),
        ("concurrent-logical-workspace-limit", capacity, true),
        "{err}"
    );
    let count = conn.query("MATCH (n:N) RETURN count(n)").unwrap();
    assert_eq!(count.rows, vec![vec![Value::Int(0)]]);
}

#[test]
fn cached_peak_is_reserved_and_a_commit_misses() {
    // The sort's real peak sits above the admit ceiling and inside the
    // pool, so the first run finishes by spending the tail and the retry
    // is refused at the door.
    let capacity = 220 * Q;
    let ceiling = capacity - capacity / 8;
    let db = open_tokens(capacity);
    let mut conn = db.connect().unwrap();
    let query = "UNWIND range(1, 4000) AS i RETURN i AS i ORDER BY i";
    let first = conn.query(query).unwrap();
    let peak = first.receipt.expect("receipt").peak_logical_bytes;
    assert!(peak > ceiling, "peak {peak} ceiling {ceiling}");
    drop(first);

    let second = conn.query(query).unwrap_err();
    assert_eq!(
        resource(&second),
        ("concurrent-logical-workspace-limit", capacity, false),
        "{second}"
    );
    assert!(second.receipt().expect("receipt").peak_logical_bytes >= peak);
    let third = conn.query(query).unwrap_err();
    assert!(
        third.receipt().expect("receipt").peak_logical_bytes >= peak,
        "{third}"
    );

    let mut params = std::collections::BTreeMap::new();
    params.insert("n".into(), Value::Int(1));
    let other = conn
        .query_with_params("UNWIND range(1, $n) AS i RETURN i", params)
        .unwrap();
    assert_eq!(other.rows, vec![vec![Value::Int(1)]]);

    conn.query("CREATE ()").unwrap();
    let after_commit = conn.query(query).unwrap();
    assert!(after_commit.receipt.expect("receipt").peak_logical_bytes >= peak);
}
