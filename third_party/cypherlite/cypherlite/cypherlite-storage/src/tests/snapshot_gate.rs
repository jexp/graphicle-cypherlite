//! ADR-0114 Decision 2: `open_store` loads and validates the snapshot before
//! `Wal::open` may repair a torn tail. A store written by a newer CypherLite
//! (unsupported Parquet `format_version`, JSON `value_encoding`, or read-only
//! marker) fails open with every WAL segment byte and the WAL listing
//! untouched, however often the open is retried.

use super::*;

fn node(id: i64) -> cypherlite_executor::RedoOp {
    cypherlite_executor::RedoOp::CreateNode {
        id,
        labels: vec!["Person".into()].into(),
        properties: Default::default(),
    }
}

/// A checkpointed store whose active WAL segment holds one post-checkpoint
/// commit plus an incomplete final frame (a torn tail that recovery would
/// otherwise trim). Exercises MemoryVfs, NativeVfs and BundleVfs targets
/// unchanged.
fn checkpointed_store_with_torn_tail(target: &StoreTarget) {
    let (vfs, prefix) = target.wal_vfs();
    let mut wal = Wal::open(Arc::clone(&vfs), prefix, 0).unwrap();
    let graph = Graph::default();
    wal.append_commit(1, &[node(0)]).unwrap();
    graph.apply_redo(vec![node(0)], 1).unwrap();
    checkpoint_with_lsn(target, &graph, wal.lsn, None).unwrap();
    wal.append_commit(2, &[node(1)]).unwrap();
    drop(wal);
    // Fewer bytes than a frame header: an incomplete frame at physical EOF.
    let segment = crate::vfs::virt_join(prefix, "00000001.log");
    let mut file = vfs.open(&segment, vfs::VfsOpen::append_create()).unwrap();
    file.write_all(&[0xDE, 0xAD, 0xBE]).unwrap();
    file.sync_all().unwrap();
}

fn assert_failed_open_preserves_wal(target: &StoreTarget, message: &str) {
    let (vfs, prefix) = target.wal_vfs();
    let before = wal_dir_state(&*vfs, prefix);
    assert!(
        before.iter().any(|(name, _)| name == "00000001.log"),
        "test store must have an active WAL segment"
    );
    for _ in 0..2 {
        let err = open_store(target).err().expect("gated open must fail");
        assert!(matches!(err, Error::Storage(_)), "{err}");
        assert!(err.to_string().contains(message), "{err}");
        assert_eq!(
            wal_dir_state(&*vfs, prefix),
            before,
            "WAL must stay byte-identical"
        );
    }
}

/// Read, edit, and write back a JSON snapshot object.
fn rewrite_json_object(
    target: &StoreTarget,
    name: &str,
    edit: impl FnOnce(&mut serde_json::Value),
) {
    let mut value: serde_json::Value =
        serde_json::from_slice(&snapshot::get_object(target, name).unwrap()).unwrap();
    edit(&mut value);
    snapshot::put_object(target, name, &serde_json::to_vec(&value).unwrap()).unwrap();
}

fn json_stores(dir: &std::path::Path) -> Vec<StoreTarget> {
    vec![
        memory_store_with_codec(SnapshotCodecKind::Json).unwrap(),
        parse_uri_with_options(
            dir.join("json-native").to_str().unwrap(),
            None,
            Some(SnapshotCodecKind::Json),
        )
        .unwrap(),
        parse_uri_with_options(
            dir.join("json-bundle.cl").to_str().unwrap(),
            None,
            Some(SnapshotCodecKind::Json),
        )
        .unwrap(),
    ]
}

#[test]
fn json_value_encoding_gate_runs_before_wal_recovery() {
    let dir = tempfile::tempdir().unwrap();
    for target in json_stores(dir.path()) {
        checkpointed_store_with_torn_tail(&target);
        rewrite_json_object(&target, "graph.json", |v| {
            v["value_encoding"] = serde_json::Value::from(4);
        });
        assert_failed_open_preserves_wal(&target, "unsupported graph value encoding: 4");
    }
}

