//! A store opened from a checkpoint plus a WAL tail has the adjacency of
//! the graph that wrote it: the checkpoint load builds adjacency once and
//! the replayed tail keeps it current. Checked for every compiled snapshot
//! codec, through the read-write open and the read-only open a follower
//! reloads through.

use std::collections::BTreeMap;

use cypherlite_core::{NodeId, RelId, Value};
use cypherlite_executor::{Graph, RedoOp};
use cypherlite_storage::{
    checkpoint_with_lsn, memory_store_with_codec, open_store, open_store_readonly,
    SnapshotCodecKind, Wal,
};

fn create_node(id: NodeId) -> RedoOp {
    RedoOp::create_node(id, vec!["N".into()], BTreeMap::new())
}

fn create_rel(id: RelId, start: NodeId, end: NodeId, rel_type: &str) -> RedoOp {
    RedoOp::create_rel(id, start, end, rel_type.into(), BTreeMap::new())
}

/// Log `ops` and apply them to `writer`, as a database commit does.
fn commit(writer: &Graph, wal: &mut Wal, ops: Vec<RedoOp>) {
    let ts = writer.commit_ts() + 1;
    wal.append_commit(ts, &ops).unwrap();
    writer.apply_redo(ops, ts).unwrap();
}

/// Each live node's outgoing and incoming hops, as stored and as an
/// expansion at the commit timestamp sees them.
fn adjacency(g: &Graph) -> Vec<(NodeId, [Vec<RelId>; 4])> {
    let ts = g.commit_ts();
    let visible = |hops: &[RelId]| -> Vec<RelId> {
        hops.iter()
            .copied()
            .filter(|id| g.rel_at(*id, ts).is_some())
            .collect()
    };
    g.all_node_ids_at(ts)
        .into_iter()
        .map(|id| {
            let (out, inc) = (g.degree_out(id), g.degree_in(id));
            (id, [visible(&out), visible(&inc), out, inc])
        })
        .collect()
}

/// The live nodes whose stored outgoing hops, and whose stored incoming
/// hops, list relationship `rel`, a node once per listing.
fn hop_holders(g: &Graph, rel: RelId) -> [Vec<NodeId>; 2] {
    let nodes = g.all_node_ids_at(g.commit_ts());
    let held = |hops: fn(&Graph, NodeId) -> Vec<RelId>| {
        let mut held = Vec::new();
        for &node in &nodes {
            held.extend(
                hops(g, node)
                    .into_iter()
                    .filter(|r| *r == rel)
                    .map(|_| node),
            );
        }
        held
    };
    [held(Graph::degree_out), held(Graph::degree_in)]
}

#[test]
fn opened_store_has_the_adjacency_of_the_graph_that_wrote_it() {
    for &codec in SnapshotCodecKind::compiled() {
        let store = memory_store_with_codec(codec).unwrap();
        let mut wal = Wal::open(store.vfs(), "wal", 0).unwrap();
        let writer = Graph::default();
        commit(
            &writer,
            &mut wal,
            vec![
                create_node(0),
                create_node(1),
                create_node(2),
                create_rel(0, 0, 1, "A"),
                create_rel(1, 0, 2, "B"),
                create_rel(2, 1, 2, "A"),
            ],
        );
        checkpoint_with_lsn(&store, &writer, wal.lsn, None).unwrap();
        // The WAL tail: a delete of a checkpointed relationship, a moved
        // hop, and a relationship created and deleted after the checkpoint.
        commit(
            &writer,
            &mut wal,
            vec![create_rel(3, 0, 2, "A"), RedoOp::delete_rel(0)],
        );
        commit(
            &writer,
            &mut wal,
            vec![
                create_rel(4, 2, 0, "B"),
                RedoOp::set_rel_prop(2, "ts".into(), Value::Int(5)),
            ],
        );
        commit(&writer, &mut wal, vec![RedoOp::delete_rel(4)]);
        drop(wal);

        let expected = adjacency(&writer);
        for open in [open_store, open_store_readonly] {
            let (opened, _) = open(&store).unwrap();
            assert_eq!(opened.commit_ts(), writer.commit_ts(), "{codec:?}");
            assert_eq!(adjacency(&opened), expected, "{codec:?}");
            // A delete leaves its hops stored, so a snapshot older than it
            // still expands them. The tail's deletes are stored as the
            // writer's own deletes leave them: one hop at each endpoint,
            // which no read at the commit timestamp sees.
            for (deleted, start, end) in [(0, 0, 1), (4, 2, 0)] {
                let holders = hop_holders(&opened, deleted);
                assert_eq!(holders, [[start], [end]], "{codec:?}: {deleted}");
                let hidden = opened.rel_at(deleted, opened.commit_ts()).is_none();
                assert!(hidden, "{codec:?}: {deleted}");
            }
            assert_eq!(opened.all_rel_ids(), [1, 2, 3], "{codec:?}");
        }
    }
}
