//! Rewrite units (ADR-0183): a checkpoint coalesces neighboring parts up to
//! the byte target instead of writing one shard per dirty or clean run.

use super::super::*;
use super::harness::checkpoint_at;
use super::harness::*;
use crate::codec::parquet::{
    take_encoded_rows, take_measured_rows, with_test_envelope, with_test_file_ceiling,
    ROW_GROUP_CAP,
};
use crate::memory_store;
use cypherlite_core::Value;
use cypherlite_executor::Graph;
use std::sync::atomic::Ordering;

#[test]
fn interleaved_dirty_clean_nodes_stay_one_shard() {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, 2000, 24);
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    assert_eq!(manifest(&store).node_parts.len(), 1);

    stage_dirty(&graph, 1, |id| id % 2 == 0);
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    let parts = manifest(&store).node_parts;
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].rows, 2000);
    // The replaced part is retired: only the published file remains.
    assert_eq!(store.vfs.list_dir("nodes").unwrap().len(), 1);
    // Copied and encoded rows share row groups; no group per run.
    let groups = row_group_rows(&store, &parts[0].path);
    assert_eq!(groups.len(), 2000usize.div_ceil(ROW_GROUP_CAP));
    assert!(groups.iter().all(|rows| *rows <= ROW_GROUP_CAP as i64));
    assert_reopens_as(&store, &graph);
}

#[test]
fn new_window_and_deleted_row_join_one_unit() {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, 100, 24);
    let ts = apply_commit(&graph);
    // 1 MiB over the 256-byte density floor keeps windows at 4,096 ids.
    let target = 1 << 20;
    checkpoint_at(&store, &graph, 0, ts, target).unwrap();
    assert_eq!(manifest(&store).node_parts.len(), 1);

    graph.begin_write().unwrap();
    graph.delete_node(50, false).unwrap();
    graph.set_next_node_id(5000);
    graph.create_node(vec!["Item".into()], text(24)).unwrap();
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, target).unwrap();
    let parts = manifest(&store).node_parts;
    assert_eq!(parts.len(), 1, "{parts:?}");
    assert_eq!((parts[0].id_lo, parts[0].id_hi), (0, 8192));
    assert_eq!(parts[0].rows, 100);
    assert_reopens_as(&store, &graph);
}

#[test]
fn repeated_interleaved_checkpoints_do_not_fragment() {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, 2000, 24);
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    let rounds: [(i64, &dyn Fn(i64) -> bool); 5] = [
        (1, &|id| id % 2 == 0),
        (2, &|id| id % 3 == 0),
        (3, &|id| id % 5 == 0),
        (4, &|id| id % 7 == 0),
        (5, &|id| id == 1001),
    ];
    for (round, pick) in rounds {
        stage_dirty(&graph, round, pick);
        let ts = apply_commit(&graph);
        checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
        assert_eq!(manifest(&store).node_parts.len(), 1, "round {round}");
        assert_eq!(store.vfs.list_dir("nodes").unwrap().len(), 1);
    }
    assert_reopens_as(&store, &graph);
}

#[test]
fn interleaved_dirty_clean_rels_stay_one_shard() {
    let store = memory_store();
    let graph = Graph::default();
    graph.begin_write().unwrap();
    for _ in 0..100 {
        graph
            .create_node(vec!["N".into()], Default::default())
            .unwrap();
    }
    for i in 0..2000 {
        graph
            .create_rel(i % 100, (i * 7 + 1) % 100, "LINK".into(), text(24))
            .unwrap();
    }
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    assert_eq!(manifest(&store).rel_parts.len(), 1);

    let ids = graph.all_rel_ids();
    graph.begin_write().unwrap();
    for id in ids.into_iter().filter(|id| id % 2 == 0) {
        graph.set_property_rel(id, "round", Value::Int(1));
    }
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    let parts = manifest(&store).rel_parts;
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].rows, 2000);
    assert_reopens_as(&store, &graph);
}

#[test]
fn fragmented_parts_heal_on_next_write() {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, 60, 24);
    let ts = apply_commit(&graph);
    // Target 1: no two rows fit one unit, so every row is its own part.
    checkpoint_at(&store, &graph, 0, ts, 1).unwrap();
    assert_eq!(manifest(&store).node_parts.len(), 60);

    stage_dirty(&graph, 1, |id| id == 30);
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    let parts = assert_healed(&store, &graph);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].rows, 60);
}

#[test]
fn clean_rows_are_copied_not_reencoded() {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, 2000, 24);
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();

    stage_dirty(&graph, 1, |id| id == 1000);
    let ts = apply_commit(&graph);
    take_encoded_rows();
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    assert_eq!(take_encoded_rows(), 1);
    assert_eq!(manifest(&store).node_parts.len(), 1);
    assert_reopens_as(&store, &graph);
}

