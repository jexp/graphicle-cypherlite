use std::time::Duration;

use crate::{Database, DurabilityMode, OpenOptions, SnapshotCodecKind, Value};

fn no_timer() -> OpenOptions {
    OpenOptions {
        wal_dir: None,
        checkpoint_interval: Some(Duration::ZERO),
        durability_mode: Default::default(),
        logical_graph_size_limit: None,
        snapshot_codec: None,
        load_csv_dir: None,
        load_csv_max_bytes: None,
        mode: crate::OpenMode::Writable,
        token_admission: false,
        concurrent_logical_workspace_limit: None,
        follow_gap: Default::default(),
    }
}

#[test]
fn open_vfs_runs_on_the_caller_store() {
    use std::sync::Arc;

    use cypherlite_storage::MemoryVfs;

    let vfs: Arc<dyn cypherlite_storage::Vfs> = Arc::new(MemoryVfs::new());
    let db = Database::open_vfs(vfs, OpenOptions::default()).unwrap();
    let mut conn = db.connect().unwrap();
    conn.execute("CREATE (:N)").unwrap();
    let found = conn.execute("MATCH (n) RETURN count(n) AS c").unwrap();
    assert_eq!(found.rows[0][0], Value::Int(1));
}

/// ADR-0172: incremental validation state is rebuilt on open. On a store
/// reopened over one shared `Arc<MemoryVfs>`, relationship `unique` still
/// rejects its duplicate, and a composite node key with ten values of `a`
/// still rejects a duplicate tuple and accepts a distinct one.
#[test]
fn constraint_validation_survives_reopen_on_a_shared_memory_vfs() {
    use std::sync::Arc;

    use cypherlite_storage::MemoryVfs;

    let vfs: Arc<dyn cypherlite_storage::Vfs> = Arc::new(MemoryVfs::new());
    {
        let db = Database::open_vfs(Arc::clone(&vfs), no_timer()).unwrap();
        let mut conn = db.connect().unwrap();
        conn.query("CREATE CONSTRAINT r_id FOR ()-[r:R]-() REQUIRE r.id IS UNIQUE")
            .unwrap();
        conn.query("CREATE ()-[:R {id: 1}]->()").unwrap();
        conn.query("CREATE CONSTRAINT p_abk FOR (n:P) REQUIRE (n.a, n.b) IS NODE KEY")
            .unwrap();
        for i in 1..=10 {
            conn.query(&format!("CREATE (:P {{a: {i}, b: {i}}})"))
                .unwrap();
        }
        db.checkpoint().unwrap();
    }
    let db = Database::open_vfs(vfs, no_timer()).unwrap();
    let mut conn = db.connect().unwrap();
    assert_eq!(
        conn.query("CREATE ()-[:R {id: 1}]->()")
            .expect_err("the relationship duplicate must be rejected after reopen")
            .to_string(),
        "semantic error: ConstraintValidationFailed: `r_id` uniqueness"
    );
    conn.query("CREATE ()-[:R {id: 2}]->()").unwrap();
    assert_eq!(
        conn.query("CREATE (:P {a: 10, b: 10})")
            .expect_err("the duplicate tuple must be rejected after reopen")
            .to_string(),
        "semantic error: ConstraintValidationFailed: `p_abk` uniqueness"
    );
    conn.query("CREATE (:P {a: 10, b: 11})").unwrap();
}

#[test]
fn database_open_rejects_opfs_scheme() {
    let err = Database::open("opfs://playground")
        .err()
        .expect("opfs:// is not a Database::open URI");
    assert!(err.to_string().contains("opfs://"), "{err}");
}

#[test]
fn open_options_default_durability_is_full() {
    assert_eq!(DurabilityMode::default(), DurabilityMode::Full);
    assert_eq!(OpenOptions::default().durability_mode, DurabilityMode::Full);
}

