use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Barrier};
use std::time::Duration;

use crate::{Database, ReadPause, Value};

fn pause_next_read(db: &Database) -> Arc<ReadPause> {
    let pause = Arc::new(ReadPause {
        entered: Barrier::new(2),
        release: Barrier::new(2),
    });
    *db.inner.read_pause.lock().unwrap() = Some(Arc::clone(&pause));
    pause
}

#[test]
fn admitted_read_does_not_block_wal_barrier_and_keeps_its_snapshot() {
    let db = Database::open(":memory:").unwrap();
    db.connect().unwrap().query("CREATE (:N {v: 1})").unwrap();
    let lsn_before = db.inner.safe_lsn.load(Ordering::Acquire);
    let pause = pause_next_read(&db);

    let mut reader = db.connect().unwrap();
    let read = std::thread::spawn(move || reader.query("MATCH (n:N) RETURN n.v AS v").unwrap());
    pause.entered.wait(); // the reader has checked health and pinned its view

    let mut writer = db.connect().unwrap();
    let (tx, rx) = mpsc::channel();
    let commit = std::thread::spawn(move || {
        tx.send(writer.query("MATCH (n:N) SET n.v = 2")).unwrap();
    });
    let written = rx.recv_timeout(Duration::from_secs(30));
    if written.is_ok() {
        // Force version reclamation while the admitted read still owns its pin.
        db.inner.graph.gc_versions();
    }
    // Release even when the assertion below fails, so a broken lock scope
    // cannot strand the query worker while the test is unwinding.
    pause.release.wait();
    written
        .expect("WAL append/sync/apply must finish before releasing the reader")
        .unwrap();
    commit.join().unwrap();
    assert_eq!(db.inner.safe_lsn.load(Ordering::Acquire), lsn_before + 1);
    assert_eq!(read.join().unwrap().rows, vec![vec![Value::Int(1)]]);
    assert_eq!(
        db.connect()
            .unwrap()
            .query("MATCH (n:N) RETURN n.v AS v")
            .unwrap()
            .rows,
        vec![vec![Value::Int(2)]]
    );
}

#[test]
fn admitted_show_uses_schema_before_concurrent_commit() {
    for (show, old_ddl, new_ddl) in [
        (
            "SHOW INDEXES YIELD name RETURN name ORDER BY name",
            "CREATE INDEX old_schema FOR (n:N) ON (n.v)",
            "CREATE INDEX new_schema FOR (n:N) ON (n.w)",
        ),
        (
            "SHOW CONSTRAINTS YIELD name RETURN name ORDER BY name",
            "CREATE CONSTRAINT old_schema FOR (n:N) REQUIRE n.v IS UNIQUE",
            "CREATE CONSTRAINT new_schema FOR (n:N) REQUIRE n.w IS UNIQUE",
        ),
    ] {
        let db = Database::open(":memory:").unwrap();
        db.connect().unwrap().query(old_ddl).unwrap();
        let lsn_before = db.inner.safe_lsn.load(Ordering::Acquire);
        let pause = pause_next_read(&db);
        let mut reader = db.connect().unwrap();
        let show = show.to_string();
        let read = std::thread::spawn(move || reader.query(&show).unwrap());
        pause.entered.wait();

        let mut writer = db.connect().unwrap();
        let (tx, rx) = mpsc::channel();
        let commit = std::thread::spawn(move || tx.send(writer.query(new_ddl)).unwrap());
        let written = rx.recv_timeout(Duration::from_secs(30));
        if written.is_ok() {
            db.inner.graph.gc_versions();
        }
        pause.release.wait();
        written
            .expect("schema WAL commit must finish while SHOW is paused")
            .unwrap();
        commit.join().unwrap();
        assert_eq!(db.inner.safe_lsn.load(Ordering::Acquire), lsn_before + 1);
        assert_eq!(
            read.join().unwrap().rows,
            vec![vec![Value::String("old_schema".into())]]
        );
        let later = db
            .connect()
            .unwrap()
            .query(if new_ddl.contains("INDEX ") {
                "SHOW INDEXES YIELD name RETURN name ORDER BY name"
            } else {
                "SHOW CONSTRAINTS YIELD name RETURN name ORDER BY name"
            })
            .unwrap();
        assert_eq!(
            later.rows,
            vec![
                vec![Value::String("new_schema".into())],
                vec![Value::String("old_schema".into())]
            ]
        );
    }
}