#[test]
fn fitting_unit_measures_each_row_once() {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, 2000, 24);
    let ts = apply_commit(&graph);
    take_measured_rows();
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    assert_eq!(take_measured_rows(), 2000);

    // Planning measures every row once; the unit fits, so its one file is
    // written without a second walk.
    stage_dirty(&graph, 1, |id| id % 2 == 0);
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    assert_eq!(take_measured_rows(), 2000);
    assert_eq!(manifest(&store).node_parts.len(), 1);
    assert_reopens_as(&store, &graph);
}

#[test]
fn coalesce_respects_window_width() {
    let store = memory_store();
    let graph = Graph::default();
    // A 1 MiB target over the 256-byte density floor keeps windows at 4,096
    // ids. Groups at 0, 995,328 and 1,000,000 land in windows starting at 0,
    // 995,328 and 999,424: the first two span 999,424 ids together, all
    // three 1,003,520, more than MAX_WINDOW.
    graph.begin_write().unwrap();
    for start in [0, 995_328, 1_000_000] {
        graph.set_next_node_id(start);
        for _ in 0..50 {
            graph.create_node(vec!["Item".into()], text(24)).unwrap();
        }
    }
    let ts = apply_commit(&graph);
    let target = 1 << 20;
    checkpoint_at(&store, &graph, 0, ts, target).unwrap();

    stage_dirty(&graph, 1, |id| id % 2 == 0);
    let ts = apply_commit(&graph);
    take_measured_rows();
    checkpoint_at(&store, &graph, 0, ts, target).unwrap();
    // The width is checked before a neighbor's rows are measured.
    assert_eq!(take_measured_rows(), 150);
    let parts = manifest(&store).node_parts;
    assert!(
        parts.iter().all(|p| p.id_hi - p.id_lo <= MAX_WINDOW),
        "{parts:?}"
    );
    assert_eq!(ranges(&parts), [(0, 999_424), (999_424, 1_003_520)]);
    assert_eq!((parts[0].rows, parts[1].rows), (100, 50));
    assert_reopens_as(&store, &graph);
}

#[test]
fn coalesce_respects_byte_target() {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, 200, 1000);
    let ts = apply_commit(&graph);
    let row = decoded(&graph, ts, &[0]);
    assert!((1000..1100).contains(&row), "{row}");
    checkpoint_at(&store, &graph, 0, ts, 1).unwrap();
    assert_eq!(manifest(&store).node_parts.len(), 200);

    // Every other one-row fragment is dirty; units absorb fragments up to
    // the target.
    let target = 10 * 1024;
    stage_dirty(&graph, 1, |id| id % 2 == 0);
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, target).unwrap();
    let parts = manifest(&store).node_parts;
    let bytes = |lo: i64, hi: i64| -> u64 {
        let ids: Vec<i64> = graph
            .all_node_ids()
            .into_iter()
            .filter(|id| (lo..hi).contains(id))
            .collect();
        decoded(&graph, ts, &ids)
    };
    for part in &parts {
        assert!(
            bytes(part.id_lo, part.id_hi) <= target || part.rows == 1,
            "{part:?}"
        );
    }
    let total = bytes(0, i64::MAX);
    assert!(parts.len() > 1);
    assert!(
        parts.len() as u64 <= 2 * total.div_ceil(target) + 1,
        "{} parts for {total} decoded bytes",
        parts.len()
    );
    assert_reopens_as(&store, &graph);
}

#[test]
fn mixed_file_envelope_retry_recopies_behind_cursor() {
    // Every other row is dirty, so each file alternates copied and encoded
    // runs out of the one previous shard, and the whole unit misses the file
    // ceiling. A row group flushes into the sink on its 64th row, an odd id:
    // with the even ids dirty the overflow surfaces in a copy, whose failed
    // run consumes the cursor; with the odd ids dirty it surfaces in an
    // encode, and the cursor held past the first half's start is dropped
    // when the file is halved.
    for dirty_parity in [0, 1] {
        let store = memory_store();
        let graph = Graph::default();
        stage_nodes(&graph, 2000, 24);
        let ts = apply_commit(&graph);
        checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
        let [part] = &manifest(&store).node_parts[..] else {
            panic!("expected one node part");
        };
        let ceiling = part.bytes * 6 / 10;

        stage_dirty(&graph, 1, |id| id % 2 == dirty_parity);
        let ts = apply_commit(&graph);
        let (watched, opens) = watch_opens(&store, &part.path);
        with_test_file_ceiling(ceiling, || checkpoint_at(&watched, &graph, 0, ts, MAX_FILE))
            .unwrap();
        let parts = manifest(&store).node_parts;
        assert!(parts.len() > 1, "{parts:?}");
        assert!(parts.iter().all(|p| p.bytes <= ceiling), "{parts:?}");
        assert_eq!(parts.iter().map(|p| p.rows).sum::<u64>(), 2000);
        // The failed file opened the shard. The first half reopens it; the
        // second half starts ahead of the first half's cursor and reuses it.
        assert_eq!(opens.load(Ordering::SeqCst), 2, "parity {dirty_parity}");
        assert_reopens_as(&store, &graph);
    }
}

