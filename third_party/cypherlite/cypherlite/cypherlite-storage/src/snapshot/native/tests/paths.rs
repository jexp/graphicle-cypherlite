use super::super::super::*;
use super::super::*;
use super::harness::checkpoint_at;
use super::harness::*;
use crate::{load_with_wal, memory_store, open_store, parse_uri, Wal};
use cypherlite_executor::Graph;

#[test]
fn mixed_fixed_name_v3_and_generation_paths_reopen_and_sweep_only_orphans() {
    let store = memory_store();
    let mut wal = Wal::memory();
    let graph = Graph::default();
    commit_two_nodes(&graph, &mut wal);
    // Target 1 keeps the two rows in separate parts (ADR-0183).
    checkpoint_at(&store, &graph, wal.lsn, graph.commit_ts(), 1).unwrap();
    let mut old = manifest(&store);
    let fixed = format!(
        "nodes/{}-{}.parquet",
        old.node_parts[1].id_lo, old.node_parts[1].id_hi
    );
    let original = old.node_parts[1].path.clone();
    put_object(&store, &fixed, &get_object(&store, &original).unwrap()).unwrap();
    old.node_parts[1].path = fixed.clone();
    put_manifest(&store, &old);
    store.vfs.remove_file(&original).unwrap();
    modify_two_nodes(&graph, &mut wal);
    checkpoint_at(&store, &graph, wal.lsn, graph.commit_ts(), 1).unwrap();
    let manifest = manifest(&store);
    assert!(manifest.node_parts.iter().all(|p| p.path != fixed));

    // Put one fixed-name part back as an unchanged reference, alongside a
    // new generation part. Crash debris matching the generation pattern
    // is removed only after snapshot + WAL recovery succeed.
    let mut mixed = manifest.clone();
    let keep = mixed.node_parts[1].path.clone();
    put_object(&store, &fixed, &get_object(&store, &keep).unwrap()).unwrap();
    mixed.node_parts[1].path = fixed.clone();
    put_manifest(&store, &mixed);
    let orphan = "nodes/123-456-0123456789abcdef0123456789abcdef.parquet";
    put_object(&store, orphan, b"orphan").unwrap();
    let restored = load_with_wal(&store, &wal).unwrap();
    assert_eq!(restored.live_node_count(), 2);
    assert_eq!(
        restored.node(300_000).unwrap().properties["x"],
        cypherlite_core::Value::Int(8)
    );
    sweep_orphans(&store).unwrap();
    assert!(object_exists(&store, &fixed).unwrap());
    assert!(object_exists(&store, &mixed.node_parts[0].path).unwrap());
    assert!(!object_exists(&store, orphan).unwrap());
    assert!(!object_exists(&store, &keep).unwrap());
}

