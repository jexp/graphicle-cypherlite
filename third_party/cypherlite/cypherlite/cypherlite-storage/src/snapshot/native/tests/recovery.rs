use super::super::super::*;
use super::super::*;
use super::harness::checkpoint_at;
use super::harness::*;
use crate::tests::bare_manifest;
use crate::{load_with_wal, memory_store, open_store, parse_uri, Wal};
use cypherlite_executor::Graph;
use std::sync::atomic::Ordering;

fn failed_checkpoint_keeps_old_parts(
    mut target: crate::StoreTarget,
    reopen: impl FnOnce() -> crate::StoreTarget,
) {
    let (vfs, prefix) = target.wal_vfs();
    let mut wal = Wal::open(vfs, prefix, 0).unwrap();
    let graph = Graph::default();
    commit_two_nodes(&graph, &mut wal);
    // Target 1 keeps the two rows in separate parts (ADR-0183), so the
    // rewrite writes two parts and the injected failure hits the second.
    checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), 1).unwrap();
    let old_manifest = get_object(&target, MANIFEST_JSON).unwrap();
    let old = manifest(&target);
    let old_bytes: Vec<_> = old
        .node_parts
        .iter()
        .map(|p| (p.path.clone(), get_object(&target, &p.path).unwrap()))
        .collect();
    modify_two_nodes(&graph, &mut wal);
    (target, _) = inject_vfs(&target, |fake| fake.fail_part = Some(2));
    let err = checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), 1).unwrap_err();
    assert!(err.to_string().contains("injected second part"), "{err}");
    assert_eq!(get_object(&target, MANIFEST_JSON).unwrap(), old_manifest);
    for (name, bytes) in old_bytes {
        assert_eq!(get_object(&target, &name).unwrap(), bytes);
    }
    let current = manifest(&target);
    for dir in ["nodes", "relationships"] {
        if target.vfs.exists(dir).unwrap() {
            for name in target.vfs.list_dir(dir).unwrap() {
                assert!(
                    referenced(&current, &format!("{dir}/{name}")),
                    "failed checkpoint left orphan {dir}/{name}"
                );
            }
        }
    }
    let restored = load_with_wal(&target, &wal).unwrap();
    assert_eq!(
        restored.node(0).unwrap().properties["x"],
        cypherlite_core::Value::Int(7)
    );
    assert_eq!(
        restored.node(300_000).unwrap().properties["x"],
        cypherlite_core::Value::Int(8)
    );
    drop((target, wal));
    let reopened = reopen();
    let (restored, wal) = open_store(&reopened).unwrap();
    assert_eq!(
        restored.node(0).unwrap().properties["x"],
        cypherlite_core::Value::Int(7)
    );
    assert_eq!(
        restored.node(300_000).unwrap().properties["x"],
        cypherlite_core::Value::Int(8)
    );
    assert!(wal.lsn > old.checkpoint_lsn);
}

#[test]
fn partial_checkpoint_failure_preserves_native_and_bundle_and_memory_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let native = dir.path().join("db");
    let bundle = dir.path().join("graph.cl");
    for path in [&native, &bundle] {
        let uri = path.to_str().unwrap();
        failed_checkpoint_keeps_old_parts(parse_uri(uri).unwrap(), || parse_uri(uri).unwrap());
    }
    let memory = memory_store();
    let reopened = memory.clone();
    failed_checkpoint_keeps_old_parts(memory, || reopened);
}

