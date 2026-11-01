//! WAL follow (ADR-0137): catch up from new frames, reload across a gap.

use std::fs;
use std::io::Read;
use std::thread;
use std::time::{Duration, Instant};

use crate::test_opts::{count, following, wait_count, writable};
use crate::{Database, Error, FollowGapPolicy, OpenOptions, Value};

#[test]
fn follow_applies_commits_and_keeps_an_open_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();

    let follower = Database::open_with(uri, following()).unwrap();
    assert_eq!(count(&follower, "Seed"), 1);
    let mut held = follower.connect().unwrap();
    held.begin().unwrap();

    writer
        .connect()
        .unwrap()
        .execute("CREATE (:Later)")
        .unwrap();
    wait_count(&follower, "Later", 1);
    assert_eq!(
        held.query("MATCH (n:Later) RETURN count(n)").unwrap().rows[0][0],
        Value::Int(0)
    );
    held.rollback().unwrap();
}

#[test]
fn follow_caught_up_through_checkpoint_does_not_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    let follower = Database::open_with(uri, following()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Next)").unwrap();
    wait_count(&follower, "Next", 1);
    let reloads = follower.follow_reloads();
    writer.checkpoint().unwrap();
    thread::sleep(Duration::from_millis(200));
    assert_eq!(follower.follow_reloads(), reloads);
    assert_eq!(count(&follower, "Next"), 1);
}

#[test]
fn follow_behind_recycle_reloads_from_the_snapshot() {
    // The parked-gap scenario the shared helper walks, with no extra
    // writer-side state to carry: only the fresh-follower comparison below.
    let (dir, follower) = park_gap_and_reload(
        |_| {},
        |follower| {
            assert_eq!(count(follower, "Missed"), 0);
        },
    );
    let uri = dir.path().join("db");
    let fresh = Database::open_with(uri.to_str().unwrap(), following()).unwrap();
    assert_eq!(count(&fresh, "Missed"), count(&follower, "Missed"));
}

/// The `defaultLanguage` `SHOW DATABASES` reports for `name` — the
/// store-level databases catalog (ADR-0276).
fn show_default_language(db: &Database, name: &str) -> Value {
    let result = db
        .connect()
        .unwrap()
        .query("SHOW DATABASES YIELD name, defaultLanguage")
        .unwrap();
    let name_col = result
        .header
        .columns
        .iter()
        .position(|column| column == "name")
        .expect("SHOW DATABASES carries a name column");
    let language_col = result
        .header
        .columns
        .iter()
        .position(|column| column == "defaultLanguage")
        .expect("SHOW DATABASES carries a defaultLanguage column");
    let row = result
        .rows
        .into_iter()
        .find(|row| row[name_col] == Value::String(name.into()))
        .unwrap_or_else(|| panic!("SHOW DATABASES carries a {name} row"));
    row[language_col].clone()
}

/// The parked-gap-reload scenario both carry tests walk: open a writer with
/// one seeded checkpoint, let a follower catch up, park the follower, run
/// the writer's `write` plus a checkpoint while the follower sees nothing,
/// assert the follower still holds its stale state (`stale`), then resume it
/// and wait for the checkpoint-only `Missed` node — proof the reload
/// installed the snapshot. Returns the follower for the carried-state
/// assertion.
fn park_gap_and_reload(
    write: impl FnOnce(&Database),
    stale: impl FnOnce(&Database),
) -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Seed", 1);

    follower.park_follow();
    writer
        .connect()
        .unwrap()
        .execute("CREATE (:Missed)")
        .unwrap();
    write(&writer);
    writer.checkpoint().unwrap();
    // The parked follower saw neither record, so the reload is the only path
    // that can carry the writer's state across.
    stale(&follower);

    follower.resume_follow();
    // `Missed` exists only in the checkpoint, so observing it proves the
    // reload installed the snapshot.
    wait_count(&follower, "Missed", 1);
    assert!(follower.follow_reloads() >= 1);
    assert!(
        !follower.needs_reopen(),
        "the default reload policy installs the snapshot itself"
    );
    (dir, follower)
}

