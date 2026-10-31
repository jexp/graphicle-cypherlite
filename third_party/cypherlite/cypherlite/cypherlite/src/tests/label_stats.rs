//! `+labels` / `-labels` through `Connection`, answered by the per-label
//! snapshot probe (ADR-0171): the values the census produced, including at
//! an explicit transaction's snapshot while other connections commit.

use crate::{Database, QueryResult};

fn labels(result: &QueryResult) -> (i64, i64) {
    (result.stats.labels_added, result.stats.labels_removed)
}

#[test]
fn create_after_committed_delete_adds_the_label() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let mut pin = db.connect().unwrap();
    conn.query("CREATE (:A)").unwrap();
    // An open transaction pins a snapshot that still sees the node, so GC
    // cannot prune its id from the label index even if a checkpoint runs.
    pin.begin().unwrap();
    assert_eq!(labels(&conn.query("MATCH (n:A) DELETE n").unwrap()), (0, 1));
    assert_eq!(labels(&conn.query("CREATE (:A)").unwrap()), (1, 0));
    pin.rollback().unwrap();
}

#[test]
fn explicit_txn_create_after_concurrent_commit_adds_the_label() {
    let db = Database::open(":memory:").unwrap();
    let mut txn = db.connect().unwrap();
    let mut other = db.connect().unwrap();
    txn.begin().unwrap();
    assert_eq!(labels(&txn.query("CREATE (:X)").unwrap()), (1, 0));
    assert_eq!(labels(&other.query("CREATE (:B)").unwrap()), (1, 0));
    // `B` is new at the transaction's snapshot, as the census reported.
    assert_eq!(labels(&txn.query("CREATE (:B)").unwrap()), (1, 0));
    txn.commit().unwrap();
    assert_eq!(labels(&other.query("CREATE (:B)").unwrap()), (0, 0));
}

#[test]
fn explicit_txn_keeps_a_label_removed_after_begin() {
    let db = Database::open(":memory:").unwrap();
    let mut txn = db.connect().unwrap();
    let mut other = db.connect().unwrap();
    other.query("CREATE (:C), (:D)").unwrap();
    txn.begin().unwrap();
    txn.query("RETURN 1").unwrap();
    assert_eq!(
        labels(&other.query("MATCH (n:C) DELETE n").unwrap()),
        (0, 1)
    );
    assert_eq!(
        labels(&other.query("MATCH (n:D) REMOVE n:D").unwrap()),
        (0, 1)
    );
    // The snapshot still carries `C` and `D`.
    assert_eq!(labels(&txn.query("CREATE (:C), (:D)").unwrap()), (0, 0));
    txn.rollback().unwrap();
    assert_eq!(labels(&other.query("CREATE (:C), (:D)").unwrap()), (2, 0));
}

#[test]
fn explicit_txn_counts_against_its_own_writes() {
    let db = Database::open(":memory:").unwrap();
    let mut txn = db.connect().unwrap();
    txn.query("CREATE (:E)").unwrap();
    txn.begin().unwrap();
    assert_eq!(labels(&txn.query("MATCH (n:E) DELETE n").unwrap()), (0, 1));
    assert_eq!(labels(&txn.query("CREATE (:E)").unwrap()), (0, 0));
    assert_eq!(labels(&txn.query("CREATE (:E)").unwrap()), (0, 0));
    txn.commit().unwrap();
}
