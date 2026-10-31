use super::super::super::*;
use super::super::*;
use super::harness::checkpoint_at;
use super::harness::*;
use crate::tests::bare_manifest;
use crate::{memory_store, parse_uri, Wal};
use cypherlite_executor::Graph;
use std::sync::atomic::{AtomicUsize, Ordering};

fn assert_oversized_rejected(err: Error, reads: &AtomicUsize) {
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert!(err.to_string().contains("16 MiB"), "{err}");
    assert_eq!(reads.load(Ordering::SeqCst), 0);
}

#[test]
fn metadata_cap_precedes_read_in_codec_snapshot_and_legacy_pull_map() {
    let store = memory_store();
    put_object(
        &store,
        MANIFEST_JSON,
        br#"{"format_version":3,"next_node_id":0,"next_rel_id":0}"#,
    )
    .unwrap();
    for metadata_lies in [true, false] {
        let (injected, fake) = inject_oversized(&store, MANIFEST_JSON, metadata_lies);
        let err = if metadata_lies {
            crate::codec::resolve_codec(&injected, None).unwrap_err()
        } else {
            load(&injected).unwrap_err()
        };
        assert_oversized_rejected(err, &fake.reads);
    }
    assert_eq!(load(&store).unwrap().live_node_count(), 0);
}

#[test]
fn sparse_oversized_manifest_and_legacy_pull_identity_fail_without_loading() {
    let dir = tempfile::tempdir().unwrap();
    let target = parse_uri(dir.path().join("db").to_str().unwrap()).unwrap();
    let manifest = dir.path().join("db").join(MANIFEST_JSON);
    std::fs::write(
        &manifest,
        br#"{"format_version":3,"next_node_id":0,"next_rel_id":0}"#,
    )
    .unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&manifest)
        .unwrap();
    file.set_len(MAX_JSON_OBJECT + 1).unwrap();
    let err = load(&target).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert!(err.to_string().contains("16 MiB"), "{err}");
    std::fs::write(
        &manifest,
        b"{\"format_version\":3,\"next_node_id\":0,\"next_rel_id\":0}".as_slice(),
    )
    .unwrap();
    assert_eq!(load(&target).unwrap().live_node_count(), 0);
}

#[test]
fn metadata_writer_and_snapshot_accounting_keep_published_objects_bounded() {
    let store = memory_store();
    put_object(
        &store,
        MANIFEST_JSON,
        br#"{"format_version":3,"next_node_id":0,"next_rel_id":0}"#,
    )
    .unwrap();
    let oversized = vec![b' '; MAX_JSON_OBJECT as usize + 1];
    {
        let name = MANIFEST_JSON;
        let original = get_object(&store, name).unwrap();
        assert!(matches!(
            put_object(&store, name, &oversized),
            Err(Error::Storage(_))
        ));
        assert_eq!(get_object(&store, name).unwrap(), original);
        let (accounting, _) = inject_oversized(&store, name, true);
        assert!(matches!(
            snapshot_data_bytes(&accounting),
            Err(Error::Storage(_))
        ));
    }
}

