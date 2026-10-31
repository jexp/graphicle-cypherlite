use super::super::*;
use super::harness::checkpoint_at;
use super::harness::*;
use crate::tests::bare_manifest;
use crate::{memory_store, Wal};
use cypherlite_executor::Graph;

#[test]
fn impossible_part_metrics_fail_before_checkpoint_mutates_files() {
    let store = memory_store();
    let graph = Graph::default();
    let mut wal = Wal::memory();
    commit_two_nodes(&graph, &mut wal);
    checkpoint_at(&store, &graph, wal.lsn, graph.commit_ts(), 1).unwrap();
    let mut manifest = manifest(&store);
    assert_eq!(manifest.node_parts.len(), 2);
    modify_two_nodes(&graph, &mut wal);
    let paths: Vec<_> = manifest.node_parts.iter().map(|p| p.path.clone()).collect();
    let original_parts: Vec<_> = paths
        .iter()
        .map(|p| get_object(&store, p).unwrap())
        .collect();

    for (rows, bytes) in [
        (1, u64::MAX),
        (u64::MAX, 1),
        (1_000_001, 1),
        (4097, 1),
        (1, MAX_FILE + 1),
        (0, 1),
        (1, 0),
    ] {
        for part in &mut manifest.node_parts {
            part.rows = rows;
            part.bytes = bytes;
        }
        let forged = put_manifest(&store, &manifest);
        let read_err = read_manifest(&store).unwrap_err();
        assert!(matches!(read_err, Error::Storage(_)), "{read_err}");
        let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            checkpoint_at(&store, &graph, wal.lsn, graph.commit_ts(), 1)
        }))
        .expect("forged metrics must return an error rather than panic")
        .unwrap_err();
        assert!(matches!(err, Error::Storage(_)), "{err}");
        assert_eq!(get_object(&store, MANIFEST_JSON).unwrap(), forged);
        for (path, bytes) in paths.iter().zip(&original_parts) {
            assert_eq!(get_object(&store, path).unwrap(), *bytes);
        }
        assert!(!graph.dirty_node_ids().is_empty());
        assert_eq!(store.vfs.list_dir("nodes").unwrap().len(), 2);
    }
}

#[test]
fn density_totals_use_checked_arithmetic() {
    let parts = [
        ShardPart {
            path: "nodes/0-1.parquet".into(),
            id_lo: 0,
            id_hi: 1,
            rows: 1,
            bytes: u64::MAX,
        },
        ShardPart {
            path: "nodes/1-2.parquet".into(),
            id_lo: 1,
            id_hi: 2,
            rows: 1,
            bytes: u64::MAX,
        },
    ];
    assert!(matches!(ewma_density(0.0, &parts), Err(Error::Storage(_))));
}

#[test]
fn manifest_timestamp_sentinel_is_typed_and_previous_value_loads() {
    let target = memory_store();
    for ts in [u64::MAX, u64::MAX - 1] {
        put_manifest(&target, &bare_manifest(FORMAT_VERSION, i64::MAX, 0, 0, ts));
        if ts == u64::MAX {
            let err = load(&target).unwrap_err();
            assert!(matches!(err, Error::Storage(_)), "{err}");
        } else {
            let graph = load(&target).unwrap();
            assert_eq!(graph.commit_ts(), ts);
            assert_eq!(graph.next_node_id(), i64::MAX);
        }
    }
}

#[test]
fn invalid_target_and_direct_checkpoint_target_fail_before_writing() {
    for invalid in [
        "",
        "0",
        "-1",
        "invalid",
        "1073741825",
        "18446744073709551616",
    ] {
        assert!(parse_shard_target(invalid)
            .unwrap_err()
            .to_string()
            .contains("1073741824"));
    }
    assert_eq!(parse_shard_target("1073741824").unwrap(), MAX_FILE);
    let store = memory_store();
    for invalid in [0, MAX_FILE + 1, u64::MAX] {
        assert!(checkpoint_at(&store, &Graph::default(), 0, 0, invalid).is_err());
        assert!(!object_exists(&store, MANIFEST_JSON).unwrap());
    }
}

#[test]
fn fat_rows_narrow_the_window() {
    let fat = window_width(256 * 1024 * 1024, 8192.0);
    let skinny = window_width(256 * 1024 * 1024, 256.0);
    assert!(fat < skinny);
    assert_eq!(fat, (256 * 1024 * 1024) / 8192);
    assert_eq!(skinny, 1_000_000);
}

#[cfg(unix)]
#[test]
fn unreadable_manifest_is_an_error_not_an_empty_graph() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let target = crate::parse_uri(dir.path().to_str().unwrap()).unwrap();
    std::fs::write(dir.path().join(MANIFEST_JSON), b"{}").unwrap();
    std::fs::set_permissions(
        dir.path().join(MANIFEST_JSON),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let err = super::super::load(&target).expect_err("unreadable manifest must error");
    assert!(matches!(err, cypherlite_core::Error::Storage(_)), "{err}");
    assert!(
        super::super::read_manifest(&target).is_err(),
        "read_manifest must propagate non-NotFound errors"
    );
    std::fs::set_permissions(
        dir.path().join(MANIFEST_JSON),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
}

#[test]
fn missing_manifest_opens_as_empty_graph() {
    let dir = tempfile::tempdir().unwrap();
    let target = crate::parse_uri(dir.path().to_str().unwrap()).unwrap();
    let g = super::super::load(&target).unwrap();
    assert_eq!(g.live_node_count(), 0);
    assert!(super::super::read_manifest(&target).unwrap().is_none());
}
