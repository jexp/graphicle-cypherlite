use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use cypherlite::{Database, Error, OpenOptions, Value};

fn options(root: &Path, limit: Option<u64>) -> OpenOptions {
    OpenOptions {
        load_csv_dir: Some(root.to_path_buf()),
        load_csv_max_bytes: limit,
        mode: cypherlite::OpenMode::Writable,
        ..OpenOptions::default()
    }
}

fn local_file_url(path: &Path) -> String {
    // tempfile gives an absolute path: /tmp/... or C:\... . The URL is
    // file:///tmp/... or file:///C:/..., with no authority or backslashes.
    let path = path.to_str().unwrap().replace('\\', "/");
    format!("file:///{}", path.trim_start_matches('/'))
}

fn csv(db: &Database, source: &str) -> Result<Vec<Vec<Value>>, Error> {
    let mut conn = db.connect().unwrap();
    conn.query_with_params(
        "LOAD CSV FROM $src AS row RETURN row",
        BTreeMap::from([("src".into(), Value::String(source.into()))]),
    )
    .map(|r| r.rows)
}

fn assert_unavailable(db: &Database, source: &str) {
    let error = csv(db, source).unwrap_err();
    assert_eq!(error, Error::execution("LOAD CSV file unavailable"));
    assert!(!error.to_string().contains(source));
}

#[test]
fn default_denies_existing_local_files_and_fixture_is_canned() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private.csv");
    fs::write(&path, "secret\n").unwrap();
    let db = Database::open(":memory:").unwrap();
    assert_unavailable(&db, path.to_str().unwrap());
    assert_unavailable(&db, &local_file_url(&path));
    assert_unavailable(&db, "file:///etc/passwd");
    assert_unavailable(&db, "https://other.example/data.csv");
    let rows = csv(&db, "https://data.neo4j.com/bands/artists.csv").unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0][0],
        Value::List(
            vec![
                Value::String("1".into()),
                Value::String("ABBA".into()),
                Value::String("1992".into())
            ]
            .into()
        )
    );
}

#[test]
fn opt_in_is_independent_of_database_uri_and_scopes_regular_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("csv");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("names.csv"), "name,year\nAda,2026\nLin,2025\n").unwrap();
    let outside = dir.path().join("outside.csv");
    fs::write(&outside, "private\n").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("names.csv"), root.join("inside.csv")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape.csv")).unwrap();
    }

    for uri in [
        ":memory:".to_owned(),
        dir.path().join("bundle.cl").to_string_lossy().into_owned(),
        dir.path().join("database").to_string_lossy().into_owned(),
    ] {
        let db = Database::open_with(&uri, options(&root, None)).unwrap();
        assert_eq!(csv(&db, "names.csv").unwrap().len(), 3);
        let url = local_file_url(&root.join("names.csv"));
        assert_eq!(csv(&db, &url).unwrap().len(), 3);
        #[cfg(unix)]
        assert_eq!(csv(&db, "inside.csv").unwrap().len(), 3);
        for source in [
            "../outside.csv",
            "names.csv/../names.csv",
            outside.to_str().unwrap(),
            "file://localhost/tmp/outside.csv",
            "http://example.org/test.csv",
            "file:../outside.csv",
            "file:////dev/zero",
            "file:///etc/passwd",
            "/dev/zero",
            "csv",
            "missing.csv",
        ] {
            assert_unavailable(&db, source);
        }
        assert_unavailable(&db, &local_file_url(&outside));
        assert_unavailable(&db, &format!("{}/../outside.csv", local_file_url(&root)));
        assert_unavailable(&db, &format!("{}/../csv/names.csv", local_file_url(&root)));
        assert_unavailable(
            &db,
            &format!("file://host/{}", url.strip_prefix("file:///").unwrap()),
        );
        assert_unavailable(
            &db,
            &format!("file:////{}", url.strip_prefix("file:///").unwrap()),
        );
        assert_unavailable(&db, &url.replace("names.csv", "names%2ecsv"));
        #[cfg(windows)]
        for source in [
            format!("file:///{}", root.join("names.csv").display()),
            format!("file://{}", root.join("names.csv").display()),
            url.replace("names.csv", "names.csv:stream"),
            r"file:///\\server\share\names.csv".into(),
            r"file:///\\?\C:\names.csv".into(),
            r"file:///\\.\C:\names.csv".into(),
            r"file://C:\names.csv".into(),
            "file:///C:names.csv".into(),
            "C:names.csv".into(),
            r"\names.csv".into(),
        ] {
            assert_unavailable(&db, &source);
        }
        #[cfg(unix)]
        assert_unavailable(&db, "escape.csv");
    }
}

