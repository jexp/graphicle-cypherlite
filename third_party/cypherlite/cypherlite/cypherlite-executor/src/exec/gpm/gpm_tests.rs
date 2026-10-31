use super::*;
use cypherlite_ir::{PathCount, PathSelector};
use std::sync::Arc;

fn test_node(id: NodeId) -> cypherlite_core::Node {
    cypherlite_core::Node {
        id,
        labels: Arc::new(Vec::new()),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    }
}

fn selector_state(start: NodeId, end: NodeId, length: usize) -> GpmState {
    GpmState {
        row: Row::new(),
        used: HashSet::new(),
        current: end,
        nodes: vec![test_node(start), test_node(end)],
        rels: (0..length)
            .map(|index| cypherlite_core::Relationship {
                id: index as RelId,
                start,
                end,
                rel_type: "R".into(),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            })
            .collect(),
    }
}

#[test]
fn gpm_shortest_groups_are_partitioned_by_endpoints() {
    let states = vec![
        selector_state(1, 2, 1),
        selector_state(1, 2, 2),
        selector_state(1, 2, 2),
        selector_state(3, 4, 3),
        selector_state(3, 4, 4),
    ];
    let selected = apply_gpm_selector(
        states,
        Some(&PathSelector::Shortest {
            count: Some(PathCount::Literal(2)),
            group: true,
        }),
        &BTreeMap::new(),
    )
    .unwrap();
    assert_eq!(
        selected
            .iter()
            .map(|state| state.rels.len())
            .collect::<Vec<_>>(),
        vec![1, 2, 2, 3, 4]
    );
}

#[test]
fn gpm_acyclic_and_simple_apply_distinct_node_rules() {
    let cycle = vec![test_node(1), test_node(2), test_node(1)];
    assert!(!gpm_path_mode_allows(Some(PathMode::Acyclic), &cycle, &[]));
    assert!(gpm_path_mode_allows(Some(PathMode::Simple), &cycle, &[]));
    let repeated_interior = vec![test_node(1), test_node(2), test_node(2), test_node(3)];
    assert!(!gpm_path_mode_allows(
        Some(PathMode::Simple),
        &repeated_interior,
        &[]
    ));
}
