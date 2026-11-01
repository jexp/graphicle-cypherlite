//! The layout a checkpoint plans over: new shard windows (`cover_ids`)
//! never overlap existing parts, even when the window width has grown since
//! those parts were written, once `heal_overlaps` has replaced overlapping
//! legacy parts with one window.

use super::super::layout::{cover_ids, lay_out, Slot};
use super::super::*;
use super::harness::checkpoint_at;
use super::harness::*;
use crate::memory_store;
use cypherlite_executor::Graph;

/// The `[start, end)` id range of each of `slots`.
fn spans(slots: &[Slot]) -> Vec<(i64, i64)> {
    slots
        .iter()
        .map(|s| (s.span().start, s.span().end))
        .collect()
}

#[test]
fn cover_ids_clips_new_windows_to_gaps() {
    // Sorted, disjoint slots: cover_ids' precondition.
    let kept = Slot::Kept(node_part(8192, 12_288, 1, 1));
    let slots = vec![Slot::Window(0..4096), kept.clone()];
    let ids = [100, 5000, 5001, 9000, 13_000, 20_000];
    let covered = cover_ids(slots, &ids, 16_384);
    assert_eq!(
        spans(&covered),
        [
            (0, 4096),
            // [0, 16384) clipped between both neighbors; 5001 reuses it.
            (4096, 8192),
            (8192, 12_288),
            // [0, 16384) clipped on the left only.
            (12_288, 16_384),
            (16_384, 32_768),
        ]
    );
    // Only windows are added; the kept part stays as it is.
    assert_eq!(covered[2], kept);
    for (index, slot) in covered.iter().enumerate() {
        assert_eq!(matches!(slot, Slot::Window(_)), index != 2, "{slot:?}");
    }
}

#[test]
fn lay_out_heals_each_overlap_group_into_one_window() {
    // Unsorted legacy parts, which lay_out sorts once. [0, 16384) reaches
    // past [4, 8) inside it; [20000, 20010) and [20015, 20030) overlap only
    // through [20005, 20020). [16384, 20000) and [30000, 30010) touch or
    // miss every other part.
    let parts = vec![
        node_part(20_005, 20_020, 1, 1),
        node_part(16_384, 20_000, 1, 1),
        node_part(4, 8, 1, 1),
        node_part(30_000, 30_010, 1, 1),
        node_part(20_015, 20_030, 1, 1),
        node_part(0, 16_384, 1, 1),
        node_part(20_000, 20_010, 1, 1),
    ];
    let (healed, retired) = lay_out(parts.clone(), &[], 16_384);
    // Each group is one window with no previous file; the others are kept.
    assert_eq!(
        healed,
        [
            Slot::Window(0..16_384),
            Slot::Kept(parts[1].clone()),
            Slot::Window(20_000..20_030),
            Slot::Kept(parts[3].clone()),
        ]
    );
    // Only kept parts' paths are retired, never a window's.
    let group = [5, 2, 6, 0, 4].map(|i| parts[i].path.clone());
    assert_eq!(retired, group);

    // An id past a part a window replaced, as 9 is past [4, 8), lies inside
    // the window and needs no new one.
    let (covered, _) = lay_out(parts, &[5, 9, 20_025, 25_000], 16_384);
    assert_eq!(
        spans(&covered),
        [
            (0, 16_384),
            (16_384, 20_000),
            (20_000, 20_030),
            (20_030, 30_000),
            (30_000, 30_010),
        ]
    );
}

#[test]
fn growing_window_width_keeps_parts_disjoint() {
    // 4 MiB: the first checkpoint assumes 1024 bytes/row, so w = 4096.
    let target = 4 << 20;
    let store = memory_store();
    let graph = Graph::default();
    // Two ~3 MB windows: each fits the target, both together do not. The
    // identical strings dictionary-encode, so few bytes per row are stored.
    graph.begin_write().unwrap();
    for start in [0, 8192] {
        graph.set_next_node_id(start);
        for _ in 0..100 {
            graph
                .create_node(vec!["Item".into()], text(30_000))
                .unwrap();
        }
    }
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, target).unwrap();
    let first = manifest(&store);
    assert_eq!(ranges(&first.node_parts), [(0, 4096), (8192, 12_288)]);

    // The measured density widens the window past the right-hand part.
    let w = window_width(target, first.bytes_per_node);
    assert!(w > 8192, "w = {w}, bytes/node = {}", first.bytes_per_node);
    graph.begin_write().unwrap();
    graph.set_next_node_id(5000);
    graph
        .create_node(vec!["Item".into()], Default::default())
        .unwrap();
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, target).unwrap();

    let parts = manifest(&store).node_parts;
    for pair in parts.windows(2) {
        assert!(pair[0].id_hi <= pair[1].id_lo, "{:?}", ranges(&parts));
    }
    for id in graph.all_node_ids() {
        let owners = parts
            .iter()
            .filter(|p| p.id_lo <= id && id < p.id_hi)
            .count();
        assert_eq!(owners, 1, "id {id} in {:?}", ranges(&parts));
    }
    assert_eq!(parts.iter().map(|p| p.rows).sum::<u64>(), 201);
    let reopened = assert_reopens_as(&store, &graph);
    assert_eq!(reopened.live_node_count(), graph.live_node_count());
}
