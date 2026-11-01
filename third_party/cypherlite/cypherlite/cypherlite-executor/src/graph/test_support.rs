//! Test-only graph seeding helpers shared by the graph's and the overlay's
//! test modules. The exec-level `test_support` is for running Cypher;
//! helpers that touch only `Graph` APIs live here, beside the graph's own
//! `tests.rs`.

use std::collections::BTreeMap;

use cypherlite_core::NodeId;

use super::Graph;

/// Two committed `:N` endpoint nodes, shared by the relationship tests.
pub(crate) fn endpoints(g: &Graph) -> (NodeId, NodeId) {
    g.begin_write().unwrap();
    let a = g.create_node(vec!["N".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["N".into()], BTreeMap::new()).unwrap();
    g.commit_write();
    (a, b)
}