#[test]
fn admitted_show_keeps_dropped_schema_and_index_metadata() {
    for (ddl, show, drop_ddl, expected) in [
        (
            "CREATE TEXT INDEX old_schema FOR (n:N) ON (n.v)",
            "SHOW INDEXES YIELD name, type RETURN name, type",
            "DROP INDEX old_schema",
            vec![
                Value::String("old_schema".into()),
                Value::String("TEXT".into()),
            ],
        ),
        (
            "CREATE CONSTRAINT old_schema FOR (n:N) REQUIRE n.v IS UNIQUE",
            "SHOW CONSTRAINTS YIELD name, type RETURN name, type",
            "DROP CONSTRAINT old_schema",
            vec![
                Value::String("old_schema".into()),
                Value::String("NODE_PROPERTY_UNIQUENESS".into()),
            ],
        ),
    ] {
        let db = Database::open(":memory:").unwrap();
        db.connect().unwrap().query(ddl).unwrap();
        let pause = pause_next_read(&db);
        let mut reader = db.connect().unwrap();
        let read = std::thread::spawn(move || reader.query(show).unwrap());
        pause.entered.wait();
        let mut writer = db.connect().unwrap();
        let (tx, rx) = mpsc::channel();
        let commit = std::thread::spawn(move || tx.send(writer.query(drop_ddl)).unwrap());
        let written = rx.recv_timeout(Duration::from_secs(30));
        pause.release.wait();
        written
            .expect("schema drop must finish before SHOW resumes")
            .unwrap();
        commit.join().unwrap();
        assert_eq!(read.join().unwrap().rows, vec![expected]);
        assert!(db.connect().unwrap().query(show).unwrap().rows.is_empty());
    }
}

#[test]
fn admitted_fulltext_search_keeps_index_dropped_during_execution() {
    let db = Database::open(":memory:").unwrap();
    let mut setup = db.connect().unwrap();
    setup
        .query("CREATE FULLTEXT INDEX ft FOR (p:Person) ON EACH [p.name]")
        .unwrap();
    setup.query("CREATE (:Person {name: 'old match'})").unwrap();

    let pause = pause_next_read(&db);
    let mut reader = db.connect().unwrap();
    let read = std::thread::spawn(move || {
        reader.query(
            "MATCH (p) SEARCH p IN (FULLTEXT INDEX ft FOR 'match' LIMIT 10) RETURN p.name AS name",
        )
    });
    pause.entered.wait();

    let mut writer = db.connect().unwrap();
    let (tx, rx) = mpsc::channel();
    let commit = std::thread::spawn(move || tx.send(writer.query("DROP INDEX ft")).unwrap());
    let dropped = rx.recv_timeout(Duration::from_secs(30));
    pause.release.wait();
    dropped
        .expect("index drop must finish before the admitted search resumes")
        .unwrap();
    commit.join().unwrap();
    assert_eq!(
        read.join().unwrap().unwrap().rows,
        vec![vec![Value::String("old match".into())]]
    );
    assert!(db
        .connect()
        .unwrap()
        .query("MATCH (p) SEARCH p IN (FULLTEXT INDEX ft FOR 'match' LIMIT 10) RETURN p.name")
        .is_err());
}