#[test]
fn invalid_manifest_paths_fail_before_read_retirement_or_wal_recycle_and_keep_lock() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    let target = parse_uri(uri.to_str().unwrap()).unwrap();
    let (vfs, prefix) = target.wal_vfs();
    let mut wal = Wal::open(vfs, prefix, 0).unwrap();
    let graph = Graph::default();
    committed_nodes(&graph, &mut wal, ["A", "B"]);
    checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
    let original = manifest(&target);
    // The lock markers are held under an exclusive byte-range lock, so capture
    // their size from directory metadata rather than reopening the raw files
    // (which fails with `os error 33` on Windows below).
    let lock_len = std::fs::metadata(uri.join(crate::LOCK_MARKER))
        .unwrap()
        .len();
    let wal_lock_len = std::fs::metadata(uri.join("wal").join(crate::LOCK_MARKER))
        .unwrap()
        .len();
    let wal_bytes = get_object(&target, "wal/00000001.log").unwrap();
    let part = original.node_parts[0].path.clone();
    let part_bytes = get_object(&target, &part).unwrap();

    for path in [
        ".cypherlite.lock",
        "wal/.cypherlite.lock",
        "nodes/../.cypherlite.lock",
        "nodes/../../wal/.cypherlite.lock",
        "/tmp/0-4096.parquet",
        "nodes\\0-4096.parquet",
        "nodes/00-4096.parquet",
        "nodes/0-4096-ABCDEF0123456789abcdef0123456789.parquet",
        "relationships/0-4096.parquet",
    ] {
        let mut forged = original.clone();
        forged.node_parts[0].path = path.into();
        put_manifest(&target, &forged);
        for err in [
            load(&target).err().unwrap(),
            checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), MAX_FILE)
                .err()
                .unwrap(),
            sweep_orphans(&target).err().unwrap(),
        ] {
            assert!(matches!(err, Error::Storage(_)), "{path}: {err}");
            assert!(err.to_string().contains("shard path"), "{path}: {err}");
        }
        assert_eq!(get_object(&target, "wal/00000001.log").unwrap(), wal_bytes);
        assert_eq!(get_object(&target, &part).unwrap(), part_bytes);
        assert_eq!(
            std::fs::metadata(uri.join(crate::LOCK_MARKER))
                .unwrap()
                .len(),
            lock_len
        );
        assert_eq!(
            std::fs::metadata(uri.join("wal").join(crate::LOCK_MARKER))
                .unwrap()
                .len(),
            wal_lock_len
        );
        assert!(matches!(
            parse_uri(uri.to_str().unwrap()),
            Err(Error::Locked(_))
        ));
    }
    put_manifest(&target, &original);
    assert_eq!(load_with_wal(&target, &wal).unwrap().live_node_count(), 2);
    drop((target, wal));
    // With the lease released, the role bytes are readable directly and prove
    // the markers were neither recycled nor rewritten.
    assert_eq!(
        std::fs::read(uri.join(crate::LOCK_MARKER)).unwrap(),
        crate::ROOT_ROLE
    );
    assert_eq!(
        std::fs::read(uri.join("wal").join(crate::LOCK_MARKER)).unwrap(),
        crate::WAL_ROLE
    );
    assert_eq!(
        open_store(&parse_uri(uri.to_str().unwrap()).unwrap())
            .unwrap()
            .0
            .live_node_count(),
        2
    );
}

#[test]
fn lock_named_objects_with_valid_parquet_bytes_are_not_manifest_parts() {
    let target = memory_store();
    let graph = Graph::default();
    let mut wal = Wal::memory();
    committed_nodes(&graph, &mut wal, ["A", "B"]);
    checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
    let original = manifest(&target);
    let valid_parquet = get_object(&target, &original.node_parts[0].path).unwrap();
    for name in [".cypherlite.lock", "wal/.cypherlite.lock"] {
        put_object(&target, name, &valid_parquet).unwrap();
        let mut forged = original.clone();
        forged.node_parts[0].path = name.into();
        put_manifest(&target, &forged);
        assert!(matches!(load(&target), Err(Error::Storage(_))));
        assert!(matches!(sweep_orphans(&target), Err(Error::Storage(_))));
        assert_eq!(get_object(&target, name).unwrap(), valid_parquet);
    }
    put_manifest(&target, &original);
    assert_eq!(load(&target).unwrap().live_node_count(), 2);
}

