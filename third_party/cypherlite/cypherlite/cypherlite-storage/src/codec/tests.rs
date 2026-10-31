use super::*;
use crate::Manifest;
use std::sync::Arc;

const LEGACY_MANIFEST: &str = r#"{"format_version":3,"next_node_id":0,"next_rel_id":0}"#;

#[test]
fn manifest_without_codec_decodes_as_parquet() {
    let manifest: Manifest = serde_json::from_str(LEGACY_MANIFEST).unwrap();
    assert_eq!(manifest.codec, SnapshotCodecKind::Parquet);
    let envelope: Envelope = serde_json::from_str(LEGACY_MANIFEST).unwrap();
    assert_eq!(envelope.codec, SnapshotCodecKind::Parquet);
}

#[test]
fn new_store_default_is_the_compiled_codec() {
    let expected = if cfg!(parquet_codec) {
        SnapshotCodecKind::Parquet
    } else {
        SnapshotCodecKind::Json
    };
    assert_eq!(SnapshotCodecKind::default(), expected);
    assert_eq!(crate::memory_store().snapshot_codec(), expected);
    let dir = tempfile::tempdir().unwrap();
    let target = crate::parse_uri(dir.path().join("db").to_str().unwrap()).unwrap();
    assert_eq!(target.snapshot_codec(), expected);
}

#[test]
fn cl_bundle_roundtrips_with_json_codec() {
    use cypherlite_executor::RedoOp;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("graph.cl");
    let uri = path.to_str().unwrap();
    let target = crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Json)).unwrap();
    assert_eq!(target.snapshot_codec(), SnapshotCodecKind::Json);
    let (graph, mut wal) = crate::open_store(&target).unwrap();
    let create = |id: i64, label: &str| RedoOp::CreateNode {
        id,
        labels: Arc::new(vec![label.into()]),
        properties: std::sync::Arc::new(cypherlite_core::PropertyMap::new()),
    };
    let first = vec![create(0, "Checkpointed")];
    wal.append_commit(1, &first).unwrap();
    graph.apply_redo(first, 1).unwrap();
    crate::checkpoint_with_lsn(&target, &graph, wal.lsn, None).unwrap();
    let cut = wal.rotate().unwrap();
    wal.recycle(cut).unwrap();
    wal.append_commit(2, &[create(1, "Replayed")]).unwrap();
    drop((wal, graph, target));

    assert!(path.is_file());
    let target = crate::parse_uri(uri).unwrap();
    assert_eq!(target.snapshot_codec(), SnapshotCodecKind::Json);
    assert!(snapshot::object_exists(&target, json::GRAPH_JSON).unwrap());
    let (loaded, reopened) = crate::open_store(&target).unwrap();
    assert_eq!(reopened.lsn, 2);
    assert_eq!(loaded.live_node_count(), 2);
    assert_eq!(loaded.node(0).unwrap().labels.as_slice(), ["Checkpointed"]);
    assert_eq!(loaded.node(1).unwrap().labels.as_slice(), ["Replayed"]);
}

#[cfg(not(parquet_codec))]
mod without_parquet {
    use super::*;

    const UNAVAILABLE: &str =
        "parquet snapshot codec is not compiled into this build (enable the \"parquet\" feature)";

    fn assert_unavailable(error: Error) {
        assert_eq!(error, Error::Storage(UNAVAILABLE.into()));
        assert_eq!(error, parquet_unavailable());
    }

    #[test]
    fn opening_a_parquet_directory_fails_closed() {
        let manifest_only = tempfile::tempdir().unwrap();
        std::fs::write(manifest_only.path().join("manifest.json"), LEGACY_MANIFEST).unwrap();
        let shards_only = tempfile::tempdir().unwrap();
        std::fs::create_dir(shards_only.path().join("nodes")).unwrap();
        let legacy_file = tempfile::tempdir().unwrap();
        std::fs::write(legacy_file.path().join("nodes.parquet"), b"PAR1").unwrap();
        for dir in [&shards_only, &legacy_file] {
            std::fs::write(dir.path().join(crate::LOCK_MARKER), crate::ROOT_ROLE).unwrap();
        }
        for dir in [&manifest_only, &shards_only, &legacy_file] {
            let error = crate::parse_uri(dir.path().to_str().unwrap())
                .err()
                .expect("parquet store must not open");
            assert_unavailable(error);
        }
    }

    #[test]
    fn opening_a_parquet_bundle_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.cl");
        let bundle = crate::BundleVfs::open_or_create(&path).unwrap();
        crate::atomic_write(&bundle, "manifest.json", LEGACY_MANIFEST.as_bytes()).unwrap();
        drop(bundle);
        let error = crate::parse_uri(path.to_str().unwrap())
            .err()
            .expect("parquet bundle must not open");
        assert_unavailable(error);
    }

    #[test]
    fn explicit_parquet_request_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        let bundle = dir.path().join("graph.cl");
        for uri in [":memory:", db.to_str().unwrap(), bundle.to_str().unwrap()] {
            let error = crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Parquet))
                .err()
                .expect("explicit parquet must not open");
            assert_unavailable(error);
        }
        assert!(!db.exists());
        assert!(!bundle.exists());
    }

    #[test]
    fn explicit_parquet_memory_store_fails_closed() {
        let error = crate::memory_store_with_codec(SnapshotCodecKind::Parquet)
            .err()
            .expect("parquet memory store must not open");
        assert_unavailable(error);
    }
}