#[test]
fn checkpoint_recycles_enforced_label_and_show_theater_for_both_codecs() {
    for &codec in SnapshotCodecKind::compiled() {
        let dir = tempfile::tempdir().unwrap();
        let uri = dir.path().to_str().unwrap();
        let show_constraint =
            "SHOW CONSTRAINTS YIELD name, enforcedLabel WHERE name = 'label_rule' RETURN name, enforcedLabel";
        let show_graph_type = "SHOW GRAPH TYPE social YIELD specification RETURN specification";
        let show_database = "SHOW DATABASE neo4j YIELD defaultLanguage RETURN defaultLanguage";
        let show_index = "SHOW INDEXES YIELD name, type WHERE name = 'people' RETURN name, type";
        let show_options =
            "SHOW INDEXES YIELD name, options WHERE name = 'people_vec' RETURN name, options";
        let expected;
        {
            let db = Database::open_with(
                uri,
                OpenOptions {
                    snapshot_codec: Some(codec),
                    ..no_timer()
                },
            )
            .unwrap();
            let mut conn = db.connect().unwrap();
            conn.query("CREATE CONSTRAINT label_rule FOR (n:Person) REQUIRE n:Verified")
                .unwrap();
            conn.query("CREATE GRAPH TYPE social AS { (:Member => {name :: STRING}) }")
                .unwrap();
            conn.query("ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5")
                .unwrap();
            conn.query("CREATE TEXT INDEX people FOR (n:Person) ON (n.name)")
                .unwrap();
            conn.query("CREATE VECTOR INDEX people_vec FOR (n:Person) ON n.embedding OPTIONS {indexConfig: {`vector.dimensions`: 2}}")
                .unwrap();
            expected = [
                show_constraint,
                show_graph_type,
                show_database,
                show_index,
                show_options,
            ]
            .map(|q| conn.query(q).unwrap().rows);
            let err = conn.query("CREATE (:Person)").unwrap_err();
            assert!(matches!(err, crate::Error::Execution(_)), "{err}");
            db.checkpoint().unwrap();
        }
        let wal_files: Vec<_> = std::fs::read_dir(dir.path().join("wal"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !wal_files.iter().any(|name| name == "00000001.log"),
            "{codec:?}: {wal_files:?}"
        );
        // The checkpoint LSN is already past the DDL records; recovery must
        // work from the published snapshot even when those records are gone.
        let db = Database::open_with(uri, no_timer()).unwrap();
        let mut conn = db.connect().unwrap();
        for (query, rows) in [
            show_constraint,
            show_graph_type,
            show_database,
            show_index,
            show_options,
        ]
        .into_iter()
        .zip(expected)
        {
            assert_eq!(conn.query(query).unwrap().rows, rows, "{codec:?}: {query}");
        }
        assert_eq!(
            conn.query(show_constraint).unwrap().rows,
            vec![vec![
                Value::String("label_rule".into()),
                Value::String("Verified".into())
            ]]
        );
        let err = conn.query("CREATE (:Person)").unwrap_err();
        assert!(
            matches!(err, crate::Error::Execution(_)),
            "{codec:?}: {err}"
        );
        conn.query("CREATE (:Person:Verified)").unwrap();
    }
}

#[test]
fn legacy_untyped_label_constraint_survives_checkpoint_and_wal_recycle() {
    for &codec in SnapshotCodecKind::compiled() {
        let dir = tempfile::tempdir().unwrap();
        let uri = dir.path().to_str().unwrap();
        let options = OpenOptions {
            snapshot_codec: Some(codec),
            ..no_timer()
        };
        {
            let db = Database::open_with(uri, options.clone()).unwrap();
            db.connect()
                .unwrap()
                .query("CREATE CONSTRAINT label_rule FOR (n:Person) REQUIRE n:Verified")
                .unwrap();
            db.checkpoint().unwrap();
        }
        let path = dir.path().join(match codec {
            SnapshotCodecKind::Parquet => "manifest.json",
            SnapshotCodecKind::Json => "graph.json",
        });
        let mut old: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if codec == SnapshotCodecKind::Parquet {
            assert_eq!(old["format_version"], 3);
        } else {
            old["constraints"][0]
                .as_object_mut()
                .unwrap()
                .remove("enforced_label");
        }
        old.as_object_mut().unwrap().remove("theater");
        std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();

        let show = "SHOW CONSTRAINTS YIELD name, enforcedLabel WHERE name = 'label_rule' RETURN name, enforcedLabel";
        let known_show = "SHOW CONSTRAINTS YIELD name, enforcedLabel WHERE name = 'known_rule' RETURN name, enforcedLabel";
        let database_show = "SHOW DATABASE neo4j YIELD defaultLanguage RETURN defaultLanguage";
        let expected_database;
        {
            let db = Database::open_with(uri, options).unwrap();
            let mut conn = db.connect().unwrap();
            assert_eq!(
                conn.query(show).unwrap().rows,
                vec![vec![Value::String("label_rule".into()), Value::Null]]
            );
            conn.query("CREATE (:Person)").unwrap();
            conn.query("ALTER DATABASE neo4j SET DEFAULT LANGUAGE CYPHER 5")
                .unwrap();
            conn.query("CREATE CONSTRAINT known_rule FOR (n:Known) REQUIRE n:Verified")
                .unwrap();
            expected_database = conn.query(database_show).unwrap().rows;
            assert_eq!(expected_database.len(), 1);
            assert_eq!(
                conn.query(known_show).unwrap().rows,
                vec![vec![
                    Value::String("known_rule".into()),
                    Value::String("Verified".into())
                ]]
            );
            db.checkpoint().unwrap();
        }
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            saved["theater"]["legacy_unknown_constraints"]["label_rule"]["name"], "label_rule",
            "{codec:?}: {saved}"
        );
        assert_eq!(
            saved["theater"]["derived_constraints"]["known_rule"]["enforced_label"],
            "Verified"
        );
        let wal_files: Vec<_> = std::fs::read_dir(dir.path().join("wal"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !wal_files.iter().any(|name| name == "00000001.log"),
            "{codec:?}: {wal_files:?}"
        );
        let db = Database::open(uri).unwrap();
        let mut conn = db.connect().unwrap();
        assert_eq!(
            conn.query(show).unwrap().rows,
            vec![vec![Value::String("label_rule".into()), Value::Null]]
        );
        assert_eq!(conn.query(database_show).unwrap().rows, expected_database);
        assert_eq!(
            conn.query(known_show).unwrap().rows,
            vec![vec![
                Value::String("known_rule".into()),
                Value::String("Verified".into())
            ]]
        );
        conn.query("CREATE (:Person)").unwrap();
        assert!(matches!(
            conn.query("CREATE (:Known)"),
            Err(crate::Error::Execution(_))
        ));
        conn.query("CREATE (:Known:Verified)").unwrap();
    }
}

#[test]
fn open_rejects_incomplete_typed_theater_after_wal_recycle() {
    for &codec in SnapshotCodecKind::compiled() {
        let dir = tempfile::tempdir().unwrap();
        let uri = dir.path().to_str().unwrap();
        {
            let db = Database::open_with(
                uri,
                OpenOptions {
                    snapshot_codec: Some(codec),
                    ..no_timer()
                },
            )
            .unwrap();
            db.connect()
                .unwrap()
                .query("CREATE CONSTRAINT label_rule FOR (n:Person) REQUIRE n:Verified")
                .unwrap();
            db.checkpoint().unwrap();
        }
        let wal_files: Vec<_> = std::fs::read_dir(dir.path().join("wal"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!wal_files.iter().any(|name| name == "00000001.log"));

        let path = dir.path().join(match codec {
            SnapshotCodecKind::Parquet => "manifest.json",
            SnapshotCodecKind::Json => "graph.json",
        });
        let mut data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(data["theater"]["derived_constraints"]
            .as_object_mut()
            .unwrap()
            .remove("label_rule")
            .is_some());
        std::fs::write(path, serde_json::to_vec(&data).unwrap()).unwrap();
        let err = Database::open(uri)
            .err()
            .expect("incomplete theater must fail open");
        assert!(matches!(err, crate::Error::Storage(_)), "{codec:?}: {err}");
    }
}

#[test]
fn checkpoint_restores_current_graph_type_and_derived_constraints() {
    for &codec in SnapshotCodecKind::compiled() {
        let dir = tempfile::tempdir().unwrap();
        let uri = dir.path().to_str().unwrap();
        let show = "SHOW CURRENT GRAPH TYPE YIELD specification RETURN specification";
        let expected;
        {
            let db = Database::open_with(
                uri,
                OpenOptions {
                    snapshot_codec: Some(codec),
                    ..no_timer()
                },
            )
            .unwrap();
            let mut conn = db.connect().unwrap();
            conn.query("ALTER CURRENT GRAPH TYPE SET { (:Person => {name :: STRING}) }")
                .unwrap();
            expected = conn.query(show).unwrap().rows;
            db.checkpoint().unwrap();
        }
        let db = Database::open_with(uri, no_timer()).unwrap();
        assert_eq!(db.connect().unwrap().query(show).unwrap().rows, expected);
    }
}

#[test]
fn bundle_cl_survives_reopen_from_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    let uri = path.to_str().unwrap();
    {
        let db = Database::open_with(uri, no_timer()).unwrap();
        db.connect()
            .unwrap()
            .query("CREATE (n:Person {name: 'Ada'})")
            .unwrap();
        db.leak_without_checkpoint();
    }
    assert!(path.is_file());
    assert!(!dir.path().join("wal").is_dir());
    let db = Database::open_with(uri, no_timer()).unwrap();
    let r = db
        .connect()
        .unwrap()
        .query("MATCH (n:Person) RETURN n.name AS name")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::String("Ada".into()));
}