#[test]
fn uncommitted_index_metadata_is_private_and_rollback_preserves_committed_kind() {
    let db = Database::open(":memory:").unwrap();
    let mut txn = db.connect().unwrap();
    let mut other = db.connect().unwrap();
    let show = "SHOW INDEXES YIELD name, type RETURN name, type ORDER BY name";

    txn.begin().unwrap();
    txn.query("CREATE TEXT INDEX private_text FOR (n:N) ON (n.v)")
        .unwrap();
    assert_eq!(
        txn.query(show).unwrap().rows,
        vec![vec![
            Value::String("private_text".into()),
            Value::String("TEXT".into())
        ]]
    );
    assert!(other.query(show).unwrap().rows.is_empty());
    txn.rollback().unwrap();
    assert!(other.query(show).unwrap().rows.is_empty());

    other
        .query("CREATE TEXT INDEX committed_text FOR (n:N) ON (n.v)")
        .unwrap();
    txn.begin().unwrap();
    txn.query("DROP INDEX committed_text").unwrap();
    assert!(txn.query(show).unwrap().rows.is_empty());
    assert_eq!(
        other.query(show).unwrap().rows,
        vec![vec![
            Value::String("committed_text".into()),
            Value::String("TEXT".into())
        ]]
    );
    txn.rollback().unwrap();
    assert_eq!(
        other.query(show).unwrap().rows,
        vec![vec![
            Value::String("committed_text".into()),
            Value::String("TEXT".into())
        ]]
    );

    txn.begin().unwrap();
    txn.query("DROP INDEX committed_text").unwrap();
    txn.query("CREATE VECTOR INDEX replacement FOR (n:N) ON (n.v)")
        .unwrap();
    assert_eq!(
        txn.query(show).unwrap().rows,
        vec![vec![
            Value::String("replacement".into()),
            Value::String("VECTOR".into())
        ]]
    );
    txn.commit().unwrap();
    assert_eq!(
        other.query(show).unwrap().rows,
        vec![vec![
            Value::String("replacement".into()),
            Value::String("VECTOR".into())
        ]]
    );
}

#[test]
fn admitted_theater_shows_are_immutable_across_commit() {
    for (setup, show, change) in [
        (
            "ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5",
            "SHOW DATABASE neo4j YIELD defaultLanguage RETURN defaultLanguage",
            "ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 25",
        ),
        (
            "ALTER CURRENT GRAPH TYPE SET { (:Old => {v :: STRING}) }",
            "SHOW CURRENT GRAPH TYPE YIELD specification RETURN specification",
            "ALTER CURRENT GRAPH TYPE SET { (:New => {w :: INTEGER}) }",
        ),
    ] {
        let db = Database::open(":memory:").unwrap();
        db.connect().unwrap().query(setup).unwrap();
        let expected = db.connect().unwrap().query(show).unwrap().rows;
        let pause = pause_next_read(&db);
        let mut reader = db.connect().unwrap();
        let read = std::thread::spawn(move || reader.query(show).unwrap());
        pause.entered.wait();
        let mut writer = db.connect().unwrap();
        let (tx, rx) = mpsc::channel();
        let commit = std::thread::spawn(move || tx.send(writer.query(change)).unwrap());
        let written = rx.recv_timeout(Duration::from_secs(30));
        pause.release.wait();
        written
            .expect("SHOW must not hold publication during execution")
            .unwrap();
        commit.join().unwrap();
        assert_eq!(read.join().unwrap().rows, expected);
        assert_ne!(db.connect().unwrap().query(show).unwrap().rows, expected);
    }
}

#[test]
fn graph_type_show_and_transaction_theater_overlay() {
    let db = Database::open(":memory:").unwrap();
    let mut txn = db.connect().unwrap();
    let mut other = db.connect().unwrap();
    txn.begin().unwrap();
    txn.query("CREATE GRAPH TYPE social AS { (:Person => {name :: STRING}) }")
        .unwrap();
    txn.query("ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5")
        .unwrap();
    assert_eq!(
        txn.query("SHOW GRAPH TYPE social YIELD name RETURN name")
            .unwrap()
            .rows,
        vec![vec![Value::String("social".into())]]
    );
    assert_eq!(
        txn.query("SHOW DATABASE neo4j YIELD defaultLanguage RETURN defaultLanguage")
            .unwrap()
            .rows,
        vec![vec![Value::String("CYPHER 5".into())]]
    );
    assert!(other.query("SHOW GRAPH TYPE social").is_err());
    assert_eq!(
        other
            .query("SHOW DATABASE neo4j YIELD defaultLanguage RETURN defaultLanguage")
            .unwrap()
            .rows,
        vec![vec![Value::String("CYPHER 25".into())]]
    );
    txn.rollback().unwrap();
    assert!(other.query("SHOW GRAPH TYPE social").is_err());
    assert_eq!(
        other
            .query("SHOW DATABASE neo4j YIELD defaultLanguage RETURN defaultLanguage")
            .unwrap()
            .rows,
        vec![vec![Value::String("CYPHER 25".into())]]
    );

    let show = "SHOW CURRENT GRAPH TYPE YIELD specification RETURN specification";
    let before = other.query(show).unwrap().rows;
    txn.begin().unwrap();
    txn.query("ALTER CURRENT GRAPH TYPE SET { (:Private => {v :: STRING}) }")
        .unwrap();
    assert_ne!(txn.query(show).unwrap().rows, before);
    assert_eq!(other.query(show).unwrap().rows, before);
    txn.rollback().unwrap();
    assert_eq!(other.query(show).unwrap().rows, before);
}