#[test]
fn overlapping_legacy_parts_heal_on_next_checkpoint() {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, 10, 24);
    graph.set_next_node_id(2_000_000);
    graph.create_node(vec!["Item".into()], text(24)).unwrap();
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    let mut legacy = manifest(&store);
    assert_eq!(
        ranges(&legacy.node_parts),
        [(0, 1_000_000), (2_000_000, 3_000_000)]
    );

    // A v0.4.0–v0.11.x store can hold parts that overlap and both hold an
    // id: [4, 8) holds node 5 again, inside [0, 1_000_000).
    add_legacy_part(&store, &mut legacy, &graph, ts, 4..8, [5]);
    let opened = assert_reopens_as(&store, &graph);

    // A write far from the overlap still heals it: node 5 is held once.
    opened.begin_write().unwrap();
    opened.set_property_node(2_000_000, "round", Value::Int(1));
    let ts = apply_commit(&opened);
    checkpoint_at(&store, &opened, 0, ts, MAX_FILE).unwrap();
    assert_healed(&store, &opened);
}

#[test]
fn overlap_chain_of_three_heals() {
    // [0, 8) and [10, 20) do not overlap each other, only [4, 12), which
    // holds ids 4..8 and 10..12 again.
    let (store, opened) = legacy_layout(20, [0..8, 4..12, 10..20]);

    stage_dirty(&opened, 1, |id| id == 15);
    let ts = apply_commit(&opened);
    checkpoint_at(&store, &opened, 0, ts, MAX_FILE).unwrap();
    let parts = assert_healed(&store, &opened);
    assert_eq!(ranges(&parts), [(0, 20)]);
    assert_eq!(parts[0].rows, 20);
}

#[test]
fn overlap_group_over_target_heals() {
    let store = memory_store();
    let graph = Graph::default();
    graph.begin_write().unwrap();
    for start in [0, 1_500_000] {
        graph.set_next_node_id(start);
        for _ in 0..50 {
            graph.create_node(vec!["Item".into()], text(200)).unwrap();
        }
    }
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    // [40, 1_500_010) overlaps both windows, so the group spans 2,000,000
    // ids, more than MAX_WINDOW, and holds more than the target.
    let high = 1_500_000;
    let mut legacy = without_node_parts(&store);
    add_legacy_part(&store, &mut legacy, &graph, ts, 0..1_000_000, 0..50);
    let ids = (40..50).chain(high..high + 10);
    add_legacy_part(&store, &mut legacy, &graph, ts, 40..high + 10, ids);
    let ids = high..high + 50;
    add_legacy_part(&store, &mut legacy, &graph, ts, 1_000_000..2_000_000, ids);
    let opened = assert_reopens_as(&store, &graph);

    stage_dirty(&opened, 1, |id| id == high + 20);
    let ts = apply_commit(&opened);
    let target = 4 * 1024;
    let all = decoded(&opened, ts, &opened.all_node_ids());
    assert!(all > 4 * target, "{all}");
    checkpoint_at(&store, &opened, 0, ts, target).unwrap();
    // The window is cut by bytes into files that tile its range; the file
    // across the gap spans more than MAX_WINDOW ids.
    assert_healed(&store, &opened);
    let parts = assert_tiles(&store, &opened, ts, 0..2_000_000, target);
    assert!(parts.len() > 1, "{parts:?}");
    assert!(parts.iter().any(|p| p.id_hi - p.id_lo > MAX_WINDOW));
}

#[test]
fn healed_group_keeps_neighbour_copy() {
    // [4, 8) and [6, 10) overlap; [0, 4) and [10, 20) are clean neighbours.
    let (store, opened) = legacy_layout(20, [0..4, 4..8, 6..10, 10..20]);

    stage_dirty(&opened, 1, |id| id == 5);
    let ts = apply_commit(&opened);
    take_encoded_rows();
    checkpoint_at(&store, &opened, 0, ts, MAX_FILE).unwrap();
    // The healed window [4, 10) encodes its six live rows; the neighbours
    // it absorbs are copied from their previous shards.
    assert_eq!(take_encoded_rows(), 6);
    let parts = assert_healed(&store, &opened);
    assert_eq!(ranges(&parts), [(0, 20)]);
    assert_eq!(parts[0].rows, 20);
}

