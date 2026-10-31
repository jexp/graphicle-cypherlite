//! Clone across the full dir/`.cl` matrix (ADR-0156, ADR-0282).

use std::fs;
use std::path::{Path, PathBuf};

use cypherlite::test_opts::{count, read_only, set_fail_after_dest, writable};
use cypherlite::{CloneOptions, Database, Error, OpenOptions, SnapshotCodecKind, Value};
use cypherlite_storage::parse_uri_opening;
use cypherlite_test_support::{uri, walk_tree, ClearOnDrop};

fn seed_source(dir: &Path) -> PathBuf {
    let src = dir.join("src");
    let writer = Database::open_with(uri(&src), writable()).unwrap();
    writer.connect().unwrap().execute("CREATE ()").unwrap();
    drop(writer);
    src
}

/// Seed a directory source and clone it into a `.cl` bundle; returns
/// `(src, bundle)`.
fn seeded_bundle(dir: &Path) -> (PathBuf, PathBuf) {
    let src = seed_source(dir);
    let bundle = dir.join("backup.cl");
    Database::clone(uri(&src), uri(&bundle), CloneOptions::default()).unwrap();
    (src, bundle)
}

/// A fail-injected clone of `src` into `dst` fails after creating the
/// destination and removes what it created (ADR-0161).
fn assert_failed_clone_cleans_up(src: &Path, dst: &Path) {
    set_fail_after_dest(true);
    let err = Database::clone(uri(src), uri(dst), CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("clone failed after creating the destination")),
        "{err}"
    );
    assert!(!dst.exists(), "failed clone left {dst:?}");
}

fn files(root: &Path) -> Vec<(String, u64)> {
    walk_tree(root, |path, rel| {
        (
            rel.to_owned(),
            std::fs::symlink_metadata(path).unwrap().len(),
        )
    })
}

/// Relative path -> content bytes for every file under `root`, sorted.
fn tree_bytes(root: &Path) -> Vec<(String, Vec<u8>)> {
    walk_tree(root, |path, rel| (rel.to_owned(), fs::read(path).unwrap()))
}

/// The snapshot codec an existing store records, observed on a read-only open.
fn store_codec(path: &Path) -> SnapshotCodecKind {
    parse_uri_opening(uri(path), None, None, true)
        .unwrap()
        .snapshot_codec()
}

fn person_names(db: &Database) -> Vec<Vec<Value>> {
    db.connect()
        .unwrap()
        .query("MATCH (n:Person) RETURN n.name ORDER BY n.name")
        .unwrap()
        .rows
}

#[test]
fn clone_beside_a_writer_is_a_frozen_cut() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    let writer = Database::open_with(uri(&src), writable()).unwrap();
    writer
        .connect()
        .unwrap()
        .execute("CREATE (:Seed {name: 'ada'})")
        .unwrap();
    let before = files(&src);

    Database::clone(uri(&src), uri(&dst), CloneOptions::default()).unwrap();
    assert_eq!(files(&src), before, "clone wrote the source");

    writer
        .connect()
        .unwrap()
        .execute("CREATE (:Later)")
        .unwrap();

    let cloned = Database::open_with(uri(&dst), read_only()).unwrap();
    assert_eq!(count(&cloned, "Seed"), 1);
    assert_eq!(count(&cloned, "Later"), 0);
    drop(cloned);

    let cloned = Database::open_with(uri(&dst), writable()).unwrap();
    cloned
        .connect()
        .unwrap()
        .execute("CREATE (:Clone)")
        .unwrap();
    assert_eq!(count(&cloned, "Clone"), 1);
    assert_eq!(count(&writer, "Clone"), 0);
    assert!(dst.join("wal").join("00000001.log").is_file());
    assert!(!dst.join("cdc").join("slots.json").exists());
}