#[cfg(unix)]
#[test]
fn valid_shard_name_symlink_alias_is_rejected_before_read_or_delete() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    let target = parse_uri(uri.to_str().unwrap()).unwrap();
    let graph = Graph::default();
    let (vfs, prefix) = target.wal_vfs();
    let mut wal = Wal::open(vfs, prefix, 0).unwrap();
    committed_nodes(&graph, &mut wal, ["A", "B"]);
    checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
    let manifest = manifest(&target);
    let path = uri.join(&manifest.node_parts[0].path);
    let bytes = std::fs::read(&path).unwrap();
    let elsewhere = uri.join("elsewhere.parquet");
    std::fs::write(&elsewhere, &bytes).unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink(&elsewhere, &path).unwrap();
    assert!(matches!(load(&target), Err(Error::Storage(_))));
    assert!(matches!(
        delete_object(&target, &manifest.node_parts[0].path),
        Err(Error::Storage(_))
    ));
    assert!(matches!(
        checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), MAX_FILE),
        Err(Error::Storage(_))
    ));
    assert!(matches!(sweep_orphans(&target), Err(Error::Storage(_))));
    assert_eq!(std::fs::read(&elsewhere).unwrap(), bytes);
    assert!(path.symlink_metadata().unwrap().file_type().is_symlink());

    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let node_dir = uri.join("nodes");
    let actual_dir = uri.join("real-nodes");
    std::fs::rename(&node_dir, &actual_dir).unwrap();
    symlink(&actual_dir, &node_dir).unwrap();
    assert!(matches!(load(&target), Err(Error::Storage(_))));
    assert!(matches!(
        delete_object(&target, &manifest.node_parts[0].path),
        Err(Error::Storage(_))
    ));
    assert!(matches!(
        checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), MAX_FILE),
        Err(Error::Storage(_))
    ));
    assert_eq!(
        std::fs::read(actual_dir.join(path.file_name().unwrap())).unwrap(),
        bytes
    );
}

#[cfg(unix)]
#[test]
fn native_writer_refuses_unreferenced_shard_directory_alias_and_replays_wal() {
    use std::os::unix::fs::symlink;

    type Commit = fn(&Graph, &mut Wal);
    let scenarios: [(&str, &[Commit], &[Commit]); 2] = [
        ("nodes", &[], &[commit_two_nodes, commit_test_rel]),
        ("relationships", &[commit_two_nodes], &[commit_test_rel]),
    ];
    for (kind, before, after) in scenarios {
        let dir = tempfile::tempdir().unwrap();
        let uri = dir.path().join("nested/db");
        let target = parse_uri(uri.to_str().unwrap()).unwrap();
        let (vfs, prefix) = target.wal_vfs();
        let mut wal = Wal::open(vfs, prefix, 0).unwrap();
        let graph = Graph::default();
        for commit in before {
            commit(&graph, &mut wal);
        }
        checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
        let old_manifest = get_object(&target, MANIFEST_JSON).unwrap();
        assert!(!uri.join(kind).exists(), "{kind} should have no old parts");

        for commit in after {
            commit(&graph, &mut wal);
        }
        let expected = graph_state(&graph);
        let lsn = wal.lsn;
        let wal_bytes = get_object(&target, "wal/00000001.log").unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        std::fs::write(&sentinel, b"unchanged").unwrap();
        symlink(&outside, uri.join(kind)).unwrap();

        let error = checkpoint_at(&target, &graph, lsn, graph.commit_ts(), MAX_FILE).unwrap_err();
        assert!(matches!(error, Error::Storage(_)), "{error}");
        assert!(error.to_string().contains("shard directory"), "{error}");
        assert_eq!(get_object(&target, MANIFEST_JSON).unwrap(), old_manifest);
        assert_eq!(get_object(&target, "wal/00000001.log").unwrap(), wal_bytes);
        assert_eq!(wal.lsn, lsn);
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1);
        assert!(uri
            .join(kind)
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());

        std::fs::remove_file(uri.join(kind)).unwrap();
        drop((target, wal));
        let reopened = parse_uri(uri.to_str().unwrap()).unwrap();
        let (restored, replayed_wal) = open_store(&reopened).unwrap();
        assert_eq!(graph_state(&restored), expected);
        assert_eq!(replayed_wal.lsn, lsn);
        assert!(replayed_wal.lsn > manifest(&reopened).checkpoint_lsn);
        checkpoint_at(&reopened, &restored, lsn, restored.commit_ts(), MAX_FILE).unwrap();
        assert_eq!(graph_state(&load(&reopened).unwrap()), expected);
    }
}