#[test]
fn over_target_seed_copies_clean_rows_across_files() {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, 2000, 24);
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    let [part] = &manifest(&store).node_parts[..] else {
        panic!("expected one node part");
    };

    let dirty = [10, 1000, 1990];
    stage_dirty(&graph, 1, |id| dirty.contains(&id));
    let ts = apply_commit(&graph);
    let target = 10 * 1024;
    let (watched, opens) = watch_opens(&store, &part.path);
    take_measured_rows();
    take_encoded_rows();
    checkpoint_at(&watched, &graph, 0, ts, target).unwrap();
    // One walk measures each row once and cuts the seed into files; the
    // clean rows of every file are copied through one cursor.
    assert_eq!(take_measured_rows(), 2000);
    assert_eq!(take_encoded_rows(), dirty.len());
    assert_eq!(opens.load(Ordering::SeqCst), 1);
    let parts = assert_tiles(&store, &graph, ts, part.id_lo..part.id_hi, target);
    assert!(parts.len() > 1, "{parts:?}");
    assert_eq!(parts.iter().map(|p| p.rows).sum::<u64>(), 2000);
    assert_reopens_as(&store, &graph);
}

#[test]
fn neighbour_over_row_envelope_is_kept() {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, 3, 24);
    graph.create_node(vec!["Item".into()], text(2000)).unwrap();
    let ts = apply_commit(&graph);
    // Target 1: every row is its own part.
    checkpoint_at(&store, &graph, 0, ts, 1).unwrap();
    let before = manifest(&store).node_parts;
    assert_eq!(before.len(), 4);

    stage_dirty(&graph, 1, |id| id == 2);
    let ts = apply_commit(&graph);
    // Node 3's row passes the lowered row envelope: measured as the seed's
    // right neighbour, it does not fit, and its part stays as it is.
    with_test_envelope(128 << 20, 1000, 100_000, || {
        checkpoint_at(&store, &graph, 0, ts, MAX_FILE)
    })
    .unwrap();
    let parts = manifest(&store).node_parts;
    assert_eq!(ranges(&parts)[0], (0, 3));
    assert_eq!(parts[0].rows, 3);
    assert_eq!(parts[1..], before[3..]);
    assert_reopens_as(&store, &graph);
}

#[test]
fn file_without_rows_is_refused() {
    // Ids the graph does not hold cost nothing to plan and encode no row;
    // publishing that file would leave a manifest read_manifest rejects.
    // Even an empty file is over a 1-byte target, so the refusal naming the
    // whole planned file shows it ran before any halving.
    let store = memory_store();
    let graph = Graph::default();
    let mut owned = Vec::new();
    let mut ctx = ShardWrite {
        target: &store,
        graph: &graph,
        ts: 1,
        kind: Kind::Node,
        target_bytes: 1,
        owned: &mut owned,
    };
    // Ids 7 and 8 get a new window [0, 4096), with no previous file.
    let err = coalesce::rewrite(&mut ctx, Vec::new(), &[7, 8], &[], 4096).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    let refused = "Parquet shard [0, 4096) would hold no rows";
    assert!(err.to_string().contains(refused), "{err}");
    assert!(owned.is_empty());
}

#[cfg(unix)]
#[test]
fn native_store_holds_only_published_node_files() {
    use crate::{parse_uri, Wal};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("db");
    let store = parse_uri(root.to_str().unwrap()).unwrap();
    let (vfs, prefix) = store.wal_vfs();
    let mut wal = Wal::open(vfs, prefix, 0).unwrap();
    let graph = Graph::default();
    stage_nodes(&graph, 60, 24);
    commit_open_write(&graph, &mut wal);
    let files = || std::fs::read_dir(root.join("nodes")).unwrap().count();
    checkpoint_at(&store, &graph, wal.lsn, graph.commit_ts(), 1).unwrap();
    assert_eq!(files(), manifest(&store).node_parts.len());
    assert_eq!(files(), 60);

    stage_dirty(&graph, 1, |id| id % 2 == 0);
    commit_open_write(&graph, &mut wal);
    checkpoint_at(&store, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
    let parts = manifest(&store).node_parts;
    assert_eq!(parts.len(), 1);
    assert_eq!(files(), parts.len());
    assert!(root.join(&parts[0].path).is_file());
}