#[test]
fn clone_reads_an_external_wal_and_writes_dest_wal() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let wal = dir.path().join("external-wal");
    let dst = dir.path().join("dst");
    let opts = OpenOptions {
        wal_dir: Some(wal.clone()),
        ..writable()
    };
    let writer = Database::open_with(uri(&src), opts).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    let source_files = files(&src);
    let wal_files = files(&wal);

    Database::clone(
        uri(&src),
        uri(&dst),
        CloneOptions {
            wal_dir: Some(wal.clone()),
            ..CloneOptions::default()
        },
    )
    .unwrap();

    assert_eq!(files(&src), source_files);
    assert_eq!(files(&wal), wal_files);
    assert!(dst.join("wal").join("00000001.log").is_file());
    assert_ne!(dst.join("wal"), wal);
    let cloned = Database::open_with(uri(&dst), read_only()).unwrap();
    assert_eq!(count(&cloned, "Seed"), 1);
}

#[test]
fn clone_rejects_memory_and_existing_destinations() {
    let dir = tempfile::tempdir().unwrap();
    let src = seed_source(dir.path());

    let out = dir.path().join("out");
    let err = Database::clone(":memory:", uri(&out), CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("native directory or .cl bundle")),
        "{err}"
    );
    assert!(!out.exists());

    let err = Database::clone(uri(&src), ":memory:", CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("native directory or .cl bundle")),
        "{err}"
    );

    std::fs::create_dir(&out).unwrap();
    let err = Database::clone(uri(&src), uri(&out), CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("already exists")),
        "{err}"
    );

    let err = Database::clone(uri(&src), uri(&src), CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("source database")),
        "{err}"
    );
}

#[test]
fn clone_refuses_a_destination_under_a_missing_parent_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let src = seed_source(dir.path());

    // The fail-closed pre-check cannot canonicalize a missing ancestor;
    // the refusal names the missing parent instead of relaying a bare OS
    // "No such file or directory".
    let dst = dir.path().join("missing").join("nested").join("dst");
    let err = Database::clone(uri(&src), uri(&dst), CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("clone destination parent does not exist")),
        "{err}"
    );
    assert!(!dst.exists());
}

#[test]
fn failed_clone_removes_the_destination_it_created() {
    let _clear = ClearOnDrop::new(|| set_fail_after_dest(false));
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    let writer = Database::open_with(uri(&src), writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    let before = files(&src);

    assert_failed_clone_cleans_up(&src, &dst);
    assert_eq!(files(&src), before);
}

#[test]
fn dir_to_bundle_to_dir_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let bundle = dir.path().join("backup.cl");
    let restored = dir.path().join("restored");
    let writer = Database::open_with(uri(&src), writable()).unwrap();
    writer
        .connect()
        .unwrap()
        .execute("CREATE (a:Person {name: 'ada'})-[:KNOWS]->(b:Person {name: 'bob'})")
        .unwrap();

    Database::clone(uri(&src), uri(&bundle), CloneOptions::default()).unwrap();
    assert!(bundle.is_file());

    // Capture the cut before the extra commit: the clone is frozen and
    // later commits are not followed.
    let expected = person_names(&writer);
    writer
        .connect()
        .unwrap()
        .execute("CREATE (:Person {name: 'cy'})")
        .unwrap();

    Database::clone(uri(&bundle), uri(&restored), CloneOptions::default()).unwrap();
    assert!(restored.is_dir());
    assert!(restored.join("wal").join("00000001.log").is_file());

    let db = Database::open_with(uri(&restored), read_only()).unwrap();
    assert_eq!(person_names(&db), expected);
    let rows = db
        .connect()
        .unwrap()
        .query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name")
        .unwrap()
        .rows;
    assert_eq!(
        rows,
        vec![vec![
            Value::String("ada".into()),
            Value::String("bob".into())
        ]]
    );
    drop(db);

    // The restored directory is a writable database of its own.
    let db = Database::open_with(uri(&restored), writable()).unwrap();
    db.connect()
        .unwrap()
        .execute("CREATE (:Person {name: 'dot'})")
        .unwrap();
    assert_eq!(count(&db, "Person"), 3);
    assert_eq!(count(&writer, "Person"), 3);
}