#[cfg(unix)]
#[test]
fn native_writer_rechecks_unchanged_legacy_shard_before_manifest_publication() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    let target = parse_uri(uri.to_str().unwrap()).unwrap();
    let (vfs, prefix) = target.wal_vfs();
    let mut wal = Wal::open(vfs, prefix, 0).unwrap();
    let graph = Graph::default();
    commit_two_nodes(&graph, &mut wal);
    checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
    let mut legacy = manifest(&target);
    let original = legacy.node_parts[0].path.clone();
    let fixed = ShardName::fixed(
        Kind::Node,
        legacy.node_parts[0].id_lo,
        legacy.node_parts[0].id_hi,
    )
    .path();
    put_object(&target, &fixed, &get_object(&target, &original).unwrap()).unwrap();
    target.vfs.remove_file(&original).unwrap();
    legacy.node_parts[0].path = fixed;
    let old_manifest = put_manifest(&target, &legacy);
    commit_test_rel(&graph, &mut wal);
    let expected = graph_state(&graph);
    let wal_bytes = get_object(&target, "wal/00000001.log").unwrap();

    let outside = dir.path().join("outside-nodes");
    let (injected, _) = inject_vfs(&target, |fake| {
        fake.alias_after_sync = Some((Kind::Rel.dir().into(), uri.join("nodes"), outside.clone()));
    });
    let error = checkpoint_at(&injected, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(error.to_string().contains("shard path"), "{error}");
    assert_eq!(get_object(&target, MANIFEST_JSON).unwrap(), old_manifest);
    assert_eq!(get_object(&target, "wal/00000001.log").unwrap(), wal_bytes);
    assert!(outside
        .join(legacy.node_parts[0].path.strip_prefix("nodes/").unwrap())
        .is_file());
    std::fs::remove_file(uri.join("nodes")).unwrap();
    std::fs::rename(&outside, uri.join("nodes")).unwrap();
    drop((injected, target, wal));
    let reopened = parse_uri(uri.to_str().unwrap()).unwrap();
    assert_eq!(graph_state(&open_store(&reopened).unwrap().0), expected);
}