#[test]
fn json_read_only_gate_runs_before_wal_recovery() {
    let dir = tempfile::tempdir().unwrap();
    for target in json_stores(dir.path()) {
        checkpointed_store_with_torn_tail(&target);
        rewrite_json_object(&target, "graph.json", |v| {
            v["read_only"] = serde_json::Value::from(true);
        });
        assert_failed_open_preserves_wal(&target, "read-only snapshot not supported");
    }
}

#[cfg(parquet_codec)]
fn parquet_stores(dir: &std::path::Path) -> Vec<StoreTarget> {
    vec![
        memory_store(),
        parse_uri(dir.join("parquet-native").to_str().unwrap()).unwrap(),
        parse_uri(dir.join("parquet-bundle.cl").to_str().unwrap()).unwrap(),
    ]
}

#[cfg(parquet_codec)]
fn rewrite_manifest_version(target: &StoreTarget, version: u32) {
    rewrite_json_object(target, "manifest.json", |value| {
        value["format_version"] = serde_json::Value::from(version);
    });
}

#[cfg(parquet_codec)]
#[test]
fn parquet_version_5_gate_runs_before_wal_recovery() {
    let dir = tempfile::tempdir().unwrap();
    for target in parquet_stores(dir.path()) {
        checkpointed_store_with_torn_tail(&target);
        rewrite_manifest_version(&target, 5);
        assert_failed_open_preserves_wal(&target, "unsupported snapshot version 5");
    }
}

#[cfg(parquet_codec)]
#[test]
fn parquet_version_99_gate_runs_before_wal_recovery() {
    let dir = tempfile::tempdir().unwrap();
    for target in parquet_stores(dir.path()) {
        checkpointed_store_with_torn_tail(&target);
        rewrite_manifest_version(&target, 99);
        assert_failed_open_preserves_wal(&target, "unsupported snapshot version 99");
    }
}

#[test]
fn unsupported_snapshot_without_wal_never_creates_wal_dir() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("graph.json"),
        br#"{"value_encoding":4,"nodes":[],"relationships":[],"next_node_id":0,"next_rel_id":0}"#,
    )
    .unwrap();
    let wal = root.join("wal");
    // A bare target without the ownership lease: parse_uri would create the
    // WAL directory itself, which is exactly what this test must not mask.
    let target = StoreTarget {
        vfs: Arc::new(NativeVfs::new(&root)),
        wal_vfs: Arc::new(NativeVfs::new(&wal)),
        wal_prefix: String::new(),
        native_root: Some(root.clone()),
        wal_native: Some(wal.clone()),
        codec: SnapshotCodecKind::Json,
    };
    let err = open_store(&target).err().expect("gated open must fail");
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert!(
        err.to_string()
            .contains("unsupported graph value encoding: 4"),
        "{err}"
    );
    assert!(!wal.exists(), "failed open must not create the WAL dir");
    assert!(!wal.join("00000001.log").exists());
}

#[test]
fn fresh_stores_open_and_write_wal_magic() {
    let memory = memory_store();
    open_store(&memory).unwrap();
    let segment = vfs::read_to_vec(&*memory.vfs, "wal/00000001.log").unwrap();
    assert!(segment.starts_with(MAGIC), ":memory: segment magic");

    let dir = tempfile::tempdir().unwrap();
    let native = parse_uri(dir.path().join("db").to_str().unwrap()).unwrap();
    open_store(&native).unwrap();
    let segment = std::fs::read(dir.path().join("db/wal/00000001.log")).unwrap();
    assert!(segment.starts_with(MAGIC), "native segment magic");

    let bundle_path = dir.path().join("fresh.cl");
    let bundle = parse_uri(bundle_path.to_str().unwrap()).unwrap();
    open_store(&bundle).unwrap();
    let segment = vfs::read_to_vec(&*bundle.vfs, "wal/00000001.log").unwrap();
    assert!(segment.starts_with(MAGIC), "bundle segment magic");
}