fn yielding() -> OpenOptions {
    OpenOptions {
        follow_gap: FollowGapPolicy::Yield,
        ..following()
    }
}

#[test]
fn follow_gap_yield_does_not_reload_and_refuses_queries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    let follower = Database::open_with(uri, yielding()).unwrap();
    wait_count(&follower, "Seed", 1);
    let reloads = follower.follow_reloads();

    follower.park_follow();
    writer
        .connect()
        .unwrap()
        .execute("CREATE (:Missed)")
        .unwrap();
    writer.checkpoint().unwrap();
    assert_eq!(count(&follower, "Missed"), 0);
    follower.resume_follow();

    let start = Instant::now();
    while !follower.needs_reopen() {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "yield did not latch follow-gap"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(follower.follow_reloads(), reloads);
    let err = match follower.connect() {
        Err(err) => err,
        Ok(_) => panic!("a yielded follower still accepted a connection"),
    };
    match err {
        Error::ResourceLimit {
            name: Error::FOLLOW_GAP,
            retryable: true,
            ..
        } => {}
        other => panic!("expected retryable follow-gap, got {other}"),
    }
}

#[test]
fn follow_apply_failure_yields_without_reloading() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    let follower = Database::open_with(uri, yielding()).unwrap();
    wait_count(&follower, "Seed", 1);
    let reloads = follower.follow_reloads();
    follower.park_follow();
    writer.connect().unwrap().execute("CREATE (:A)").unwrap();
    writer.connect().unwrap().execute("CREATE (:B)").unwrap();
    follower.fail_follow_apply_on(2);
    follower.resume_follow();

    let start = Instant::now();
    while !follower.needs_reopen() {
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "a torn apply did not yield"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(follower.follow_reloads(), reloads);
    let err = match follower.connect() {
        Err(err) => err,
        Ok(_) => panic!("a yielded follower still accepted a connection"),
    };
    assert!(
        matches!(
            err,
            Error::ResourceLimit {
                name: Error::FOLLOW_GAP,
                retryable: true,
                ..
            }
        ),
        "{err}"
    );
}

/// ADR-0276 across a gap reload: the databases catalog is durable state.
/// `open_store_readonly` restores the checkpoint's table into the freshly
/// loaded graph's own bundle, so the reload must install that table into
/// the store's surviving bundle before `share_services_from` adopts it.
/// Without the carry the share keeps the follower's stale in-memory table,
/// and an `ALTER DATABASE` committed in the checkpoint's gap window stays
/// invisible until the next such write.
#[test]
fn follow_gap_reload_keeps_the_checkpoint_databases_catalog() {
    let (_dir, follower) = park_gap_and_reload(
        |writer| {
            writer
                .connect()
                .unwrap()
                .execute("ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5")
                .unwrap();
        },
        |follower| {
            assert_eq!(
                show_default_language(follower, "neo4j"),
                Value::String("CYPHER 25".into()),
                "the parked follower must still hold the stale catalog"
            );
        },
    );
    assert_eq!(
        show_default_language(&follower, "neo4j"),
        Value::String("CYPHER 5".into()),
        "the reload discarded the checkpoint's databases catalog"
    );
}

/// A valid ontology document for the gap-reload test (the shape
/// `tests/ontology_store.rs` uses).
const ONTOLOGY_DOC: &str = concat!(
    "{\"schema\":\"https://neo4j.com/docs/ontology-graph-spec/1.0.0/schema.json\",",
    "\"id\":\"twin\",\"version\":\"1\",\"name\":\"Twin\",",
    "\"nodes\":{\"Person\":{\"label\":\"Person\",\"properties\":{",
    "\"id\":{\"type\":\"STRING\",\"key\":true}}}},",
    "\"relationships\":{}}"
);