#[test]
fn bundle_cl_checkpoint_then_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    let uri = path.to_str().unwrap();
    {
        let db = Database::open_with(uri, no_timer()).unwrap();
        db.connect()
            .unwrap()
            .query("CREATE (n:Person {name: 'Ada'})")
            .unwrap();
        db.checkpoint().unwrap();
    }
    assert!(path.is_file());
    let db = Database::open_with(uri, no_timer()).unwrap();
    let r = db
        .connect()
        .unwrap()
        .query("MATCH (n:Person) RETURN n.name AS name")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::String("Ada".into()));
}

#[test]
fn relaxed_modes_checkpoint_and_reopen_native_and_bundle() {
    for mode in [DurabilityMode::EverySecond, DurabilityMode::Snapshot] {
        for bundle in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = if bundle {
                dir.path().join("graph.cl")
            } else {
                dir.path().join("graph")
            };
            let uri = path.to_str().unwrap();
            let mut opts = no_timer();
            opts.durability_mode = mode;
            {
                let db = Database::open_with(uri, opts.clone()).unwrap();
                db.connect()
                    .unwrap()
                    .query("CREATE (:Persisted {value: 1})")
                    .unwrap();
                db.checkpoint().unwrap();
                db.leak_without_checkpoint();
            }
            let db = Database::open_with(uri, opts).unwrap();
            let result = db
                .connect()
                .unwrap()
                .query("MATCH (n:Persisted) RETURN count(n) AS n")
                .unwrap();
            assert_eq!(result.rows[0][0], Value::Int(1), "{mode} bundle={bundle}");
        }
    }
}

