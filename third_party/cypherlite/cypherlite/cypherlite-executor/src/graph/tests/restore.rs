use super::*;
use crate::graph::index::take_adj_builds;
use crate::graph::test_support::endpoints;
use std::sync::Arc;

#[test]
fn direct_restore_rejects_sentinel_before_mutating() {
    let graph = Graph::default();
    assert!(matches!(
        graph.load_checkpoint_node(Node {
            id: i64::MAX,
            labels: Arc::new(vec![]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new())
        }),
        Err(cypherlite_core::Error::Storage(_))
    ));
    assert!(matches!(
        graph.load_checkpoint_rel(Relationship {
            id: i64::MAX,
            start: 0,
            end: 0,
            rel_type: "R".into(),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new())
        }),
        Err(cypherlite_core::Error::Storage(_))
    ));
    assert!(matches!(
        graph.load_checkpoint_rel(Relationship {
            id: 0,
            start: i64::MAX,
            end: 0,
            rel_type: "R".into(),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new())
        }),
        Err(cypherlite_core::Error::Storage(_))
    ));
    assert_eq!(graph.next_node_id(), 0);
    assert_eq!(graph.next_rel_id(), 0);
    assert_eq!(graph.live_node_count(), 0);
}

fn restored_node(id: NodeId, label: &str, name: &str) -> Node {
    Node {
        id,
        labels: Arc::new(vec![label.into()]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
            "name".to_string(),
            Value::String(name.into()),
        )])),
    }
}

fn restored_rel(start: NodeId, end: NodeId, rel_type: &str, ts: i64) -> Relationship {
    Relationship {
        id: 0,
        start,
        end,
        rel_type: rel_type.into(),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
            "ts".to_string(),
            Value::Int(ts),
        )])),
    }
}

/// Overlapping legacy parts (ADR-0183) can restore an id twice; the later
/// row replaces the earlier one, so a delete leaves no copy visible.
#[test]
fn restoring_a_node_twice_keeps_one_version_of_the_later_row() {
    let g = Graph::default();
    g.load_checkpoint_node(restored_node(3, "Old", "first"))
        .unwrap();
    g.load_checkpoint_node(restored_node(3, "New", "second"))
        .unwrap();
    g.set_commit_ts(1);
    g.finish_restore();
    assert_eq!(g.nodes.get(&3).unwrap().len(), 1);
    let node = g.node(3).unwrap();
    assert_eq!(node.labels.as_slice(), ["New".to_string()].as_slice());
    assert_eq!(
        node.properties,
        restored_node(3, "New", "second").properties
    );
    let no_props = BTreeMap::new();
    assert_eq!(g.lookup_nodes(&["Old".into()], &no_props), Some(vec![]));
    assert_eq!(g.lookup_nodes(&["New".into()], &no_props), Some(vec![3]));

    g.begin_write().unwrap();
    g.delete_node(3, false).unwrap();
    g.commit_write();
    assert!(g.node(3).is_none());
    assert!(g.all_node_ids().is_empty());
}

#[test]
fn restoring_a_rel_twice_keeps_one_version_and_one_hop_per_endpoint() {
    use cypherlite_ir::Direction;

    let g = Graph::default();
    for id in 0..4 {
        g.load_checkpoint_node(restored_node(id, "N", "n")).unwrap();
    }
    // Same endpoints, but a different `ts` sorts the hop elsewhere.
    g.load_checkpoint_rel(restored_rel(0, 1, "R", 1)).unwrap();
    g.load_checkpoint_rel(restored_rel(0, 1, "R", 2)).unwrap();
    assert_eq!(g.rels.get(&0).unwrap().len(), 1);
    // Restore leaves adjacency to `finish_restore`.
    assert!(g.degree_out(0).is_empty());
    g.set_commit_ts(1);
    g.finish_restore();
    assert_eq!(g.degree_out(0), [0]);
    assert_eq!(g.degree_in(1), [0]);
    // Other endpoints and type: the later row wins entirely.
    g.load_checkpoint_rel(restored_rel(2, 3, "S", 3)).unwrap();
    g.finish_restore();
    assert_eq!(g.rels.get(&0).unwrap().len(), 1);
    let rel = g.rel(0).unwrap();
    assert_eq!((rel.start, rel.end, rel.rel_type.as_str()), (2, 3, "S"));
    assert_eq!(rel.properties, restored_rel(2, 3, "S", 3).properties);
    assert!(g.degree_out(0).is_empty());
    assert!(g.degree_in(1).is_empty());
    assert_eq!(g.degree_out(2), [0]);
    assert_eq!(g.degree_in(3), [0]);
    assert!(g.neighbors(0, Direction::Both).is_empty());
    assert_eq!(g.neighbors(2, Direction::Both), [0]);

    g.begin_write().unwrap();
    g.delete_rel(0);
    g.commit_write();
    assert!(g.rel(0).is_none());
    assert!(g.all_rel_ids().is_empty());
    assert!(g.neighbors(2, Direction::Both).is_empty());
    assert!(g.neighbors(3, Direction::Both).is_empty());
}