#[test]
fn shard_writer_guard_allows_nested_native_and_virtual_directories() {
    let dir = tempfile::tempdir().unwrap();
    let native = dir.path().join("parent/nested/db");
    let bundle = dir.path().join("graph.cl");
    for store in [
        parse_uri(native.to_str().unwrap()).unwrap(),
        parse_uri(bundle.to_str().unwrap()).unwrap(),
        memory_store(),
    ] {
        let (vfs, prefix) = store.wal_vfs();
        let mut wal = Wal::open(vfs, prefix, 0).unwrap();
        let graph = Graph::default();
        commit_two_nodes(&graph, &mut wal);
        commit_test_rel(&graph, &mut wal);
        checkpoint_at(&store, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
        let manifest = manifest(&store);
        assert_eq!(manifest.node_parts.len(), 1);
        assert_eq!(manifest.rel_parts.len(), 1);
        assert_reopens_as(&store, &graph);
    }
}

#[test]
fn shard_parser_governs_manifest_deletion_and_sweep_on_all_vfs() {
    let dir = tempfile::tempdir().unwrap();
    let targets = [
        memory_store(),
        parse_uri(dir.path().join("db").to_str().unwrap()).unwrap(),
        parse_uri(dir.path().join("graph.cl").to_str().unwrap()).unwrap(),
    ];
    for target in targets {
        let (vfs, prefix) = target.wal_vfs();
        let mut wal = Wal::open(vfs, prefix, 0).unwrap();
        let graph = Graph::default();
        commit_two_nodes(&graph, &mut wal);
        commit_test_rel(&graph, &mut wal);
        checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
        let mut manifest = manifest(&target);
        let selected_node = manifest.node_parts[0].path.clone();
        let selected_rel = manifest.rel_parts[0].path.clone();
        let fixed_rel = ShardName::fixed(
            Kind::Rel,
            manifest.rel_parts[0].id_lo,
            manifest.rel_parts[0].id_hi,
        )
        .path();
        put_object(
            &target,
            &fixed_rel,
            &get_object(&target, &selected_rel).unwrap(),
        )
        .unwrap();
        manifest.rel_parts[0].path = fixed_rel.clone();
        put_manifest(&target, &manifest);
        let orphan_node = "nodes/12-34-0123456789abcdef0123456789abcdef.parquet";
        let orphan_rel = "relationships/12-34-0123456789abcdef0123456789abcdef.parquet";
        let fixed_orphan = "nodes/12-34.parquet";
        let spoof = "nodes/012-34-0123456789abcdef0123456789abcdef.parquet";
        let spoof_suffix = "nodes/12-34-0123456789abcdef0123456789abcdef.parquet.bak";
        for path in [orphan_node, orphan_rel, fixed_orphan, spoof, spoof_suffix] {
            put_object(&target, path, b"not selected").unwrap();
        }
        for bad in [
            "nodes/012-34-0123456789abcdef0123456789abcdef.parquet",
            "nodes/12-34-0123456789abcdef0123456789abcdef.parquet.bak",
            "nodes/12-34-0123456789abcdef0123456789abcdef.parquet/extra",
            "nodes/../.cypherlite.lock",
            "nodes\\12-34.parquet",
        ] {
            assert!(ShardName::parse(bad).is_none(), "{bad}");
            assert!(
                matches!(delete_object(&target, bad), Err(Error::Storage(_))),
                "{bad}"
            );
            let mut forged = manifest.clone();
            forged.node_parts[0].path = bad.into();
            put_manifest(&target, &forged);
            assert!(matches!(load(&target), Err(Error::Storage(_))), "{bad}");
            assert!(
                matches!(sweep_orphans(&target), Err(Error::Storage(_))),
                "{bad}"
            );
            assert!(object_exists(&target, &selected_node).unwrap());
            assert!(object_exists(&target, orphan_node).unwrap());
        }
        put_manifest(&target, &manifest);
        assert!(delete_object(&target, spoof).is_err());
        assert!(object_exists(&target, spoof).unwrap());
        assert_eq!(graph_state(&load(&target).unwrap()), graph_state(&graph));
        assert_reopens_as(&target, &graph);
        assert!(object_exists(&target, &selected_node).unwrap());
        assert!(object_exists(&target, &fixed_rel).unwrap());
        assert!(object_exists(&target, fixed_orphan).unwrap());
        assert!(object_exists(&target, spoof).unwrap());
        assert!(object_exists(&target, spoof_suffix).unwrap());
        if !deferred_retire(&target) {
            assert!(!object_exists(&target, orphan_node).unwrap());
            assert!(!object_exists(&target, orphan_rel).unwrap());
            assert!(!object_exists(&target, &selected_rel).unwrap());
        }
        drop(wal);
    }
}

#[cfg(unix)]
#[test]
fn orphan_sweep_does_not_follow_shard_symlink() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    let target = parse_uri(uri.to_str().unwrap()).unwrap();
    let (vfs, prefix) = target.wal_vfs();
    let mut wal = Wal::open(vfs, prefix, 0).unwrap();
    let graph = Graph::default();
    commit_two_nodes(&graph, &mut wal);
    checkpoint_at(&target, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
    let sentinel = dir.path().join("sentinel");
    std::fs::write(&sentinel, b"untouched").unwrap();
    let symlink_path = uri.join("nodes/123-456-0123456789abcdef0123456789abcdef.parquet");
    symlink(&sentinel, &symlink_path).unwrap();
    load_with_wal(&target, &wal).unwrap();
    sweep_orphans(&target).unwrap();
    assert!(std::fs::symlink_metadata(&symlink_path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"untouched");
}
