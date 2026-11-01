//! ADR-0171: the graph reads behind the label probe.

use std::collections::BTreeMap;

use crate::graph::Graph;

#[test]
fn label_index_copy_reports_whole_sets() {
    let g = Graph::default();
    for _ in 0..200 {
        g.create_node(vec!["L".into()], BTreeMap::new()).unwrap();
    }
    let lid = g.label_id("L").unwrap();
    let (ids, whole) = g.label_index_copy(lid, 64);
    assert_eq!((ids.len(), whole), (64, false));
    let (ids, whole) = g.label_index_copy(lid, 256);
    assert_eq!((ids.len(), whole), (200, true));
    assert_eq!(g.label_index_copy(u32::MAX, 64), (Vec::new(), true));
    assert_eq!(g.label_id("Never"), None);
}