/// Rows restored out of id order still give each node the adjacency of an
/// id-ordered load: type chunks in first-id order, hops `ts` DESC.
#[test]
fn restored_adjacency_follows_id_order_not_load_order() {
    let g = Graph::default();
    for id in 0..2 {
        g.load_checkpoint_node(restored_node(id, "N", "n")).unwrap();
    }
    for (id, rel_type, ts) in [(3, "B", 9), (1, "A", 5), (2, "B", 1), (0, "A", 7)] {
        g.load_checkpoint_rel(Relationship {
            id,
            ..restored_rel(0, 1, rel_type, ts)
        })
        .unwrap();
    }
    g.set_commit_ts(1);
    g.finish_restore();
    assert_eq!(g.degree_out(0), [0, 1, 3, 2]);
    assert_eq!(g.degree_in(1), [0, 1, 3, 2]);
}

#[test]
fn restore_during_an_open_write_is_a_storage_error() {
    let g = Graph::default();
    g.load_checkpoint_node(restored_node(0, "N", "n")).unwrap();
    g.set_commit_ts(1);
    g.finish_restore();
    g.begin_write().unwrap();
    let written = g.create_node(vec!["W".into()], BTreeMap::new()).unwrap();

    for result in [
        g.load_checkpoint_node(restored_node(written, "Other", "x")),
        g.load_checkpoint_node(restored_node(7, "Other", "x")),
        g.load_checkpoint_rel(restored_rel(0, written, "R", 1)),
    ] {
        assert!(
            matches!(result, Err(cypherlite_core::Error::Storage(ref message)) if message.contains("write is open")),
            "{result:?}"
        );
    }
    assert_eq!(g.next_node_id(), written + 1);
    assert_eq!(g.next_rel_id(), 0);
    assert!(g.rels.is_empty() && !g.nodes.contains_key(&7));
    assert_eq!(g.nodes.get(&written).unwrap().len(), 1);
    assert_eq!(
        g.node(written).unwrap().labels.as_slice(),
        ["W".to_string()].as_slice()
    );
    g.commit_write();
    assert_eq!(g.all_node_ids(), [0, written]);
}

#[test]
fn restore_over_a_committed_write_is_a_storage_error() {
    let g = Graph::default();
    g.load_checkpoint_node(restored_node(0, "N", "first"))
        .unwrap();
    g.set_commit_ts(1);
    g.finish_restore();
    g.begin_write().unwrap();
    g.set_property_node(0, "name", Value::String("second".into()));
    g.commit_write();
    assert_eq!(g.nodes.get(&0).unwrap().len(), 2);

    assert!(matches!(
        g.load_checkpoint_node(restored_node(0, "N", "third")),
        Err(cypherlite_core::Error::Storage(_))
    ));
    assert_eq!(g.nodes.get(&0).unwrap().len(), 2);
    assert_eq!(
        g.node(0).unwrap().properties,
        restored_node(0, "N", "second").properties
    );
}

/// Every live node's adjacency, raw and as each direction expands it.
type NodeAdjacency = (NodeId, Vec<RelId>, Vec<RelId>, [Vec<RelId>; 3]);