#[test]
fn admitted_missing_graph_type_stays_missing_after_concurrent_create() {
    let db = Database::open(":memory:").unwrap();
    let pause = pause_next_read(&db);
    let mut reader = db.connect().unwrap();
    let read = std::thread::spawn(move || reader.query("SHOW GRAPH TYPE social"));
    pause.entered.wait();
    let mut writer = db.connect().unwrap();
    let (tx, rx) = mpsc::channel();
    let commit = std::thread::spawn(move || {
        tx.send(writer.query("CREATE GRAPH TYPE social AS { (:Person => {name :: STRING}) }"))
            .unwrap();
    });
    let written = rx.recv_timeout(Duration::from_secs(30));
    pause.release.wait();
    written
        .expect("graph-type commit must not wait for SHOW execution")
        .unwrap();
    commit.join().unwrap();
    assert!(read.join().unwrap().is_err());
    assert_eq!(
        db.connect()
            .unwrap()
            .query("SHOW GRAPH TYPE social YIELD name RETURN name")
            .unwrap()
            .rows,
        vec![vec![Value::String("social".into())]]
    );
}

#[test]
fn enforced_constraint_metadata_is_private_and_replays_with_its_definition() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph");
    let uri = path.to_str().unwrap();
    let show = "SHOW CONSTRAINTS YIELD name, enforcedLabel RETURN name, enforcedLabel";
    let create = "CREATE CONSTRAINT label_rule FOR (n:Person) REQUIRE n:Verified";
    {
        let db = Database::open(uri).unwrap();
        let mut txn = db.connect().unwrap();
        let mut other = db.connect().unwrap();
        txn.begin().unwrap();
        txn.query(create).unwrap();
        assert_eq!(
            txn.query(show).unwrap().rows,
            vec![vec![
                Value::String("label_rule".into()),
                Value::String("Verified".into())
            ]]
        );
        assert!(other.query(show).unwrap().rows.is_empty());
        txn.rollback().unwrap();
        assert!(other.query(show).unwrap().rows.is_empty());
        txn.begin().unwrap();
        txn.query(create).unwrap();
        txn.commit().unwrap();
        assert_eq!(
            other.query(show).unwrap().rows,
            vec![vec![
                Value::String("label_rule".into()),
                Value::String("Verified".into())
            ]]
        );
        // Exercise WAL replay without drop-time checkpoint (the checkpoint
        // representation has a separate schema-metadata contract).
        db.leak_without_checkpoint();
    }
    let db = Database::open(uri).unwrap();
    let mut conn = db.connect().unwrap();
    assert_eq!(
        conn.query(show).unwrap().rows,
        vec![vec![
            Value::String("label_rule".into()),
            Value::String("Verified".into())
        ]]
    );
    conn.query("DROP CONSTRAINT label_rule").unwrap();
    assert!(conn.query(show).unwrap().rows.is_empty());
}