fn manifest_sync_ambiguity_retains_visible_parts_and_wal_for(
    store: crate::StoreTarget,
    reopen: impl FnOnce() -> crate::StoreTarget,
) {
    let (vfs, prefix) = store.wal_vfs();
    let mut wal = Wal::open(vfs, prefix, 0).unwrap();
    let graph = Graph::default();
    commit_two_nodes(&graph, &mut wal);
    let old_state = graph_state(&graph);
    checkpoint_at(&store, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
    let old = manifest(&store);
    let old_bytes = get_object(&store, &old.node_parts[0].path).unwrap();
    modify_two_nodes(&graph, &mut wal);
    let new_state = graph_state(&graph);
    let (injected, _) = inject_vfs(&store, |fake| fake.fail_manifest_sync = true);
    let error = checkpoint_at(&injected, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap_err();
    assert!(
        error.to_string().contains("manifest directory sync"),
        "{error}"
    );
    let visible = manifest(&store);
    assert!(visible.checkpoint_lsn > old.checkpoint_lsn);
    assert_ne!(visible.node_parts[0].path, old.node_parts[0].path);
    for part in &visible.node_parts {
        assert!(object_exists(&store, &part.path).unwrap());
    }
    // The old generation is retained, not overwritten.
    assert_eq!(
        get_object(&store, &old.node_parts[0].path).unwrap(),
        old_bytes
    );
    let restored = load_with_wal(&store, &wal).unwrap();
    assert_eq!(graph_state(&load(&store).unwrap()), new_state);
    assert_eq!(graph_state(&restored), new_state);
    assert_eq!(
        restored.node(0).unwrap().properties["x"],
        cypherlite_core::Value::Int(7)
    );
    put_manifest(&store, &old);
    let old_graph = load(&store).unwrap();
    assert_eq!(graph_state(&old_graph), old_state);
    assert!(!old_graph.node(0).unwrap().properties.contains_key("x"));
    assert_eq!(
        graph_state(&load_with_wal(&store, &wal).unwrap()),
        new_state
    );
    drop((injected, store, wal));
    let reopened = reopen();
    let (recovered, _) = open_store(&reopened).unwrap();
    assert_eq!(graph_state(&recovered), new_state);
}

#[test]
fn manifest_sync_ambiguity_retains_visible_parts_and_wal_on_all_vfses() {
    let dir = tempfile::tempdir().unwrap();
    for path in [dir.path().join("db"), dir.path().join("graph.cl")] {
        let uri = path.to_str().unwrap();
        manifest_sync_ambiguity_retains_visible_parts_and_wal_for(parse_uri(uri).unwrap(), || {
            parse_uri(uri).unwrap()
        });
    }
    let memory = memory_store();
    let reopened = memory.clone();
    manifest_sync_ambiguity_retains_visible_parts_and_wal_for(memory, || reopened);
}

#[test]
fn legacy_v1_v2_monoliths_remain_readable_with_pre_read_limit() {
    let graph = Graph::default();
    let mut wal = Wal::memory();
    commit_two_nodes(&graph, &mut wal);
    let ts = graph.commit_ts();
    for version in [1, 2] {
        let store = memory_store();
        let nodes =
            crate::codec::parquet::nodes_parquet_ids(&graph, ts, &graph.all_node_ids_at(ts))
                .unwrap();
        put_object(&store, "nodes.parquet", &nodes).unwrap();
        put_object(
            &store,
            "relationships.parquet",
            &crate::codec::parquet::rels_parquet_ids(&graph, ts, &[]).unwrap(),
        )
        .unwrap();
        let manifest = bare_manifest(
            version,
            graph.next_node_id(),
            graph.next_rel_id(),
            wal.lsn,
            ts,
        );
        put_manifest(&store, &manifest);
        assert_eq!(load(&store).unwrap().live_node_count(), 2);
        let (store, fake) = inject_oversized(&store, "nodes.parquet", true);
        assert!(load(&store).unwrap_err().to_string().contains("1 GiB"));
        assert_eq!(fake.reads.load(Ordering::SeqCst), 0);
    }
}

fn empty_v3_ignores_retained_monoliths(
    target: crate::StoreTarget,
    reopen: impl FnOnce() -> crate::StoreTarget,
) {
    let (vfs, prefix) = target.wal_vfs();
    let mut wal = Wal::open(vfs, prefix, 0).unwrap();
    let graph = Graph::default();
    graph.begin_write().unwrap();
    graph
        .create_node(vec!["A".into()], Default::default())
        .unwrap();
    graph
        .create_node(vec!["B".into()], Default::default())
        .unwrap();
    graph
        .create_rel(0, 1, "R".into(), Default::default())
        .unwrap();
    commit_open_write(&graph, &mut wal);
    let ts = graph.commit_ts();
    checkpoint_at(&target, &graph, wal.lsn, ts, MAX_FILE).unwrap();
    let old_nodes = nodes_parquet_ids(&graph, ts, &[0, 1]).unwrap();
    let old_rels = rels_parquet_ids(&graph, ts, &[0]).unwrap();

    graph.begin_write().unwrap();
    graph.delete_node(0, true).unwrap();
    graph.delete_node(1, true).unwrap();
    commit_open_write(&graph, &mut wal);
    let ts = graph.commit_ts();
    checkpoint_at(&target, &graph, wal.lsn, ts, MAX_FILE).unwrap();
    let manifest = manifest(&target);
    assert_eq!(manifest.format_version, FORMAT_VERSION);
    assert!(manifest.node_parts.is_empty() && manifest.rel_parts.is_empty());
    let bytes = snapshot_data_bytes(&target).unwrap();
    // Simulate a crash before cleanup or Windows Native's deferred retire.
    put_object(&target, "nodes.parquet", &old_nodes).unwrap();
    put_object(&target, "relationships.parquet", &old_rels).unwrap();
    assert_eq!(snapshot_data_bytes(&target).unwrap(), bytes);
    let cut = wal.rotate().unwrap();
    wal.recycle(cut).unwrap();
    drop((target, wal));
    let reopened = reopen();
    let (restored, _) = open_store(&reopened).unwrap();
    assert_eq!(restored.live_node_count(), 0);
    assert!(restored.rel_at(0, ts).is_none());
    assert!(object_exists(&reopened, "nodes.parquet").unwrap());
    assert!(object_exists(&reopened, "relationships.parquet").unwrap());
}

#[test]
fn empty_v3_part_lists_are_authoritative_after_wal_recycle_on_all_vfs() {
    let dir = tempfile::tempdir().unwrap();
    for path in [dir.path().join("db"), dir.path().join("graph.cl")] {
        let uri = path.to_str().unwrap();
        empty_v3_ignores_retained_monoliths(parse_uri(uri).unwrap(), || parse_uri(uri).unwrap());
    }
    let memory = memory_store();
    let reopened = memory.clone();
    empty_v3_ignores_retained_monoliths(memory, || reopened);
}

#[test]
fn crash_orphans_swept_only_after_validated_reopen_on_all_vfs() {
    let dir = tempfile::tempdir().unwrap();
    let native = dir.path().join("db");
    let bundle = dir.path().join("graph.cl");
    let orphan = "nodes/123-456-0123456789abcdef0123456789abcdef.parquet";
    for path in [&native, &bundle] {
        let uri = path.to_str().unwrap();
        let target = parse_uri(uri).unwrap();
        let (vfs, prefix) = target.wal_vfs();
        let mut wal = Wal::open(vfs, prefix, 0).unwrap();
        let graph = Graph::default();
        commit_two_nodes(&graph, &mut wal);
        checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
        let live = manifest(&target).node_parts[0].path.clone();
        put_object(&target, orphan, b"crashed attempt").unwrap();
        drop((target, wal));
        let reopened = parse_uri(uri).unwrap();
        assert!(object_exists(&reopened, orphan).unwrap());
        let (restored, _) = open_store(&reopened).unwrap();
        assert_eq!(restored.live_node_count(), 2);
        assert!(object_exists(&reopened, &live).unwrap());
        if !deferred_retire(&reopened) {
            assert!(!object_exists(&reopened, orphan).unwrap());
        }
    }

    let memory = memory_store();
    let (vfs, prefix) = memory.wal_vfs();
    let mut wal = Wal::open(vfs, prefix, 0).unwrap();
    let graph = Graph::default();
    commit_two_nodes(&graph, &mut wal);
    checkpoint_at(&memory, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
    put_object(&memory, orphan, b"crashed attempt").unwrap();
    let valid = get_object(&memory, MANIFEST_JSON).unwrap();
    put_object(&memory, MANIFEST_JSON, b"invalid manifest").unwrap();
    assert!(load_with_wal(&memory, &wal).is_err());
    assert!(object_exists(&memory, orphan).unwrap());
    put_object(&memory, MANIFEST_JSON, &valid).unwrap();
    drop(wal);
    assert_eq!(open_store(&memory).unwrap().0.live_node_count(), 2);
    assert!(!object_exists(&memory, orphan).unwrap());
}
