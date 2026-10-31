use super::*;
use crate::tests::person_ada;
use crate::{checkpoint_with_lsn, init_empty_wal, parse_uri, parse_uri_with_options};

/// A Parquet store checkpointed at LSN 1 with one node, its writer still
/// alive and holding the store's locks.
#[cfg(parquet_codec)]
fn parquet_store(dir: &tempfile::TempDir, name: &str) -> (std::path::PathBuf, StoreTarget) {
    let db = dir.path().join(name);
    let target = parse_uri(db.to_str().unwrap()).unwrap();
    let (g, wal) = person_ada();
    checkpoint_with_lsn(&target, &g, wal.lsn, None).unwrap();
    init_empty_wal(&target, wal.lsn).unwrap();
    (db, target)
}

/// Platform split (ADR-0136/ADR-0165): Unix locks are advisory, so the
/// lock-free peek reads beside a live writer. The writer's Windows
/// `LockFileEx` lock spans the whole host file and is mandatory against
/// every other handle, so the peek reports the typed `Error::Locked` a
/// locking open would return. On Windows this asserts that typed error and
/// returns true so the caller skips the Unix-side assertions; off Windows
/// it returns false.
fn windows_locked_beside_writer(result: &Result<StoreInfo>) -> bool {
    if cfg!(windows) {
        match result {
            Err(Error::Locked(_)) => return true,
            other => panic!("expected `Error::Locked` beside the live writer, got {other:?}"),
        }
    }
    false
}

#[cfg(parquet_codec)]
#[test]
fn native_dir_info_beside_live_writer() {
    let dir = tempfile::tempdir().unwrap();
    // `_writer` keeps the exclusive store locks held through the cold read.
    let (db, _writer) = parquet_store(&dir, "db");
    let info = store_info(&db).unwrap();
    assert_eq!(info.layout, StoreLayout::NativeDir);
    let StoreSnapshot::Parquet(m) = &info.snapshot else {
        panic!("expected a Parquet manifest, got {:?}", info.snapshot);
    };
    assert_eq!(m.format_version, 3);
    assert_eq!(m.checkpoint_lsn, 1);
    assert_eq!(m.node_parts.iter().map(|p| p.rows).sum::<u64>(), 1);
    assert_eq!(info.wal_segments, 1);
    assert!(info.total_bytes > 0);
}

#[cfg(parquet_codec)]
#[test]
fn bundle_info_beside_live_writer() {
    let dir = tempfile::tempdir().unwrap();
    // The writable opener holds LOCK_EX; a shared-lock open would fail here.
    let (db, _writer) = parquet_store(&dir, "graph.cl");
    let result = store_info(&db);
    if windows_locked_beside_writer(&result) {
        return;
    }
    let info = result.unwrap();
    assert_eq!(
        info.layout,
        StoreLayout::Bundle {
            superblock_version: 1
        }
    );
    let StoreSnapshot::Parquet(m) = &info.snapshot else {
        panic!("expected a Parquet manifest, got {:?}", info.snapshot);
    };
    assert_eq!(m.format_version, 3);
    assert_eq!(info.wal_segments, 1);
    assert_eq!(info.total_bytes, std::fs::metadata(&db).unwrap().len());
}

#[test]
fn json_codec_store_is_envelope_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    let target =
        parse_uri_with_options(db.to_str().unwrap(), None, Some(SnapshotCodecKind::Json)).unwrap();
    let (g, wal) = person_ada();
    checkpoint_with_lsn(&target, &g, wal.lsn, None).unwrap();
    let info = store_info(&db).unwrap();
    assert_eq!(info.layout, StoreLayout::NativeDir);
    let StoreSnapshot::Json {
        checkpoint_lsn,
        commit_ts,
    } = &info.snapshot
    else {
        panic!("expected a JSON envelope, got {:?}", info.snapshot);
    };
    assert_eq!(*checkpoint_lsn, 1);
    assert_eq!(*commit_ts, g.commit_ts());
}

#[test]
fn missing_database_is_a_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    for path in [dir.path().join("nope"), dir.path().join("nope.cl")] {
        let err = store_info(&path).unwrap_err();
        assert!(
            matches!(&err, Error::Storage(m) if m.contains("database not found")),
            "{err}"
        );
    }
}

#[test]
fn missing_manifest_is_snapshot_none_not_an_error() {
    // A fresh store before its first checkpoint has no manifest.json yet;
    // that is a typed state (StoreSnapshot::None), never an error.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    std::fs::create_dir(&db).unwrap();
    let info = store_info(&db).unwrap();
    assert_eq!(info.layout, StoreLayout::NativeDir);
    assert!(
        matches!(info.snapshot, StoreSnapshot::None),
        "{:?}",
        info.snapshot
    );
    assert_eq!(info.wal_segments, 0);
}

#[test]
fn fresh_bundle_is_snapshot_none_not_an_error() {
    // Same fresh-store state through the bundle path: open_or_create writes
    // the superblocks, but no checkpoint means no manifest.json inside.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("fresh.cl");
    let _writer = parse_uri(db.to_str().unwrap()).unwrap();
    let result = store_info(&db);
    if windows_locked_beside_writer(&result) {
        return;
    }
    let info = result.unwrap();
    assert_eq!(
        info.layout,
        StoreLayout::Bundle {
            superblock_version: 1
        }
    );
    assert!(
        matches!(info.snapshot, StoreSnapshot::None),
        "{:?}",
        info.snapshot
    );
    assert_eq!(info.wal_segments, 0);
    assert_eq!(info.total_bytes, std::fs::metadata(&db).unwrap().len());
}

