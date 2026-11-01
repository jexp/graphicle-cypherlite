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

#[test]
fn a_root_envelope_naming_the_markdown_codec_is_refused() {
    // The markdown codec's manifest home is the `.cypherlite/` sidecar
    // (ADR-0406 §3): a root manifest.json naming it is a never-written
    // state and fails closed like the mixed cases.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("manifest.json"), r#"{"codec":"markdown"}"#).unwrap();
    let error = crate::parse_uri(dir.path().to_str().unwrap())
        .err()
        .expect("a root envelope naming markdown must not open");
    assert!(
        error.to_string().contains("mixed snapshot codecs"),
        "{error}"
    );
}

#[test]
fn read_envelope_refuses_a_root_envelope_naming_markdown_itself() {
    // The refusal lives in the one typed envelope reader, so every
    // caller — codec resolution, the cold `store_info` read, the LSN
    // reader — inherits the same error; a classic envelope still reads
    // through, proving the refusal is typed, not a blanket parse
    // failure.
    let target = crate::memory_store();
    crate::atomic_write(&*target.vfs(), "manifest.json", br#"{"codec":"markdown"}"#).unwrap();
    let error =
        read_envelope(&target).expect_err("a root envelope naming markdown must not read as one");
    assert_eq!(error, root_envelope_names_markdown());
    crate::atomic_write(&*target.vfs(), "manifest.json", br#"{"codec":"json"}"#).unwrap();
    let envelope = read_envelope(&target)
        .expect("a classic root envelope still reads")
        .expect("the envelope is present");
    assert_eq!(envelope.codec, SnapshotCodecKind::Json);
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn root_envelope_names_classic_separates_absence_errors_and_unrelated_manifests() {
    // No manifest.json at all: a benign absence, no classic evidence.
    let empty = tempfile::tempdir().unwrap();
    assert!(!root_envelope_names_classic(empty.path()));

    // A vault folder's own unrelated manifest.json — no `codec` member,
    // or one naming the markdown codec — is not engine evidence.
    for bytes in [
        br#"{"name":"my vault"}"#.as_slice(),
        br#"{"codec":"markdown"}"#.as_slice(),
    ] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manifest.json"), bytes).unwrap();
        assert!(
            !root_envelope_names_classic(dir.path()),
            "unrelated manifest.json {bytes:?} must not be classic evidence"
        );
    }

    // An engine envelope naming a classic codec is evidence.
    let classic = tempfile::tempdir().unwrap();
    std::fs::write(
        classic.path().join("manifest.json"),
        r#"{"codec":"parquet","checkpoint_lsn":4}"#,
    )
    .unwrap();
    assert!(root_envelope_names_classic(classic.path()));

    // A manifest.json that cannot be read (here: a directory where the
    // file would be) is classic evidence, fail-closed: the pre-lease
    // guard refuses to create WAL engine state beside a manifest.json
    // it could not read, never folding the read error into "no
    // evidence".
    let unreadable = tempfile::tempdir().unwrap();
    std::fs::create_dir(unreadable.path().join("manifest.json")).unwrap();
    assert!(root_envelope_names_classic(unreadable.path()));

    // An oversized manifest.json is bounded before any byte is read
    // (the metadata-first policy its siblings apply) and fails closed
    // exactly like the unreadable one — never an unbounded read on the
    // open path.
    let oversized = tempfile::tempdir().unwrap();
    let mut bytes = vec![b' '; crate::snapshot::MAX_JSON_OBJECT as usize + 1];
    let prefix = br#"{"codec":"par""#;
    bytes[..prefix.len()].copy_from_slice(prefix);
    std::fs::write(oversized.path().join("manifest.json"), bytes).unwrap();
    assert!(root_envelope_names_classic(oversized.path()));
}

#[test]
fn a_markdown_sidecar_beside_other_evidence_is_refused() {
    // The evidence set check (one codec per directory): a markdown
    // sidecar beside `graph.json` names two codecs and fails.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".cypherlite")).unwrap();
    std::fs::write(
        dir.path().join(".cypherlite/manifest.json"),
        r#"{"codec":"markdown"}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("graph.json"), "{}").unwrap();
    let error = crate::parse_uri(dir.path().to_str().unwrap())
        .err()
        .expect("a markdown sidecar beside graph.json must not open");
    assert!(
        error
            .to_string()
            .contains("mixed snapshot codecs: directory has both markdown and json evidence"),
        "{error}"
    );
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