/// ADR-0120 §1: the checkpoint's ontology document is store-level durable
/// state, so the gap reload must install it — a follower parked across a
/// writer's `db.ontology.set` + checkpoint would otherwise serve the stale
/// (absent) document until the next such write. The databases-catalog
/// sibling of this carry is pinned by
/// `follow_gap_reload_keeps_the_checkpoint_databases_catalog`.
#[test]
fn follow_gap_reload_keeps_the_checkpoint_ontology_document() {
    let (_dir, follower) = park_gap_and_reload(
        |writer| {
            writer
                .commit_ontology_set(ONTOLOGY_DOC.to_string())
                .unwrap();
        },
        |follower| {
            assert_eq!(
                follower.ontology_document().unwrap(),
                None,
                "the parked follower must still hold the stale document"
            );
        },
    );
    assert_eq!(
        follower.ontology_document().unwrap(),
        Some(ONTOLOGY_DOC.to_string()),
        "the reload discarded the checkpoint's ontology document"
    );
}

#[test]
fn follow_truncation_reloads_without_trimming() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    writer.connect().unwrap().execute("CREATE (:Tail)").unwrap();
    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Tail", 1);
    follower.park_follow();
    writer.leak_without_checkpoint();

    let segment = wal_segment(&path);
    let before = fs::metadata(&segment).unwrap().len();
    assert!(before > 8);
    let file = fs::OpenOptions::new().write(true).open(&segment).unwrap();
    file.set_len(cypherlite_storage::MAGIC.len() as u64)
        .unwrap();
    drop(file);
    let truncated = fs::metadata(&segment).unwrap().len();

    follower.resume_follow();
    wait_count(&follower, "Tail", 0);
    assert!(follower.follow_reloads() >= 1);
    assert_eq!(fs::metadata(&segment).unwrap().len(), truncated);
    let mut bytes = Vec::new();
    fs::File::open(&segment)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes.len() as u64, truncated);
}

#[test]
fn follow_pinned_snapshot_reports_reload_stall_then_catches_up() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Seed", 1);
    follower.park_follow();
    writer
        .connect()
        .unwrap()
        .execute("CREATE (:Missed)")
        .unwrap();
    writer.checkpoint().unwrap();
    let mut pinned = follower.connect().unwrap();
    pinned.begin().unwrap();
    follower.resume_follow();

    let start = Instant::now();
    let mut message = String::new();
    while start.elapsed() < Duration::from_secs(2) {
        match follower.connect() {
            Err(err) => {
                message = err.to_string();
                if message.contains("snapshot pin") {
                    break;
                }
            }
            Ok(_) => thread::sleep(Duration::from_millis(5)),
        }
    }
    assert!(
        message.contains("snapshot pin"),
        "stall was not reported: {message}"
    );
    pinned.rollback().unwrap();
    let start = Instant::now();
    while follower.connect().is_err() {
        if start.elapsed() > Duration::from_secs(2) {
            panic!("stall error was not cleared");
        }
        thread::sleep(Duration::from_millis(20));
    }
    wait_count(&follower, "Missed", 1);
}

#[test]
fn follow_trimmed_tail_reloads_instead_of_applying_regrowth() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    writer.connect().unwrap().execute("CREATE (:Tail)").unwrap();
    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Tail", 1);
    follower.park_follow();
    writer.leak_without_checkpoint();

    let segment = wal_segment(&path);
    let mut bytes = fs::read(&segment).unwrap();
    let magic = cypherlite_storage::MAGIC.len();
    assert!(bytes.len() > magic);
    bytes.truncate(magic + 4);
    fs::write(&segment, &bytes).unwrap();

    let writer = Database::open_with(uri, writable()).unwrap();
    assert_eq!(
        fs::metadata(&segment).unwrap().len(),
        magic as u64,
        "recovery must leave the trimmed segment at the intact prefix"
    );
    writer
        .connect()
        .unwrap()
        .execute("CREATE (:After)")
        .unwrap();
    assert_eq!(
        fs::metadata(&segment).unwrap().len(),
        magic as u64,
        "appends after a trim must use a new segment"
    );

    follower.resume_follow();
    wait_count(&follower, "After", 1);
    assert_eq!(count(&follower, "Tail"), 0);
    assert!(follower.follow_reloads() >= 1);
}