#[test]
fn bundle_to_bundle_copy() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let bundle = dir.path().join("backup.cl");
    let copy = dir.path().join("copy.cl");
    let writer = Database::open_with(uri(&src), writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    drop(writer);

    Database::clone(uri(&src), uri(&bundle), CloneOptions::default()).unwrap();
    Database::clone(uri(&bundle), uri(&copy), CloneOptions::default()).unwrap();
    assert!(copy.is_file());
    let db = Database::open_with(uri(&copy), read_only()).unwrap();
    assert_eq!(count(&db, "Seed"), 1);
}

/// dir -> `.cl` -> dir keeps each hop's codec and the data (ADR-0282,
/// ADR-0163). The source is opened with `open_codec` (`None` is the build
/// default); each clone hop takes its `to_snapshot_format` override, and
/// each destination must record that hop's override or, absent one, the
/// previous hop's codec.
fn assert_clone_round_trip(
    open_codec: Option<SnapshotCodecKind>,
    to_bundle: Option<SnapshotCodecKind>,
    from_bundle: Option<SnapshotCodecKind>,
) {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let bundle = dir.path().join("backup.cl");
    let restored = dir.path().join("restored");
    let source_codec = open_codec.unwrap_or_default();
    let bundle_codec = to_bundle.unwrap_or(source_codec);
    let restored_codec = from_bundle.unwrap_or(bundle_codec);
    let writer = Database::open_with(
        uri(&src),
        OpenOptions {
            snapshot_codec: open_codec,
            ..writable()
        },
    )
    .unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    assert_eq!(store_codec(&src), source_codec);
    drop(writer);

    Database::clone(
        uri(&src),
        uri(&bundle),
        CloneOptions {
            to_snapshot_format: to_bundle,
            ..CloneOptions::default()
        },
    )
    .unwrap();
    assert_eq!(store_codec(&bundle), bundle_codec);

    Database::clone(
        uri(&bundle),
        uri(&restored),
        CloneOptions {
            to_snapshot_format: from_bundle,
            ..CloneOptions::default()
        },
    )
    .unwrap();
    assert_eq!(store_codec(&restored), restored_codec);
    let db = Database::open_with(uri(&restored), read_only()).unwrap();
    assert_eq!(count(&db, "Seed"), 1);
}

#[test]
fn clone_preserves_the_json_codec_both_ways() {
    assert_clone_round_trip(Some(SnapshotCodecKind::Json), None, None);
}

#[cfg(feature = "parquet")]
#[test]
fn clone_preserves_the_default_parquet_codec_both_ways() {
    assert_clone_round_trip(None, None, None);
}

#[cfg(feature = "parquet")]
#[test]
fn clone_transcodes_json_to_parquet_and_back() {
    assert_clone_round_trip(
        Some(SnapshotCodecKind::Json),
        Some(SnapshotCodecKind::Parquet),
        Some(SnapshotCodecKind::Json),
    );
}

#[cfg(feature = "parquet")]
#[test]
fn clone_transcodes_parquet_to_json_and_back() {
    assert_clone_round_trip(
        Some(SnapshotCodecKind::Parquet),
        Some(SnapshotCodecKind::Json),
        Some(SnapshotCodecKind::Parquet),
    );
}

/// A requested codec this build does not include is `Error::Storage` from
/// the storage layer's availability check, raised before the destination is
/// created (ADR-0163 §3) — no partial destination is left behind.
#[cfg(not(feature = "parquet"))]
#[test]
fn an_uncompiled_codec_request_fails_before_the_destination_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let src = seed_source(dir.path());
    let dst = dir.path().join("dst");
    let err = Database::clone(
        uri(&src),
        uri(&dst),
        CloneOptions {
            to_snapshot_format: Some(SnapshotCodecKind::Parquet),
            ..CloneOptions::default()
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("not compiled")),
        "{err}"
    );
    assert!(!dst.exists(), "unavailable codec left {dst:?}");
}

