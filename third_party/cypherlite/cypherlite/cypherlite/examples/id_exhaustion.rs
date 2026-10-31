use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use cypherlite::Error;
use cypherlite_executor::{ExecGraph, Graph, WriteScratch};

fn main() {
    let graph = Graph::default();
    let id = graph
        .create_node(vec!["N".into()], BTreeMap::new())
        .unwrap();
    graph.set_next_node_id(i64::MAX);
    graph.set_next_rel_id(i64::MAX);
    assert!(matches!(
        graph.create_node(vec!["N".into()], BTreeMap::new()),
        Err(Error::Execution(_))
    ));
    assert!(matches!(
        graph.create_rel(id, id, "R".into(), BTreeMap::new()),
        Err(Error::Execution(_))
    ));
    assert_eq!(graph.next_node_id(), i64::MAX);
    assert_eq!(graph.next_rel_id(), i64::MAX);
    assert!(graph.node(i64::MAX).is_none());
    assert!(graph.rel(i64::MAX).is_none());
    let scratch = RefCell::new(WriteScratch::new(
        graph.commit_ts(),
        Arc::new(AtomicU64::new(i64::MAX as u64)),
        Arc::new(AtomicU64::new(i64::MAX as u64)),
    ));
    let view = ExecGraph::write(&graph, &scratch);
    assert!(matches!(
        view.create_node(vec!["N".into()], BTreeMap::new()),
        Err(Error::Execution(_))
    ));
    assert!(matches!(
        view.create_rel(id, id, "R".into(), BTreeMap::new()),
        Err(Error::Execution(_))
    ));
    assert!(scratch.borrow_mut().take_redo().is_empty());

    let timestamps = Graph::default();
    timestamps.set_commit_ts(u64::MAX - 1);
    assert!(matches!(timestamps.begin_write(), Err(Error::Execution(_))));
    assert!(matches!(
        timestamps.begin_write_at(u64::MAX),
        Err(Error::Execution(_))
    ));
    assert_eq!(timestamps.commit_ts(), u64::MAX - 1);
    assert!(matches!(
        timestamps.apply_redo(vec![], u64::MAX),
        Err(Error::Storage(_))
    ));
    assert!(matches!(
        timestamps.apply_redo(vec![], u64::MAX - 1),
        Err(Error::Storage(_))
    ));
    let replay = Graph::default();
    replay.apply_redo(vec![], u64::MAX - 1).unwrap();
    assert_eq!(replay.commit_ts(), u64::MAX - 1);
    let direct = Graph::default();
    direct.begin_write_at(u64::MAX - 1).unwrap();
    direct.commit_write();
    assert_eq!(direct.commit_ts(), u64::MAX - 1);
}