#[test]
fn file_uri_cl_opens_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    let uri = format!("file://{}", path.display());
    let db = Database::open_with(&uri, no_timer()).unwrap();
    db.connect().unwrap().query("CREATE (:N)").unwrap();
    drop(db);
    let db = Database::open_with(&uri, no_timer()).unwrap();
    let r = db
        .connect()
        .unwrap()
        .query("MATCH (n:N) RETURN count(n) AS c")
        .unwrap();
    assert_eq!(r.rows[0][0], Value::Int(1));
}

#[test]
fn native_tempdir_still_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let db = Database::open_with(uri, no_timer()).unwrap();
    db.connect().unwrap().query("CREATE (:N)").unwrap();
    drop(db);
    assert!(dir.path().is_dir());
}

#[cfg(not(feature = "parquet"))]
#[test]
fn parquet_stores_fail_closed_at_open_without_the_codec() {
    let unavailable = crate::Error::storage(
        "parquet snapshot codec is not compiled into this build (enable the \"parquet\" feature)",
    );
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("manifest.json"),
        r#"{"format_version":3,"next_node_id":0,"next_rel_id":0}"#,
    )
    .unwrap();
    let error = Database::open_with(dir.path().to_str().unwrap(), no_timer())
        .err()
        .expect("parquet store must not open");
    assert_eq!(error, unavailable);

    let mut explicit = no_timer();
    explicit.snapshot_codec = Some(SnapshotCodecKind::Parquet);
    let error = Database::open_with(":memory:", explicit)
        .err()
        .expect("explicit parquet must not open");
    assert_eq!(error, unavailable);
}