#[test]
fn rejects_bad_config_and_bounds_actual_bytes() {
    let dir = tempfile::tempdir().unwrap();
    for limit in [Some(0), Some(67_108_865), Some(u64::MAX)] {
        let error = Database::open_with(":memory:", options(dir.path(), limit))
            .err()
            .unwrap();
        assert_eq!(error, Error::semantic("LOAD CSV invalid configuration"));
    }
    let missing = dir.path().join("absent");
    assert_eq!(
        Database::open_with(":memory:", options(&missing, None))
            .err()
            .unwrap(),
        Error::semantic("LOAD CSV invalid configuration")
    );
    let file_root = dir.path().join("not-a-dir");
    fs::write(&file_root, "x").unwrap();
    assert_eq!(
        Database::open_with(":memory:", options(&file_root, None))
            .err()
            .unwrap(),
        Error::semantic("LOAD CSV invalid configuration")
    );
    let db = Database::open_with(":memory:", options(dir.path(), Some(5))).unwrap();
    fs::write(dir.path().join("exact.csv"), b"a,b\nc").unwrap();
    fs::write(dir.path().join("large.csv"), b"a,b\nc\n").unwrap();
    assert_eq!(csv(&db, "exact.csv").unwrap().len(), 2);
    assert_eq!(
        csv(&db, "large.csv").unwrap_err(),
        Error::execution("LOAD CSV file exceeds byte limit")
    );
    fs::write(dir.path().join("invalid.csv"), [0xff]).unwrap();
    assert_unavailable(&db, "invalid.csv");
    // A limit without a root does not grant access.
    let no_root = Database::open_with(
        ":memory:",
        OpenOptions {
            load_csv_max_bytes: Some(5),
            mode: cypherlite::OpenMode::Writable,
            ..OpenOptions::default()
        },
    )
    .unwrap();
    assert_unavailable(&no_root, "exact.csv");

    fs::write(
        dir.path().join("default-too-large.csv"),
        vec![b'x'; 8_388_609],
    )
    .unwrap();
    let default_cap = Database::open_with(":memory:", options(dir.path(), None)).unwrap();
    assert_eq!(
        csv(&default_cap, "default-too-large.csv").unwrap_err(),
        Error::execution("LOAD CSV file exceeds byte limit")
    );
}

#[test]
fn invalid_csv_options_do_not_create_persistent_storage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("csv");
    fs::create_dir(&root).unwrap();
    for (name, configured_root, limit) in [
        ("bad-limit", root.clone(), Some(0)),
        ("bad-root.cl", root.join("missing"), None),
        ("bad-ceiling", root.clone(), Some(u64::MAX)),
    ] {
        let uri = dir.path().join(name);
        let error = Database::open_with(uri.to_str().unwrap(), options(&configured_root, limit))
            .err()
            .unwrap();
        assert_eq!(error, Error::semantic("LOAD CSV invalid configuration"));
        assert!(!uri.exists(), "invalid CSV options created {name}");
        assert!(!uri.with_extension("wal").exists());
    }
}

#[cfg(unix)]
#[test]
fn special_files_are_rejected_without_reading() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::open_with(":memory:", options(dir.path(), Some(5))).unwrap();
    assert_unavailable(&db, "file:///dev/zero");
    let fifo = dir.path().join("pipe.csv");
    assert!(std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    assert_unavailable(&db, "pipe.csv");
}

#[test]
fn repeated_input_rows_and_nested_paths_use_same_capability() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("rows.csv"), "a\nb\n").unwrap();
    let db = Database::open_with(":memory:", options(dir.path(), None)).unwrap();
    let mut conn = db.connect().unwrap();
    let result = conn
        .query("UNWIND [1,2,3] AS n LOAD CSV FROM 'rows.csv' AS row RETURN n, row")
        .unwrap();
    assert_eq!(result.rows.len(), 6);
    assert_eq!(
        result
            .rows
            .iter()
            .filter(|r| r[1] == Value::List(vec![Value::String("a".into())].into()))
            .count(),
        3
    );

    let nested = "CALL { LOAD CSV FROM 'rows.csv' AS row RETURN row } RETURN row";
    assert_eq!(conn.query(nested).unwrap().rows.len(), 2);
    let next = "LOAD CSV FROM 'rows.csv' AS row RETURN row NEXT LOAD CSV FROM 'rows.csv' AS second RETURN second";
    assert_eq!(conn.query(next).unwrap().rows.len(), 4);
    let write = "LOAD CSV FROM 'rows.csv' AS row CREATE (:Imported {name: row[0]})";
    conn.query(write).unwrap();
    assert_eq!(
        conn.query("MATCH (n:Imported) RETURN count(n)")
            .unwrap()
            .rows[0][0],
        Value::Int(2)
    );
    conn.begin().unwrap();
    assert_eq!(conn.query(nested).unwrap().rows.len(), 2);
    conn.rollback().unwrap();
    let denied = Database::open(":memory:").unwrap();
    for query in [nested, write, next] {
        let error = denied.connect().unwrap().query(query).unwrap_err();
        assert_eq!(
            error,
            Error::execution("LOAD CSV file unavailable"),
            "{query}: {error}"
        );
    }
    for query in [
        "RETURN COUNT { LOAD CSV FROM 'rows.csv' AS row RETURN row } AS n",
        "RETURN COLLECT { LOAD CSV FROM 'rows.csv' AS row RETURN row } AS rows",
        "RETURN EXISTS { LOAD CSV FROM 'rows.csv' AS row RETURN row } AS yes",
    ] {
        let error = db.connect().unwrap().query(query).unwrap_err();
        assert!(matches!(error, Error::Syntax(_)), "{query}: {error}");
    }
}
