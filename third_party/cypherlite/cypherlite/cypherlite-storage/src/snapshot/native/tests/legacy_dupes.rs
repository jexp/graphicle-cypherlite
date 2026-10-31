//! Overlapping legacy parts (ADR-0183) can both hold an id. Open restores
//! it as one version, so a delete before the next checkpoint leaves no
//! copy behind for that checkpoint's overlap heal to persist.

use super::super::*;
use super::harness::checkpoint_at;
use super::harness::*;
use crate::memory_store;
use cypherlite_executor::Graph;

#[test]
fn node_deleted_from_overlapping_parts_stays_deleted() {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, 10, 24);
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    let mut legacy = manifest(&store);
    assert_eq!(legacy.node_parts.len(), 1);
    assert!((legacy.node_parts[0].id_lo..legacy.node_parts[0].id_hi).contains(&5));

    // [4, 8) holds node 5 again, inside the first part.
    add_legacy_part(&store, &mut legacy, &graph, ts, 4..8, [5]);

    let opened = assert_reopens_as(&store, &graph);
    assert!(opened.node(5).is_some());
    opened.begin_write().unwrap();
    opened.delete_node(5, false).unwrap();
    let ts = apply_commit(&opened);
    assert!(opened.node(5).is_none());
    checkpoint_at(&store, &opened, 0, ts, MAX_FILE).unwrap();

    // The heal holds no copy of node 5, so the store reopens without it.
    assert_healed(&store, &opened);
    assert_eq!(opened.all_node_ids(), [0, 1, 2, 3, 4, 6, 7, 8, 9]);
}

#[test]
fn legacy_part_listed_twice_heals_and_retires_its_file() {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, 10, 24);
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    // A v0.4.0–v0.9.x manifest could list one fixed-name part twice.
    let mut legacy = manifest(&store);
    let [part] = &legacy.node_parts[..] else {
        panic!("expected one node part");
    };
    delete_object(&store, &part.path).unwrap();
    let fixed = ShardPart {
        path: ShardName::fixed(Kind::Node, part.id_lo, part.id_hi).path(),
        ..part.clone()
    };
    let bytes = nodes_parquet_ids(&graph, ts, &graph.all_node_ids_at(ts)).unwrap();
    put_object(&store, &fixed.path, &bytes).unwrap();
    legacy.node_parts = vec![fixed.clone(), fixed.clone()];
    put_manifest(&store, &legacy);

    // Open restores one version of each id the file holds twice over.
    let opened = assert_reopens_as(&store, &graph);
    assert_eq!(opened.all_node_ids(), (0..10).collect::<Vec<_>>());
    assert_eq!(opened.live_node_count(), 10);

    // A write anywhere heals the pair into disjoint parts that hold each
    // id once, and retires the file both entries named.
    stage_dirty(&opened, 1, |id| id == 7);
    let ts = apply_commit(&opened);
    checkpoint_at(&store, &opened, 0, ts, MAX_FILE).unwrap();
    let parts = assert_healed(&store, &opened);
    assert_eq!(parts.iter().map(|p| p.rows).sum::<u64>(), 10);
    assert!(parts.iter().all(|p| p.path != fixed.path));
    assert!(!object_exists(&store, &fixed.path).unwrap());
}