/// Security-review stress: race the lock-free cold read against an actively
/// checkpointing writer. Every outcome must be `Ok` or a typed
/// `Error::Storage`/`Error::Io`/`Error::Locked` — never a panic, never
/// garbage. (`Error::Locked` is the Windows outcome per the platform-split
/// rationale in `windows_locked_beside_writer`; here it is one of several
/// typed outcomes under the race, not an either/or split.)
#[test]
fn store_info_beside_a_checkpointing_writer_never_panics() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc as StdArc;
    for name in ["db", "graph.cl"] {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join(name);
        let target = parse_uri(db.to_str().unwrap()).unwrap();
        let (g, wal) = person_ada();
        checkpoint_with_lsn(&target, &g, wal.lsn, None).unwrap();
        init_empty_wal(&target, wal.lsn).unwrap();
        let stop = StdArc::new(AtomicBool::new(false));
        let writer = std::thread::spawn(move || {
            for lsn in 2..40u64 {
                checkpoint_with_lsn(&target, &g, lsn, None).expect("checkpoint");
            }
        });
        let reader = {
            let db = db.clone();
            let stop = StdArc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    match store_info(&db) {
                        Ok(info) => assert!(info.total_bytes > 0, "garbage read: {info:?}"),
                        Err(Error::Storage(_) | Error::Io(_) | Error::Locked(_)) => {}
                        Err(other) => panic!("store_info returned an unexpected error: {other}"),
                    }
                }
            })
        };
        let writer_result = writer.join();
        stop.store(true, Ordering::Relaxed);
        let reader_result = reader.join();
        writer_result.unwrap();
        reader_result.unwrap();
    }
}

#[test]
fn corrupt_manifest_is_a_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    std::fs::create_dir(&db).unwrap();
    std::fs::write(db.join("manifest.json"), b"not json").unwrap();
    let err = store_info(&db).unwrap_err();
    assert!(matches!(&err, Error::Storage(_)), "{err}");
}

#[test]
fn non_bundle_cl_file_is_a_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("bad.cl");
    std::fs::write(&db, b"not a bundle at all").unwrap();
    let err = store_info(&db).unwrap_err();
    assert!(
        matches!(&err, Error::Storage(m) if m.contains("not a CypherLite .cl bundle")),
        "{err}"
    );
}

#[cfg(parquet_codec)]
#[test]
fn unreadable_format_version_is_a_typed_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db");
    std::fs::create_dir(&db).unwrap();
    std::fs::write(
        db.join("manifest.json"),
        br#"{"format_version":9999,"next_node_id":0,"next_rel_id":0}"#,
    )
    .unwrap();
    let err = store_info(&db).unwrap_err();
    assert!(
        matches!(&err, Error::Storage(m) if m.contains("unsupported snapshot version 9999")),
        "{err}"
    );
}

#[test]
fn store_info_uri_accepts_file_scheme_and_bare_paths() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["db", "graph.cl"] {
        let db = dir.path().join(name);
        // `writer` keeps the writer — and its mandatory Windows whole-file
        // lock — alive through the peeks below; the native-dir arm holds no
        // writer, so only `Some(..)` can see the Windows `Error::Locked`.
        let writer = if name == "db" {
            std::fs::create_dir(&db).unwrap();
            None
        } else {
            // A fresh bundle: open_or_create writes the superblocks.
            Some(parse_uri(db.to_str().unwrap()).unwrap())
        };
        let bare = store_info_uri(db.to_str().unwrap());
        let file_scheme = store_info_uri(&format!("file://{}", db.display()));
        if writer.is_some() && windows_locked_beside_writer(&bare) {
            // Both spellings peek the same locked bundle.
            assert!(
                windows_locked_beside_writer(&file_scheme),
                "{bare:?} vs {file_scheme:?}"
            );
        } else {
            let bare = bare.unwrap();
            let file_scheme = file_scheme.unwrap();
            assert_eq!(bare.layout, file_scheme.layout);
            assert_eq!(bare.total_bytes, file_scheme.total_bytes);
            assert_eq!(bare.wal_segments, file_scheme.wal_segments);
        }
    }
}

#[test]
fn store_info_uri_memory_is_a_typed_error() {
    // A :memory: store has no on-disk layout to describe; that is a typed
    // error, not a panic or an empty answer.
    let err = store_info_uri(":memory:").unwrap_err();
    assert!(
        matches!(&err, Error::Storage(m) if m.contains(":memory:")),
        "{err}"
    );
}

#[test]
fn store_info_uri_rejects_remote_schemes() {
    for uri in ["s3://bucket/db", "https://example.com/db"] {
        let err = store_info_uri(uri).unwrap_err();
        assert!(
            matches!(&err, Error::Storage(m) if m.contains("URI scheme") || m.contains("not an engine URI")),
            "{uri}: {err}"
        );
    }
}