#[test]
fn explicit_transaction_schema_is_captured_at_each_statement_admission() {
    let db = Database::open(":memory:").unwrap();
    let mut txn = db.connect().unwrap();
    txn.begin().unwrap();
    db.connect()
        .unwrap()
        .query("CREATE INDEX old_schema FOR (n:N) ON (n.v)")
        .unwrap();
    let pause = pause_next_read(&db);
    let read = std::thread::spawn(move || {
        let first = txn.query("SHOW INDEXES YIELD name RETURN name ORDER BY name");
        let second = txn.query("SHOW INDEXES YIELD name RETURN name ORDER BY name");
        txn.rollback().unwrap();
        (first, second)
    });
    pause.entered.wait();
    let mut writer = db.connect().unwrap();
    let (tx, rx) = mpsc::channel();
    let commit = std::thread::spawn(move || {
        tx.send(writer.query("CREATE INDEX new_schema FOR (n:N) ON (n.w)"))
            .unwrap();
    });
    let written = rx.recv_timeout(Duration::from_secs(30));
    pause.release.wait();
    written
        .expect("txn SHOW must not hold publication guard")
        .unwrap();
    commit.join().unwrap();
    let (first, second) = read.join().unwrap();
    assert_eq!(
        first.unwrap().rows,
        vec![vec![Value::String("old_schema".into())]]
    );
    assert_eq!(
        second.unwrap().rows,
        vec![
            vec![Value::String("new_schema".into())],
            vec![Value::String("old_schema".into())]
        ]
    );
}

#[test]
fn schema_show_admitted_before_poison_stays_safe_and_late_show_fails() {
    for (ddl, show) in [
        (
            "CREATE INDEX safe_schema FOR (n:N) ON (n.v)",
            "SHOW INDEXES YIELD name RETURN name",
        ),
        (
            "CREATE CONSTRAINT safe_schema FOR (n:N) REQUIRE n.v IS UNIQUE",
            "SHOW CONSTRAINTS YIELD name RETURN name",
        ),
    ] {
        let db = Database::open(":memory:").unwrap();
        db.connect().unwrap().query(ddl).unwrap();
        let pause = pause_next_read(&db);
        let mut reader = db.connect().unwrap();
        let mut late_reader = db.connect().unwrap();
        let read = std::thread::spawn(move || reader.query(show));
        pause.entered.wait();
        db.inner.fail_after_append.store(true, Ordering::SeqCst);
        let mut writer = db.connect().unwrap();
        let (tx, rx) = mpsc::channel();
        let commit =
            std::thread::spawn(move || tx.send(writer.query("CREATE (:Uncertain)")).unwrap());
        let poisoned = rx.recv_timeout(Duration::from_secs(30));
        pause.release.wait();
        assert!(poisoned
            .expect("publication must finish before SHOW resumes")
            .unwrap_err()
            .to_string()
            .contains("uncertain WAL outcome"));
        commit.join().unwrap();
        assert_eq!(
            read.join().unwrap().unwrap().rows,
            vec![vec![Value::String("safe_schema".into())]]
        );
        assert!(late_reader
            .query(show)
            .unwrap_err()
            .to_string()
            .contains("uncertain WAL outcome"));
    }
}

#[test]
fn explicit_transaction_read_uses_its_begin_pin_without_blocking_commit() {
    let db = Database::open(":memory:").unwrap();
    db.connect().unwrap().query("CREATE (:N)").unwrap();
    let pause = pause_next_read(&db);
    let mut txn = db.connect().unwrap();
    txn.begin().unwrap();
    let read = std::thread::spawn(move || {
        let result = txn.query("MATCH (n:N) RETURN count(n) AS total");
        txn.rollback().unwrap();
        result
    });
    pause.entered.wait();

    let mut writer = db.connect().unwrap();
    let (tx, rx) = mpsc::channel();
    let commit = std::thread::spawn(move || {
        tx.send(writer.query("CREATE (:N)")).unwrap();
    });
    let written = rx.recv_timeout(Duration::from_secs(30));
    pause.release.wait();
    written
        .expect("commit must finish while the transaction read is blocked")
        .unwrap();
    commit.join().unwrap();
    assert_eq!(
        read.join().unwrap().unwrap().rows,
        vec![vec![Value::Int(1)]]
    );
}