#[test]
fn follow_apply_failure_stays_failed_when_reload_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Seed", 1);
    follower.park_follow();
    writer.connect().unwrap().execute("CREATE (:A)").unwrap();
    writer.connect().unwrap().execute("CREATE (:B)").unwrap();
    follower.fail_follow_apply_on(2);
    follower.fail_next_follow_reload();
    follower.resume_follow();

    let start = Instant::now();
    let mut message = String::new();
    while start.elapsed() < Duration::from_secs(2) {
        match follower.connect() {
            Err(err) => {
                message = err.to_string();
                if message.contains("injected follow reload failure") {
                    break;
                }
            }
            Ok(_) => thread::sleep(Duration::from_millis(20)),
        }
    }
    assert!(
        message.contains("injected follow reload failure"),
        "connect returned {message}"
    );
    thread::sleep(Duration::from_millis(150));
    let again = match follower.connect() {
        Err(err) => err.to_string(),
        Ok(_) => panic!("follower recovered after a failed reload"),
    };
    assert!(
        again.contains("injected follow reload failure"),
        "follower recovered: {again}"
    );
    writer.connect().unwrap().execute("CREATE (:C)").unwrap();
    thread::sleep(Duration::from_millis(150));
    let still = match follower.connect() {
        Err(err) => err.to_string(),
        Ok(_) => panic!("follower applied after poison"),
    };
    assert!(
        still.contains("injected follow reload failure"),
        "follower applied after poison: {still}"
    );
}

/// One failed poll records the failure in the follow health state. The next
/// poll must still apply the pending frames and clear it, with no writer
/// checkpoint to force a reload. Before the fix the apply refused its own
/// stale error forever.
#[test]
fn follow_transient_poll_error_clears_on_the_next_apply() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Seed", 1);
    follower.park_follow();
    writer
        .connect()
        .unwrap()
        .execute("CREATE (:Later)")
        .unwrap();
    follower.fail_next_follow_poll();
    follower.resume_follow();

    let start = Instant::now();
    let mut saw_error = false;
    loop {
        // The error can land between `connect` and `query`; either call may
        // report it while the failed poll is the latest one.
        let outcome = follower
            .connect()
            .and_then(|mut conn| conn.query("MATCH (n:Later) RETURN count(n)"));
        match outcome {
            Err(err) => {
                assert!(
                    err.to_string().contains("injected follow poll failure"),
                    "{err}"
                );
                saw_error = true;
            }
            Ok(result) => {
                if result.rows[0][0] == Value::Int(1) {
                    break;
                }
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "follower did not apply after a transient poll error (error seen: {saw_error})"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(follower.follow_reloads(), 0);
    follower.connect().unwrap();
}

fn wal_segment(root: &std::path::Path) -> std::path::PathBuf {
    let mut logs: Vec<_> = fs::read_dir(root.join("wal"))
        .unwrap()
        .map(|ent| ent.unwrap().path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("log"))
        .collect();
    logs.sort();
    logs.pop().unwrap()
}

#[test]
fn follow_refuses_memory_and_bundle() {
    let err = match Database::open_with(
        ":memory:",
        OpenOptions {
            mode: crate::OpenMode::Follow,
            ..OpenOptions::default()
        },
    ) {
        Err(err) => err,
        Ok(_) => panic!("follow on :memory: opened"),
    };
    assert!(err.to_string().contains("native directory"), "{err}");

    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("g.cl");
    drop(Database::open(bundle.to_str().unwrap()).unwrap());
    let err = match Database::open_with(
        bundle.to_str().unwrap(),
        OpenOptions {
            mode: crate::OpenMode::Follow,
            ..OpenOptions::default()
        },
    ) {
        Err(err) => err,
        Ok(_) => panic!("follow on a bundle opened"),
    };
    assert!(err.to_string().contains("native directory"), "{err}");
}