fn adjacency(g: &Graph) -> Vec<NodeAdjacency> {
    use cypherlite_ir::Direction;

    let ts = g.commit_ts();
    g.all_node_ids_at(ts)
        .into_iter()
        .map(|id| {
            let expands = [Direction::Outgoing, Direction::Incoming, Direction::Both]
                .map(|dir| g.neighbors_at(id, dir, ts));
            (id, g.degree_out(id), g.degree_in(id), expands)
        })
        .collect()
}

fn create_rel(id: RelId, start: NodeId, end: NodeId, rel_type: &str) -> RedoOp {
    RedoOp::CreateRel {
        id,
        start,
        end,
        rel_type: rel_type.into(),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    }
}

/// The calls a store open makes — restore the checkpoint rows,
/// `finish_restore`, replay the WAL tail, `rebuild_indexes` — build
/// adjacency once, and the opened graph has the writer's adjacency.
#[test]
fn checkpoint_load_and_wal_replay_build_adjacency_once() {
    let checkpointed = vec![
        RedoOp::CreateNode {
            id: 0,
            labels: Arc::new(vec!["N".into()]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        },
        RedoOp::CreateNode {
            id: 1,
            labels: Arc::new(vec!["N".into()]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        },
        RedoOp::CreateNode {
            id: 2,
            labels: Arc::new(vec![]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        },
        create_rel(0, 0, 1, "A"),
        create_rel(1, 0, 2, "B"),
        create_rel(2, 1, 2, "A"),
    ];
    let tail = vec![
        vec![create_rel(3, 0, 2, "A"), RedoOp::DeleteRel { id: 0 }],
        vec![
            create_rel(4, 2, 0, "B"),
            RedoOp::SetRelProp {
                id: 2,
                key: "ts".into(),
                value: Value::Int(5),
            },
        ],
        vec![RedoOp::DeleteRel { id: 4 }],
    ];

    // The writer commits the checkpointed rows, then the tail.
    let writer = Graph::default();
    writer.apply_redo(checkpointed, 1).unwrap();
    let nodes: Vec<Node> = writer
        .all_node_ids_at(1)
        .into_iter()
        .map(|id| writer.node_at(id, 1).unwrap())
        .collect();
    let rels: Vec<Relationship> = writer
        .all_rel_ids_at(1)
        .into_iter()
        .map(|id| writer.rel_at(id, 1).unwrap())
        .collect();
    for (ops, ts) in tail.iter().cloned().zip(2..) {
        writer.apply_redo(ops, ts).unwrap();
    }

    take_adj_builds();
    let opened = Graph::default();
    opened.set_commit_ts(1);
    for node in nodes {
        opened.load_checkpoint_node(node).unwrap();
    }
    for rel in rels {
        opened.load_checkpoint_rel(rel).unwrap();
    }
    opened.finish_restore();
    for (ops, ts) in tail.into_iter().zip(2..) {
        opened.apply_replayed_redo(ops, ts).unwrap();
    }
    opened.rebuild_indexes();
    assert_eq!(take_adj_builds(), 1);

    assert_eq!(opened.commit_ts(), writer.commit_ts());
    assert_eq!(adjacency(&opened), adjacency(&writer));
    // Relationships the tail deleted expand nowhere.
    for (id, _, _, expands) in adjacency(&opened) {
        for rels in expands {
            assert!(!rels.contains(&0) && !rels.contains(&4), "{id}: {rels:?}");
        }
    }
}

/// An aborted write drops its relationships from adjacency: `abort_write`
/// still rebuilds it from the live versions.
#[test]
fn abort_write_rebuilds_adjacency() {
    let g = Graph::default();
    let (a, b) = endpoints(&g);
    g.begin_write().unwrap();
    let kept = g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    g.begin_write().unwrap();
    g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    take_adj_builds();
    g.abort_write();
    assert_eq!(take_adj_builds(), 1);
    assert_eq!(g.degree_out(a), [kept]);
    assert_eq!(g.degree_in(b), [kept]);
}