#[test]
fn read_admitted_before_poison_finishes_on_safe_snapshot() {
    let db = Database::open(":memory:").unwrap();
    db.connect().unwrap().query("CREATE (:Safe)").unwrap();
    let pause = pause_next_read(&db);
    let mut late_reader = db.connect().unwrap();
    let mut reader = db.connect().unwrap();
    let read = std::thread::spawn(move || reader.query("MATCH (n) RETURN count(n) AS total"));
    pause.entered.wait();

    db.inner.fail_after_append.store(true, Ordering::SeqCst);
    let mut writer = db.connect().unwrap();
    let (tx, rx) = mpsc::channel();
    let commit = std::thread::spawn(move || {
        tx.send(writer.query("CREATE (:Uncertain)")).unwrap();
    });
    let poisoned = rx.recv_timeout(Duration::from_secs(30));
    pause.release.wait();
    let err = poisoned
        .expect("poison publication must finish before the reader is released")
        .unwrap_err();
    assert!(err.to_string().contains("uncertain WAL outcome"));
    commit.join().unwrap();
    assert_eq!(
        read.join().unwrap().unwrap().rows,
        vec![vec![Value::Int(1)]]
    );
    assert!(late_reader
        .query("MATCH (n) RETURN n")
        .unwrap_err()
        .to_string()
        .contains("uncertain WAL outcome"));
}

/// The `QueryOutcome` contract (ADR-0319) on the PROFILE path: an admitted
/// read keeps the rows it executed on its own pinned snapshot even when the
/// store is poisoned while it runs. PROFILE's epilogue reads the store again
/// for the tree's estimates (ADR-0218), and that post-run guard is refused by
/// the poisoned store — the refusal must not discard the finished result. The
/// estimates are a rendering detail, so the rows ride un-enriched: the
/// missing plan tree is the proof the refused guard was taken (a guard that
/// had succeeded would always stamp the tree).
#[test]
fn profiled_read_keeps_its_rows_when_the_post_run_guard_is_refused() {
    let db = Database::open(":memory:").unwrap();
    db.connect()
        .unwrap()
        .query("CREATE (:Safe {v: 1})")
        .unwrap();
    let pause = pause_next_read(&db);
    let mut late_reader = db.connect().unwrap();
    let mut reader = db.connect().unwrap();
    let read = std::thread::spawn(move || reader.query("PROFILE MATCH (n:Safe) RETURN n.v AS v"));
    pause.entered.wait();

    db.inner.fail_after_append.store(true, Ordering::SeqCst);
    let mut writer = db.connect().unwrap();
    let (tx, rx) = mpsc::channel();
    let commit = std::thread::spawn(move || {
        tx.send(writer.query("CREATE (:Uncertain)")).unwrap();
    });
    let poisoned = rx.recv_timeout(Duration::from_secs(30));
    pause.release.wait();
    let err = poisoned
        .expect("poison publication must finish before the reader is released")
        .unwrap_err();
    assert!(err.to_string().contains("uncertain WAL outcome"));
    commit.join().unwrap();
    let result = read
        .join()
        .unwrap()
        .expect("the admitted read's rows must survive the refused post-run guard");
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
    assert!(
        result.plan.is_none(),
        "the refused guard left the result un-enriched"
    );
    assert!(late_reader
        .query("MATCH (n) RETURN n")
        .unwrap_err()
        .to_string()
        .contains("uncertain WAL outcome"));
}

#[test]
fn poison_before_admission_rejects_reads_and_explicit_transaction_commit() {
    let db = Database::open(":memory:").unwrap();
    let mut txn = db.connect().unwrap();
    let mut late_reader = db.connect().unwrap();
    txn.begin().unwrap();
    txn.query("CREATE (:Private)").unwrap();

    db.inner.fail_after_append.store(true, Ordering::SeqCst);
    let err = db
        .connect()
        .unwrap()
        .query("CREATE (:Uncertain)")
        .unwrap_err();
    assert!(err.to_string().contains("uncertain WAL outcome"));
    let pause = pause_next_read(&db);
    let err = late_reader.query("RETURN 1 AS x").unwrap_err();
    assert!(err.to_string().contains("uncertain WAL outcome"));
    assert!(db.inner.read_pause.lock().unwrap().is_some()); // never executed
    let err = txn.query("RETURN 1 AS x").unwrap_err();
    assert!(err.to_string().contains("uncertain WAL outcome"));
    assert!(txn
        .commit()
        .unwrap_err()
        .to_string()
        .contains("uncertain WAL outcome"));
    assert!(txn.in_transaction());
    txn.rollback().unwrap();
    drop(pause);
}
