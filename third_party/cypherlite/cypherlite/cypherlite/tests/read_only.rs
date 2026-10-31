//! Frozen read-only open (ADR-0136).

use std::fs;
use std::path::Path;

use cypherlite::test_opts::{count, read_only, writable};
use cypherlite::{Database, Error, OpenOptions, SnapshotCodecKind};
use cypherlite_storage::reset_uri;
use cypherlite_test_support::{uri, walk_tree};

fn tree(root: &Path) -> Vec<String> {
    walk_tree(root, |_, rel| rel.to_owned())
}

#[test]
fn writer_reopens_while_readers_stay_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let writer = Database::open_with(uri(&path), writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    drop(writer);

    let reader = Database::open_with(uri(&path), read_only()).unwrap();
    let follower = Database::open_with(
        uri(&path),
        OpenOptions {
            mode: cypherlite::OpenMode::Follow,
            ..read_only()
        },
    )
    .unwrap();
    let writer = Database::open_with(uri(&path), writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    assert!(matches!(
        Database::open_with(uri(&path), writable()),
        Err(Error::Locked(_))
    ));
    drop(writer);
    let writer = Database::open_with(uri(&path), writable()).unwrap();
    assert_eq!(count(&writer, "Seed"), 2);
    assert_eq!(count(&reader, "Seed"), 1);
    drop((follower, reader, writer));
}

#[test]
fn frozen_reader_shares_a_native_directory_and_does_not_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let writer = Database::open_with(uri(&path), writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();

    let reader = Database::open_with(uri(&path), read_only()).unwrap();
    let second = Database::open_with(uri(&path), read_only()).unwrap();
    assert_eq!(count(&reader, "Seed"), 1);
    assert_eq!(count(&second, "Seed"), 1);
    assert!(matches!(
        Database::open_with(uri(&path), writable()),
        Err(Error::Locked(_))
    ));

    writer
        .connect()
        .unwrap()
        .execute("CREATE (:Later)")
        .unwrap();
    assert_eq!(count(&reader, "Later"), 0);
    let before = tree(&path);
    assert!(matches!(reader.checkpoint(), Err(Error::Semantic(_))));
    assert!(matches!(
        reader.connect().unwrap().execute("CREATE (:Nope)"),
        Err(Error::Semantic(_))
    ));
    drop(reader);
    drop(second);
    assert_eq!(tree(&path), before);

    let refreshed = Database::open_with(uri(&path), read_only()).unwrap();
    assert_eq!(count(&refreshed, "Later"), 1);
}

#[test]
fn reset_while_a_reader_is_open_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let writer = Database::open_with(uri(&path), writable()).unwrap();
    writer.connect().unwrap().execute("CREATE ()").unwrap();
    writer.checkpoint().unwrap();
    let reader = Database::open_with(uri(&path), read_only()).unwrap();
    drop(writer);
    assert!(matches!(reset_uri(uri(&path), None), Err(Error::Locked(_))));
    drop(reader);
    reset_uri(uri(&path), None).unwrap();
}

#[test]
fn read_only_refuses_a_missing_path_without_creating_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing");
    match Database::open_with(uri(&path), read_only()) {
        Err(Error::Storage(_)) => {}
        Err(err) => panic!("expected storage error, got {err}"),
        Ok(_) => panic!("missing path opened"),
    }
    assert!(!path.exists());
}

#[test]
fn read_only_does_not_trim_a_torn_wal_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let writer = Database::open_with(uri(&path), writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    drop(writer);

    let mut logs: Vec<_> = fs::read_dir(path.join("wal"))
        .unwrap()
        .map(|ent| ent.unwrap().path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("log"))
        .collect();
    logs.sort();
    let segment = logs.pop().unwrap();
    let original = fs::read(&segment).unwrap();
    let mut torn = original.clone();
    torn.extend_from_slice(&[1, 2, 3, 4]);
    fs::write(&segment, &torn).unwrap();

    let reader = Database::open_with(uri(&path), read_only()).unwrap();
    assert_eq!(count(&reader, "Seed"), 1);
    drop(reader);
    assert_eq!(fs::read(&segment).unwrap(), torn);

    let healed = Database::open_with(uri(&path), writable()).unwrap();
    assert_eq!(count(&healed, "Seed"), 1);
    drop(healed);
    let after = fs::read(&segment).unwrap();
    assert_eq!(after, original);
}

#[test]
fn read_only_json_legacy_encoding_is_not_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let writer = Database::open_with(
        uri(&path),
        OpenOptions {
            snapshot_codec: Some(SnapshotCodecKind::Json),
            ..writable()
        },
    )
    .unwrap();
    writer
        .connect()
        .unwrap()
        .execute("CREATE (:Seed {name: 'ada'})")
        .unwrap();
    writer.checkpoint().unwrap();
    drop(writer);

    let graph_path = path.join("graph.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&graph_path).unwrap()).unwrap();
    let obj = value.as_object_mut().unwrap();
    assert!(obj.remove("value_encoding").is_some());
    let legacy = serde_json::to_vec_pretty(&value).unwrap();
    fs::write(&graph_path, &legacy).unwrap();

    let reader = Database::open_with(uri(&path), read_only()).unwrap();
    assert_eq!(count(&reader, "Seed"), 1);
    drop(reader);
    assert_eq!(fs::read(&graph_path).unwrap(), legacy);
}

#[test]
fn read_only_bundle_requires_the_writer_to_be_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    let writer = Database::open(uri(&path)).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    assert!(matches!(
        Database::open_with(uri(&path), read_only()),
        Err(Error::Locked(_))
    ));
    drop(writer);
    let reader = Database::open_with(uri(&path), read_only()).unwrap();
    assert_eq!(count(&reader, "Seed"), 1);
    assert!(matches!(
        reader.connect().unwrap().execute("CREATE ()"),
        Err(Error::Semantic(_))
    ));
}

#[test]
fn memory_read_only_rejects_writes() {
    let db = Database::open_with(":memory:", read_only()).unwrap();
    let err = db.connect().unwrap().execute("CREATE ()").unwrap_err();
    assert!(matches!(err, Error::Semantic(_)), "{err}");
}