#[test]
fn existing_destination_is_refused_for_both_dest_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let (src, bundle) = seeded_bundle(dir.path());

    // Existing .cl destination.
    let err = Database::clone(uri(&src), uri(&bundle), CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("clone destination already exists")),
        "{err}"
    );

    // Existing directory destination.
    let out = dir.path().join("out");
    fs::create_dir(&out).unwrap();
    let err = Database::clone(uri(&bundle), uri(&out), CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("clone destination already exists")),
        "{err}"
    );

    // A bundle cloned onto its own path is an existing destination.
    let err = Database::clone(uri(&bundle), uri(&bundle), CloneOptions::default()).unwrap_err();
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("clone destination already exists")),
        "{err}"
    );
}

#[test]
fn wal_dir_with_a_bundle_source_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_src, bundle) = seeded_bundle(dir.path());

    let out = dir.path().join("out");
    let err = Database::clone(
        uri(&bundle),
        uri(&out),
        CloneOptions {
            wal_dir: Some(dir.path().join("wal")),
            ..CloneOptions::default()
        },
    )
    .unwrap_err();
    // The canonical storage-layer check (ADR-0074), surfaced through the
    // read-only source open (ADR-0163 §4).
    assert!(
        matches!(err, Error::Storage(ref msg) if msg.contains("wal_dir is not supported for .cl")),
        "{err}"
    );
    assert!(!out.exists());
}

#[test]
fn writer_held_bundle_source_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let bundle = dir.path().join("graph.cl");
    let writer = Database::open(uri(&bundle)).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();

    let out = dir.path().join("out");
    let err = Database::clone(uri(&bundle), uri(&out), CloneOptions::default()).unwrap_err();
    assert!(matches!(err, Error::Locked(_)), "{err}");
    assert!(!out.exists());
}

#[test]
fn failed_clone_removes_a_bundle_destination_it_created() {
    let _clear = ClearOnDrop::new(|| set_fail_after_dest(false));
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let writer = Database::open_with(uri(&src), writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    let bundle = dir.path().join("backup.cl");
    let restored = dir.path().join("restored");

    // .cl destination: the partially created file is removed.
    assert_failed_clone_cleans_up(&src, &bundle);

    // Native directory destination from a bundle source: the partially
    // created directory is removed.
    Database::clone(uri(&src), uri(&bundle), CloneOptions::default()).unwrap();
    assert_failed_clone_cleans_up(&bundle, &restored);
}

#[test]
fn clone_does_not_touch_the_source_either_way() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let writer = Database::open_with(uri(&src), writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    drop(writer);
    // Read store files only while no Database handle is alive: on Windows
    // the engine's byte-range locks deny reads from other handles (error
    // 33). Clone opens and drops its own read-only handle internally, so
    // the source is unlocked again once it returns.
    let before = tree_bytes(&src);

    let bundle = dir.path().join("backup.cl");
    Database::clone(uri(&src), uri(&bundle), CloneOptions::default()).unwrap();
    assert_eq!(tree_bytes(&src), before, "clone wrote the source");

    let bundle_bytes = fs::read(&bundle).unwrap();
    let restored = dir.path().join("restored");
    Database::clone(uri(&bundle), uri(&restored), CloneOptions::default()).unwrap();
    assert_eq!(
        fs::read(&bundle).unwrap(),
        bundle_bytes,
        "clone wrote the source"
    );
}

#[test]
fn clone_from_a_directory_reads_an_external_wal_into_a_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let wal = dir.path().join("external-wal");
    let bundle = dir.path().join("backup.cl");
    let opts = OpenOptions {
        wal_dir: Some(wal.clone()),
        ..writable()
    };
    let writer = Database::open_with(uri(&src), opts).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    drop(writer);
    // Read store and WAL files only while no Database handle is alive: on
    // Windows the engine's byte-range locks deny reads from other handles
    // (error 33).
    let source_files = tree_bytes(&src);
    let wal_files = tree_bytes(&wal);

    Database::clone(
        uri(&src),
        uri(&bundle),
        CloneOptions {
            wal_dir: Some(wal.clone()),
            ..CloneOptions::default()
        },
    )
    .unwrap();

    assert_eq!(tree_bytes(&src), source_files);
    assert_eq!(tree_bytes(&wal), wal_files);
    let cloned = Database::open_with(uri(&bundle), read_only()).unwrap();
    assert_eq!(count(&cloned, "Seed"), 1);
}