#[test]
fn json_snapshot_accounting_propagates_bounded_manifest_read_error() {
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    put_object(&store, MANIFEST_JSON, br#"{"codec":"json","commit_ts":1}"#).unwrap();
    put_object(
        &store,
        "graph.json",
        br#"{"nodes":[],"relationships":[],"next_node_id":0,"next_rel_id":0}"#,
    )
    .unwrap();
    assert!(crate::snapshot_data_bytes(&store).unwrap() > 0);
    for metadata_lies in [true, false] {
        let (injected, fake) = inject_oversized(&store, MANIFEST_JSON, metadata_lies);
        assert_oversized_rejected(
            crate::snapshot_data_bytes(&injected).unwrap_err(),
            &fake.reads,
        );
    }
}

#[test]
fn snapshot_accounting_propagates_metadata_failures_for_both_codecs() {
    let parquet = memory_store();
    let manifest = Manifest {
        node_parts: vec![ShardPart {
            path: "nodes/0-1.parquet".into(),
            id_lo: 0,
            id_hi: 1,
            rows: 1,
            bytes: 4,
        }],
        ..bare_manifest(FORMAT_VERSION, 0, 0, 0, 0)
    };
    parquet.vfs.create_dir_all("nodes").unwrap();
    put_manifest(&parquet, &manifest);
    put_object(&parquet, "nodes/0-1.parquet", b"part").unwrap();
    let json = crate::memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    put_object(&json, MANIFEST_JSON, br#"{"codec":"json","commit_ts":1}"#).unwrap();
    put_object(
        &json,
        "graph.json",
        br#"{"nodes":[],"relationships":[],"next_node_id":0,"next_rel_id":0}"#,
    )
    .unwrap();

    for (store, name) in [
        (&parquet, "nodes/0-1.parquet"),
        (&json, MANIFEST_JSON),
        (&json, "graph.json"),
    ] {
        let (injected, _) = inject_vfs(store, |fake| fake.metadata_error = Some(name.into()));
        let err = crate::snapshot_data_bytes(&injected).unwrap_err();
        assert!(
            err.to_string().contains("injected metadata failure"),
            "{name}: {err}"
        );
    }
    assert!(crate::snapshot_data_bytes(&parquet).unwrap() > 0);
    assert!(crate::snapshot_data_bytes(&json).unwrap() > 0);
}

#[test]
fn missing_named_parts_fail_accounting_on_memory_and_bundle() {
    let dir = tempfile::tempdir().unwrap();
    for store in [
        memory_store(),
        parse_uri(dir.path().join("graph.cl").to_str().unwrap()).unwrap(),
    ] {
        let graph = Graph::default();
        let mut wal = Wal::memory();
        commit_two_nodes(&graph, &mut wal);
        checkpoint_at(&store, &graph, wal.lsn, graph.commit_ts(), 1).unwrap();
        let parts = manifest(&store).node_parts;
        assert_eq!(parts.len(), 2);
        let accounted = snapshot_data_bytes(&store).unwrap();
        put_object(&store, "nodes/999-1000.parquet", b"unreferenced").unwrap();
        assert_eq!(snapshot_data_bytes(&store).unwrap(), accounted);
        store.vfs.remove_file(&parts[0].path).unwrap();
        let err = snapshot_data_bytes(&store).unwrap_err();
        assert!(matches!(err, Error::Storage(_)), "{err}");
        assert!(err.to_string().contains(&parts[0].path), "{err}");
    }
}

#[test]
fn legacy_missing_monolith_remains_optional_for_accounting() {
    let store = memory_store();
    let published = put_manifest(&store, &bare_manifest(2, 0, 0, 0, 0));
    assert_eq!(snapshot_data_bytes(&store).unwrap(), published.len() as u64);
}

#[test]
fn oversized_legacy_and_v3_parquet_metadata_never_reads() {
    for name in [
        "nodes.parquet",
        "relationships.parquet",
        "nodes/0-4096.parquet",
        "relationships/0-4096.parquet",
    ] {
        for metadata_lies in [false, true] {
            let target = memory_store();
            if let Some(parent) = name.split_once('/').map(|(parent, _)| parent) {
                target.vfs.create_dir_all(parent).unwrap();
            }
            put_object(&target, name, b"small").unwrap();
            let (target, fake) = inject_oversized(&target, name, metadata_lies);
            let err = get_parquet(&target, name).unwrap_err();
            assert!(err.to_string().contains("1 GiB"), "{err}");
            assert_eq!(fake.reads.load(Ordering::SeqCst), 0, "{name}");
        }
    }
}

#[test]
fn parquet_length_growth_during_read_is_rejected() {
    let target = memory_store();
    put_object(&target, "nodes.parquet", b"small").unwrap();
    let (target, fake) = inject_vfs(&target, |fake| {
        fake.oversized = Some("nodes.parquet".into());
        fake.length_changes = true;
    });
    assert!(get_parquet(&target, "nodes.parquet")
        .unwrap_err()
        .to_string()
        .contains("1 GiB"));
    assert_eq!(fake.reads.load(Ordering::SeqCst), 1);
}

#[test]
fn snapshot_bytes_exclude_retained_unreferenced_monolith() {
    let store = memory_store();
    let graph = Graph::default();
    let mut wal = Wal::memory();
    commit_two_nodes(&graph, &mut wal);
    checkpoint_at(&store, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
    let bytes = snapshot_data_bytes(&store).unwrap();
    put_object(&store, "nodes.parquet", b"retained for Windows fallback").unwrap();
    assert_eq!(snapshot_data_bytes(&store).unwrap(), bytes);
}
