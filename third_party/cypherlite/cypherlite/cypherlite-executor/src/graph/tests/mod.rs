use super::*;

mod pattern_stats;
mod restore;
mod services;

#[test]
fn schema_capture_never_waits_for_theater_while_holding_catalog() {
    use std::sync::{mpsc, Arc, Barrier};
    use std::time::Duration;

    let graph = Arc::new(Graph::default());
    // Repeatedly force capture to wait for theater while another thread
    // acquires catalog. Even on failure, release theater before joining:
    // the regression is reported as a timeout, not a hung test process.
    for _ in 0..32 {
        let theater = graph.theater.lock().unwrap();
        let arrived = Arc::new(Barrier::new(2));
        let capture = {
            let graph = Arc::clone(&graph);
            let arrived = Arc::clone(&arrived);
            std::thread::spawn(move || {
                graph.schema_snapshot_with_hook(|| {
                    arrived.wait();
                });
            })
        };
        arrived.wait();
        let (tx, rx) = mpsc::channel();
        let catalog = {
            let graph = Arc::clone(&graph);
            std::thread::spawn(move || tx.send(graph.named_constraints()).unwrap())
        };
        let progressed = rx.recv_timeout(Duration::from_secs(2)).is_ok();
        drop(theater);
        capture.join().unwrap();
        catalog.join().unwrap();
        assert!(
            progressed,
            "schema capture held catalog while waiting for theater"
        );
    }
}

#[test]
fn write_timestamp_sentinel_rejected_before_changing_graph_state() {
    let graph = Graph::default();
    graph.begin_write().unwrap();
    graph
        .create_node(vec!["Kept".into()], BTreeMap::new())
        .unwrap();
    let pending_ts = graph.pending_commit_ts();
    let stats = graph.stats.lock().unwrap().nodes_created;
    let redo = graph.pending_redo.lock().unwrap().clone();
    graph.set_commit_ts(u64::MAX - 1);

    for err in [
        graph.begin_write().unwrap_err(),
        graph.begin_write_at(u64::MAX).unwrap_err(),
    ] {
        assert!(
            matches!(err, cypherlite_core::Error::Execution(ref message) if message == "write timestamp exhausted")
        );
        assert!(graph.in_write.load(Ordering::Acquire));
        assert_eq!(graph.pending_commit_ts(), pending_ts);
        assert_eq!(graph.stats.lock().unwrap().nodes_created, stats);
        assert_eq!(*graph.pending_redo.lock().unwrap(), redo);
        assert!(graph.record_redo.load(Ordering::Acquire));
        assert_eq!(graph.commit_ts(), u64::MAX - 1);
    }

    let fresh = Graph::default();
    fresh.begin_write_at(u64::MAX - 1).unwrap();
    assert_eq!(fresh.pending_commit_ts(), u64::MAX - 1);
    fresh.commit_write();
    assert_eq!(fresh.commit_ts(), u64::MAX - 1);
    assert!(matches!(
        fresh.begin_write(),
        Err(cypherlite_core::Error::Execution(_))
    ));
    assert!(!fresh.in_write.load(Ordering::Acquire));
}

#[test]
fn apply_redo_rejects_max_timestamp_before_installing_any_ops() {
    let graph = Graph::default();
    let ops = vec![RedoOp::CreateNode {
        id: 0,
        labels: Arc::new(vec!["Kept".into()]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    }];
    assert!(matches!(
        graph.apply_redo(ops.clone(), u64::MAX),
        Err(cypherlite_core::Error::Storage(_))
    ));
    assert!(!graph.in_write.load(Ordering::Acquire));
    assert_eq!(graph.pending_commit_ts(), 0);
    assert_eq!(graph.commit_ts(), 0);
    assert_eq!(graph.next_node_id(), 0);
    assert!(graph.all_node_ids().is_empty());
    assert!(graph.pending_redo.lock().unwrap().is_empty());

    graph.apply_redo(ops, u64::MAX - 1).unwrap();
    assert_eq!(graph.commit_ts(), u64::MAX - 1);
    assert_eq!(graph.all_node_ids(), vec![0]);
}

#[test]
fn redo_preflight_rejects_zero_and_nonforward_timestamps_without_a_prefix() {
    let node = |id| RedoOp::CreateNode {
        id,
        labels: Arc::new(vec!["Kept".into()]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
            "value".into(),
            Value::Int(1),
        )])),
    };
    let rel = |id, end| RedoOp::CreateRel {
        id,
        start: 0,
        end,
        rel_type: "R".into(),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
            "value".into(),
            Value::Int(2),
        )])),
    };
    let versions = |g: &Graph| {
        let nodes: BTreeMap<_, _> = g
            .nodes
            .iter()
            .map(|entry| {
                let id = *entry.key();
                (
                    id,
                    entry
                        .iter()
                        .map(|v| (v.xmin, v.xmax, v.live.to_node(id, &g.interner)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        let rels: BTreeMap<_, _> = g
            .rels
            .iter()
            .map(|entry| {
                let id = *entry.key();
                (
                    id,
                    entry
                        .iter()
                        .map(|v| (v.xmin, v.xmax, v.live.to_rel(id, &g.interner)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        (nodes, rels)
    };
    // Reproductions: (published ts, next node ID, proposed ts).
    for (published, next_id, proposed) in [(0, 0, 0), (2, 1, 1), (2, 1, 2)] {
        let g = Graph::default();
        if published != 0 {
            g.apply_redo(vec![node(0), rel(0, 0)], published).unwrap();
        }
        assert_eq!(g.next_node_id(), next_id);
        let before_nodes = g.all_node_ids();
        let before_rels = g.all_rel_ids();
        let before_versions = versions(&g);
        let ops = vec![
            node(next_id),
            rel(next_id, next_id),
            RedoOp::SetNodeProp {
                id: 0,
                key: "value".into(),
                value: Value::Int(3),
            },
            RedoOp::SetRelProp {
                id: 0,
                key: "value".into(),
                value: Value::Int(4),
            },
        ];
        assert!(matches!(
            g.validate_redo_applicability(&ops, proposed),
            Err(cypherlite_core::Error::Storage(_))
        ));
        assert!(matches!(
            g.apply_redo(ops, proposed),
            Err(cypherlite_core::Error::Storage(_))
        ));
        assert_eq!(g.next_node_id(), next_id);
        assert_eq!(g.next_rel_id(), next_id);
        assert_eq!(g.commit_ts(), published);
        assert_eq!(g.pending_commit_ts(), published);
        assert_eq!(g.all_node_ids(), before_nodes);
        assert_eq!(g.all_rel_ids(), before_rels);
        // Compare every version, including invisible ones and the xmax of
        // existing IDs; visible rows and map sizes alone miss both.
        assert_eq!(versions(&g), before_versions);
        assert!(g
            .nodes
            .iter()
            .all(|entry| entry.iter().all(|v| v.xmin != 0)));
        assert!(g.rels.iter().all(|entry| entry.iter().all(|v| v.xmin != 0)));
        assert!(!g.in_write.load(Ordering::Acquire));
    }
}

#[test]
fn redo_preflight_accepts_same_record_node_and_relationship_at_advancing_ts() {
    for prior in [0, 2] {
        let g = Graph::default();
        if prior != 0 {
            g.apply_redo(vec![], prior).unwrap();
        }
        let ops = vec![
            RedoOp::CreateNode {
                id: 0,
                labels: Arc::new(vec!["N".into()]),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            },
            RedoOp::CreateRel {
                id: 0,
                start: 0,
                end: 0,
                rel_type: "R".into(),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            },
        ];
        g.validate_redo_applicability(&ops, prior + 1).unwrap();
        g.apply_redo(ops, prior + 1).unwrap();
        assert_eq!(g.commit_ts(), prior + 1);
        assert_eq!(g.all_node_ids(), vec![0]);
        assert_eq!(g.all_rel_ids(), vec![0]);
    }
}

#[test]
fn apply_redo_rejects_max_ids_before_installing_any_ops() {
    for ops in [
        vec![RedoOp::CreateNode {
            id: i64::MAX,
            labels: Arc::new(vec![]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        }],
        vec![
            RedoOp::CreateNode {
                id: 0,
                labels: Arc::new(vec![]),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            },
            RedoOp::CreateRel {
                id: i64::MAX,
                start: 0,
                end: 0,
                rel_type: "R".into(),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            },
        ],
        vec![
            RedoOp::CreateNode {
                id: 0,
                labels: Arc::new(vec![]),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            },
            RedoOp::CreateRel {
                id: 0,
                start: 0,
                end: i64::MAX,
                rel_type: "R".into(),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            },
        ],
    ] {
        let graph = Graph::default();
        assert!(matches!(
            graph.apply_redo(ops, 1),
            Err(cypherlite_core::Error::Storage(_))
        ));
        assert_eq!(graph.next_node_id(), 0);
        assert_eq!(graph.next_rel_id(), 0);
        assert_eq!(graph.commit_ts(), 0);
    }
}

#[test]
fn redo_preflight_rejects_later_endpoint_without_installing_prefix() {
    let node = |id| RedoOp::CreateNode {
        id,
        labels: Arc::new(vec!["Fresh".into()]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    };
    let rel = |id, start, end| RedoOp::CreateRel {
        id,
        start,
        end,
        rel_type: "R".into(),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    };
    for suffix in [
        vec![rel(2, 1, 99)],
        vec![RedoOp::DeleteNode { id: 1 }, rel(2, 1, 0)],
        vec![rel(1, 1, 0), RedoOp::DeleteNode { id: 1 }, rel(2, 0, 1)],
    ] {
        let g = Graph::default();
        g.apply_redo(vec![node(0)], 1).unwrap();
        let ops = std::iter::once(node(1)).chain(suffix).collect::<Vec<_>>();
        assert!(matches!(
            g.validate_redo_applicability(&ops, 2),
            Err(cypherlite_core::Error::Semantic(_))
        ));
        assert!(matches!(
            g.apply_redo(ops, 2),
            Err(cypherlite_core::Error::Semantic(_))
        ));
        assert_eq!(g.commit_ts(), 1);
        assert_eq!(g.next_node_id(), 1);
        assert_eq!(g.next_rel_id(), 0);
        assert_eq!(g.all_node_ids(), vec![0]);
        assert!(g.all_rel_ids().is_empty());
        assert!(!g.in_write.load(Ordering::Acquire));
        assert_eq!(g.pending_commit_ts(), 1);
    }
}

#[test]
fn direct_redo_rejects_incomplete_label_metadata_without_a_prefix() {
    let graph = Graph::default();
    let ops = vec![
        RedoOp::CreateNode {
            id: 0,
            labels: Arc::new(vec!["Person".into()]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        },
        RedoOp::CreateConstraint {
            name: "rule".into(),
            relationship: false,
            label: "Person".into(),
            properties: vec![],
            kind: "nodeLabelExistence".into(),
            property_type: None,
        },
    ];
    assert!(matches!(
        graph.apply_redo(ops, 1),
        Err(cypherlite_core::Error::Storage(_))
    ));
    assert_eq!(graph.commit_ts(), 0);
    assert_eq!(graph.next_node_id(), 0);
    assert!(graph.named_constraints().is_empty());
    assert!(!graph.in_write.load(Ordering::Acquire));
}

#[test]
fn redo_preflight_catalog_duplicates_and_bad_options_are_atomic() {
    let node = RedoOp::CreateNode {
        id: 1,
        labels: Arc::new(vec!["Fresh".into()]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    };
    let index = |name: &str, property: &str| RedoOp::CreateIndex {
        name: name.into(),
        label: "L".into(),
        property: property.into(),
    };
    let constraint = |name: &str, kind: &str| RedoOp::CreateConstraint {
        name: name.into(),
        relationship: false,
        label: "L".into(),
        properties: vec!["p".into()],
        kind: kind.into(),
        property_type: None,
    };
    let invalid = [
        vec![index("first", "p"), index("second", "p")],
        vec![index("first", "p"), index("first", "q")],
        vec![
            constraint("first", "UNIQUE"),
            constraint("second", "UNIQUE"),
        ],
        vec![constraint("first", "UNIQUE"), constraint("first", "EXISTS")],
        vec![RedoOp::CatalogIndexDetail {
            name: "bad".into(),
            detail: vector_index_detail(),
            options: serde_json::json!({"config": {"untagged": true}}),
        }],
    ];
    for suffix in invalid {
        let g = Graph::default();
        g.apply_redo(
            vec![RedoOp::CreateNode {
                id: 0,
                labels: Arc::new(vec!["Kept".into()]),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                    "p".into(),
                    Value::Int(7),
                )])),
            }],
            1,
        )
        .unwrap();
        let ops = std::iter::once(node.clone())
            .chain(suffix)
            .collect::<Vec<_>>();
        let err = g.validate_redo_applicability(&ops, 2).unwrap_err();
        assert!(
            matches!(
                err,
                cypherlite_core::Error::Semantic(_) | cypherlite_core::Error::Storage(_)
            ),
            "{err}"
        );
        assert!(g.apply_redo(ops, 2).is_err());
        assert_eq!(g.commit_ts(), 1);
        assert_eq!((g.next_node_id(), g.next_rel_id()), (1, 0));
        assert_eq!(g.all_node_ids(), vec![0]);
        assert_eq!(g.node(0).unwrap().properties["p"], Value::Int(7));
        assert!(g.named_indexes().is_empty());
        assert!(g.named_constraints().is_empty());
        assert!(g.theater.lock().unwrap().index_details.is_empty());
        assert!(!g.in_write.load(Ordering::Acquire));
    }
}

#[test]
fn redo_preflight_invalid_pull_local_id_keeps_prior_rows() {
    for bad in [-1, i64::MAX] {
        let g = Graph::default();
        let ops = vec![
            RedoOp::CreateNode {
                id: 0,
                labels: Arc::new(vec![]),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            },
            RedoOp::PullIdentityPut {
                source: "src".into(),
                kind: PullKind::Node,
                remote_id: "remote".into(),
                local_id: bad,
            },
        ];
        assert!(matches!(
            g.validate_redo_applicability(&ops, 1),
            Err(cypherlite_core::Error::Storage(_))
        ));
        assert!(matches!(
            g.apply_redo(ops, 1),
            Err(cypherlite_core::Error::Storage(_))
        ));
        assert_eq!(g.commit_ts(), 0);
        assert_eq!(g.next_node_id(), 0);
        assert!(g.all_node_ids().is_empty());
        assert_eq!(g.pull_identity_get("src", PullKind::Node, "remote"), None);
    }
}

#[test]
fn redo_preflight_accepts_ordered_detach_schema_and_pull_updates() {
    let g = Graph::default();
    let node = |id| RedoOp::CreateNode {
        id,
        labels: Arc::new(vec![]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    };
    let index = |name: &str| RedoOp::CreateIndex {
        name: name.into(),
        label: "L".into(),
        property: "p".into(),
    };
    let constraint = |name: &str| RedoOp::CreateConstraint {
        name: name.into(),
        relationship: false,
        label: "L".into(),
        properties: vec!["p".into()],
        kind: "UNIQUE".into(),
        property_type: None,
    };
    let ops = vec![
        node(0),
        node(1),
        RedoOp::CreateRel {
            id: 0,
            start: 0,
            end: 1,
            rel_type: "R".into(),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        },
        RedoOp::DeleteNode { id: 1 },
        node(1),
        RedoOp::CreateRel {
            id: 1,
            start: 0,
            end: 1,
            rel_type: "R".into(),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        },
        index("i"),
        index("i"),
        RedoOp::DropIndex { name: "i".into() },
        index("j"),
        constraint("c"),
        constraint("c"),
        RedoOp::DropConstraint { name: "c".into() },
        constraint("d"),
        RedoOp::PullIdentityPut {
            source: "src".into(),
            kind: PullKind::Node,
            remote_id: "remote".into(),
            local_id: 1,
        },
    ];
    g.validate_redo_applicability(&ops, 1).unwrap();
    g.apply_redo(ops, 1).unwrap();
    assert_eq!(g.commit_ts(), 1);
    assert_eq!(g.all_node_ids(), vec![0, 1]);
    assert_eq!(g.all_rel_ids(), vec![1]);
    assert_eq!(
        g.pull_identity_get("src", PullKind::Node, "remote"),
        Some(1)
    );
    assert_eq!(g.named_indexes()[0].name, "j");
    assert_eq!(g.named_constraints()[0].name, "d");
}

#[test]
fn redo_group_preflight_tracks_prior_records_without_applying_them() {
    let g = Graph::default();
    let first = [
        RedoOp::CreateNode {
            id: 0,
            labels: Arc::new(vec![]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        },
        RedoOp::CreateIndex {
            name: "i".into(),
            label: "L".into(),
            property: "p".into(),
        },
    ];
    let second = [
        RedoOp::CreateRel {
            id: 0,
            start: 0,
            end: 0,
            rel_type: "R".into(),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        },
        RedoOp::PullIdentityPut {
            source: "src".into(),
            kind: PullKind::Rel,
            remote_id: "remote".into(),
            local_id: 0,
        },
    ];
    g.validate_redo_group_applicability([(&first[..], 1), (&second[..], 2)])
        .unwrap();
    for timestamps in [[0, 1], [1, 1], [2, 1], [1, u64::MAX]] {
        assert!(matches!(
            g.validate_redo_group_applicability([
                (&first[..], timestamps[0]),
                (&second[..], timestamps[1])
            ]),
            Err(cypherlite_core::Error::Storage(_))
        ));
    }
    let bad = [RedoOp::CreateIndex {
        name: "other".into(),
        label: "L".into(),
        property: "p".into(),
    }];
    assert!(matches!(
        g.validate_redo_group_applicability([(&first[..], 1), (&bad[..], 2)]),
        Err(cypherlite_core::Error::Semantic(_))
    ));
    let deleted = [RedoOp::DeleteNode { id: 0 }];
    assert!(matches!(
        g.validate_redo_group_applicability([(&first[..], 1), (&deleted[..], 2), (&second[..], 3)]),
        Err(cypherlite_core::Error::Semantic(_))
    ));
    assert_eq!(g.commit_ts(), 0);
    assert!(g.all_node_ids().is_empty());
    assert!(g.named_indexes().is_empty());
}

#[test]
fn direct_graph_creation_exhaustion_is_typed_and_has_no_side_effects() {
    let graph = Graph::default();
    let node = graph.create_node(vec![], BTreeMap::new()).unwrap();
    graph.set_next_node_id(i64::MAX);
    graph.set_next_rel_id(i64::MAX);
    let used = graph.used_bytes();
    graph.begin_write().unwrap();
    assert!(matches!(
        graph.create_node(vec!["Bad".into()], BTreeMap::new()),
        Err(cypherlite_core::Error::Execution(ref message)) if message == "node ID exhausted"
    ));
    assert!(matches!(
        graph.create_rel(node, node, "Bad".into(), BTreeMap::new()),
        Err(cypherlite_core::Error::Execution(ref message)) if message == "relationship ID exhausted"
    ));
    assert!(graph.take_redo().is_empty());
    assert_eq!(graph.next_node_id(), i64::MAX);
    assert_eq!(graph.next_rel_id(), i64::MAX);
    assert_eq!(graph.used_bytes(), used);
    assert_eq!(graph.stats.lock().unwrap().nodes_created, 0);
    assert_eq!(graph.stats.lock().unwrap().relationships_created, 0);
    assert!(graph.dirty_node_ids().iter().all(|id| *id != i64::MAX));
}

#[test]
fn failed_relationship_does_not_consume_last_valid_id() {
    let graph = Graph::default();
    let node = graph.create_node(vec![], BTreeMap::new()).unwrap();
    graph.begin_write().unwrap();
    graph.set_next_rel_id(i64::MAX - 1);
    let used = graph.used_bytes();
    assert!(matches!(
        graph.create_rel(node, 42, "Missing".into(), BTreeMap::new()),
        Err(cypherlite_core::Error::Semantic(_))
    ));
    assert!(matches!(
        graph.create_rel(i64::MAX, node, "Invalid".into(), BTreeMap::new()),
        Err(cypherlite_core::Error::Storage(_))
    ));
    assert_eq!(graph.next_rel_id(), i64::MAX - 1);
    assert!(graph.take_redo().is_empty());
    assert!(graph.dirty_rel_ids().is_empty());
    assert_eq!(graph.used_bytes(), used);
    assert_eq!(graph.stats.lock().unwrap().relationships_created, 0);

    let id = graph
        .create_rel(node, node, "Valid".into(), BTreeMap::new())
        .unwrap();
    assert_eq!(id, i64::MAX - 1);
    assert_eq!(graph.next_rel_id(), i64::MAX);
    assert_eq!(graph.take_redo().len(), 1);
    assert!(matches!(
        graph.create_rel(node, node, "Extra".into(), BTreeMap::new()),
        Err(cypherlite_core::Error::Execution(ref message)) if message == "relationship ID exhausted"
    ));
}

#[test]
fn concurrent_last_id_has_exactly_one_winner_per_entity_kind() {
    use std::sync::{Arc, Barrier};

    let graph = Arc::new(Graph::default());
    let root = graph.create_node(vec![], BTreeMap::new()).unwrap();
    graph.begin_write().unwrap();
    graph.set_next_node_id(i64::MAX - 1);
    graph.set_next_rel_id(i64::MAX - 1);
    let barrier = Arc::new(Barrier::new(16));
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let graph = Arc::clone(&graph);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                (
                    graph.create_node(vec![], BTreeMap::new()),
                    graph.create_rel(root, root, "R".into(), BTreeMap::new()),
                )
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|(node, _)| node.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|(_, rel)| rel.is_ok()).count(), 1);
    assert!(results.iter().all(|(node, rel)| {
        node.as_ref().map_or(true, |id| *id == i64::MAX - 1)
            && rel.as_ref().map_or(true, |id| *id == i64::MAX - 1)
    }));
    assert_eq!(graph.next_node_id(), i64::MAX);
    assert_eq!(graph.next_rel_id(), i64::MAX);
    assert!(graph.node(i64::MAX - 1).is_some());
    assert!(graph.rel(i64::MAX - 1).is_some());
    assert!(graph.node(i64::MAX).is_none());
    assert!(graph.rel(i64::MAX).is_none());
}

#[test]
fn create_and_rel() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    assert_eq!(g.neighbors(a, cypherlite_ir::Direction::Outgoing).len(), 1);
}

/// `node_labels_at` is the label-only read; it must agree with
/// `node_at(..).labels` for multi-label nodes and carry no properties.
#[test]
fn node_labels_at_matches_node_at_labels() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let mut props = BTreeMap::new();
    props.insert("name".to_string(), Value::String("Anna".into()));
    let a = g
        .create_node(vec!["Person".into(), "Admin".into()], props)
        .unwrap();
    let b = g
        .create_node(vec!["Person".into()], BTreeMap::new())
        .unwrap();
    g.commit_write();
    let ts = g.commit_ts();

    let mut a_labels = g.node_labels_at(a, ts).unwrap();
    let mut expected = (*g.node_at(a, ts).unwrap().labels).clone();
    a_labels.sort();
    expected.sort();
    assert_eq!(a_labels, expected);
    assert_eq!(a_labels, vec!["Admin".to_string(), "Person".to_string()]);
    assert_eq!(g.node_labels_at(b, ts).unwrap(), vec!["Person".to_string()]);
    // Invisible / absent ids yield None rather than an empty label set.
    assert!(g.node_labels_at(999, ts).is_none());
}

#[test]
fn for_each_neighbor_matches_neighbors_at() {
    use cypherlite_ir::Direction;
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    let c = g.create_node(vec!["C".into()], BTreeMap::new()).unwrap();
    g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    g.create_rel(c, a, "R".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    for dir in [Direction::Outgoing, Direction::Incoming, Direction::Both] {
        let listed = g.neighbors(a, dir);
        let mut walked = Vec::new();
        g.for_each_neighbor_at(a, dir, g.commit_ts(), &[], |rid| walked.push(rid));
        assert_eq!(walked, listed, "dir={dir:?}");
    }
}

#[test]
fn typed_adj_walks_newest_ts_first() {
    use cypherlite_ir::Direction;
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    let c = g.create_node(vec!["C".into()], BTreeMap::new()).unwrap();
    let d = g.create_node(vec!["D".into()], BTreeMap::new()).unwrap();
    let old = g
        .create_rel(
            a,
            b,
            "T".into(),
            BTreeMap::from([("ts".into(), Value::Int(1))]),
        )
        .unwrap();
    let newest = g
        .create_rel(
            a,
            c,
            "T".into(),
            BTreeMap::from([("ts".into(), Value::Int(30))]),
        )
        .unwrap();
    let mid = g
        .create_rel(
            a,
            d,
            "T".into(),
            BTreeMap::from([("ts".into(), Value::Int(10))]),
        )
        .unwrap();
    g.commit_write();
    let mut walked = Vec::new();
    g.for_each_neighbor_at(
        a,
        Direction::Outgoing,
        g.commit_ts(),
        &["T".into()],
        |rid| walked.push(rid),
    );
    assert_eq!(walked, vec![newest, mid, old]);
    g.begin_write().unwrap();
    g.set_property_rel(old, "ts", Value::Int(40));
    g.commit_write();
    walked.clear();
    g.for_each_neighbor_at(
        a,
        Direction::Outgoing,
        g.commit_ts(),
        &["T".into()],
        |rid| walked.push(rid),
    );
    assert_eq!(walked, vec![old, newest, mid]);
}

#[test]
fn typed_adj_skips_other_types() {
    use cypherlite_ir::Direction;
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    let c = g.create_node(vec!["C".into()], BTreeMap::new()).unwrap();
    let t = g.create_rel(a, b, "T".into(), BTreeMap::new()).unwrap();
    g.create_rel(a, c, "OTHER".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    let mut only_t = Vec::new();
    g.for_each_neighbor_at(
        a,
        Direction::Outgoing,
        g.commit_ts(),
        &["T".into()],
        |rid| only_t.push(rid),
    );
    assert_eq!(only_t, vec![t]);
    let mut dup = Vec::new();
    g.for_each_neighbor_at(
        a,
        Direction::Outgoing,
        g.commit_ts(),
        &["T".into(), "T".into()],
        |rid| dup.push(rid),
    );
    assert_eq!(dup, vec![t]);
}

#[test]
fn string_contains_lookup_is_selective() {
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(
        vec!["C".into()],
        BTreeMap::from([("name".into(), Value::String("needle-here".into()))]),
    )
    .unwrap();
    g.create_node(
        vec!["C".into()],
        BTreeMap::from([("name".into(), Value::String("other".into()))]),
    )
    .unwrap();
    g.commit_write();
    g.materialize_text_index("C", "name");
    let ids = g
        .lookup_string_contains_at(&["C".into()], "name", "needle", g.commit_ts())
        .unwrap();
    assert_eq!(ids.len(), 1);
    let none = g
        .lookup_string_contains_at(&["C".into()], "name", "zzz", g.commit_ts())
        .unwrap();
    assert!(none.is_empty());
    assert!(g
        .lookup_string_contains_at(&["C".into()], "name", "", g.commit_ts())
        .is_none());
}

#[test]
fn short_contains_needle_does_not_seek() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let id = g
        .create_node(
            vec!["C".into()],
            BTreeMap::from([("name".into(), Value::String("xxabxx".into()))]),
        )
        .unwrap();
    g.commit_write();
    g.materialize_text_index("C", "name");
    assert!(g
        .lookup_string_contains_at(&["C".into()], "name", "a", g.commit_ts())
        .is_none());
    assert!(g
        .lookup_string_contains_at(&["C".into()], "name", "ab", g.commit_ts())
        .is_none());
    let ids = g
        .lookup_string_contains_at(&["C".into()], "name", "xab", g.commit_ts())
        .unwrap();
    assert_eq!(ids, vec![id]);
    let none = g
        .lookup_string_contains_at(&["C".into()], "name", "zzz", g.commit_ts())
        .unwrap();
    assert!(none.is_empty());
    let r = crate::exec::test_support::exec_cypher(
        &g,
        "MATCH (n:C) WHERE n.name CONTAINS 'ab' RETURN n.name AS n",
    );
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0].as_string().unwrap(), "xxabxx");
}

#[test]
fn string_prefix_lookup() {
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(
        vec!["C".into()],
        BTreeMap::from([("name".into(), Value::String("Ada".into()))]),
    )
    .unwrap();
    g.create_node(
        vec!["C".into()],
        BTreeMap::from([("name".into(), Value::String("Bob".into()))]),
    )
    .unwrap();
    g.commit_write();
    g.materialize_range_index("C", "name");
    let ids = g
        .lookup_string_prefix_at(&["C".into()], "name", "Ad", g.commit_ts())
        .unwrap();
    assert_eq!(ids.len(), 1);
}

#[test]
fn string_index_is_lazy_until_first_seek() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g
        .create_node(
            vec!["C".into()],
            BTreeMap::from([("name".into(), Value::String("needle-here".into()))]),
        )
        .unwrap();
    g.create_node(
        vec!["C".into()],
        BTreeMap::from([("name".into(), Value::String("other".into()))]),
    )
    .unwrap();
    g.commit_write();
    // Writes alone never materialize trigram postings. CONTAINS scans
    // until a TEXT index names the pair; STARTS WITH scans until a RANGE
    // index does.
    assert_eq!(g.gram_pair_state("C", "name"), None);
    assert!(g
        .lookup_string_contains_at(&["C".into()], "name", "needle", g.commit_ts())
        .is_none());
    assert!(g
        .lookup_string_prefix_at(&["C".into()], "name", "need", g.commit_ts())
        .is_none());
    g.materialize_text_index("C", "name");
    let ids = g
        .lookup_string_contains_at(&["C".into()], "name", "needle", g.commit_ts())
        .unwrap();
    assert_eq!(ids, vec![a]);
    let (grams, dirty) = g.gram_pair_state("C", "name").unwrap();
    assert!(grams > 0 && dirty == 0);
    g.materialize_range_index("C", "name");
    let ids = g
        .lookup_string_prefix_at(&["C".into()], "name", "need", g.commit_ts())
        .unwrap();
    assert_eq!(ids, vec![a]);
}

#[test]
fn string_index_dirty_catchup_after_writes() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g
        .create_node(
            vec!["C".into()],
            BTreeMap::from([("name".into(), Value::String("needle-a".into()))]),
        )
        .unwrap();
    let b = g
        .create_node(
            vec!["C".into()],
            BTreeMap::from([("name".into(), Value::String("plain".into()))]),
        )
        .unwrap();
    g.commit_write();
    g.materialize_text_index("C", "name");
    let ids = g
        .lookup_string_contains_at(&["C".into()], "name", "needle", g.commit_ts())
        .unwrap();
    assert_eq!(ids, vec![a]);

    // Writes after the build: one create, one SET into a matching
    // string, one SET away from it, one delete (ADR-0063 dirty drain).
    g.begin_write().unwrap();
    let c = g
        .create_node(
            vec!["C".into()],
            BTreeMap::from([("name".into(), Value::String("needle-c".into()))]),
        )
        .unwrap();
    g.set_property_node(b, "name", Value::String("needle-b".into()));
    g.set_property_node(a, "name", Value::String("gone".into()));
    g.commit_write();
    let (grams, dirty) = g.gram_pair_state("C", "name").unwrap();
    assert!(grams > 0);
    assert_eq!(dirty, 3, "create + two SETs dirty three nodes");
    let ids = g
        .lookup_string_contains_at(&["C".into()], "name", "needle", g.commit_ts())
        .unwrap();
    assert_eq!(
        ids,
        vec![b, c],
        "drain adds new matches, residual drops stale"
    );
    let (_, dirty) = g.gram_pair_state("C", "name").unwrap();
    assert_eq!(dirty, 0);

    g.begin_write().unwrap();
    g.delete_node(c, true).unwrap();
    g.commit_write();
    let ids = g
        .lookup_string_contains_at(&["C".into()], "name", "needle", g.commit_ts())
        .unwrap();
    assert_eq!(ids, vec![b]);
}

#[test]
fn string_index_add_labels_catchup() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g
        .create_node(
            vec!["A".into()],
            BTreeMap::from([("name".into(), Value::String("needle-a".into()))]),
        )
        .unwrap();
    g.commit_write();
    g.materialize_text_index("B", "name");
    // The text index exists on an empty label, then the node moves into :B.
    let ids = g
        .lookup_string_contains_at(&["B".into()], "name", "needle", g.commit_ts())
        .unwrap();
    assert!(ids.is_empty());
    g.begin_write().unwrap();
    g.add_labels(a, &["B".into()]);
    g.commit_write();
    let ids = g
        .lookup_string_contains_at(&["B".into()], "name", "needle", g.commit_ts())
        .unwrap();
    assert_eq!(
        ids,
        vec![a],
        "label add must dirty the new (label, key) pair"
    );
    // REMOVE LABEL leaves a stale posting; the residual check filters it.
    g.begin_write().unwrap();
    g.remove_labels(a, &["B".into()]);
    g.commit_write();
    let ids = g
        .lookup_string_contains_at(&["B".into()], "name", "needle", g.commit_ts())
        .unwrap();
    assert!(ids.is_empty());
}

#[test]
fn string_index_serves_pre_build_snapshot() {
    // ADR-0063: a seek pinned before the pair was built must see the same
    // versions eager maintenance would have posted, so builds index every
    // version in the chain, not just the latest.
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g
        .create_node(
            vec!["C".into()],
            BTreeMap::from([("name".into(), Value::String("needle-a".into()))]),
        )
        .unwrap();
    g.commit_write();
    let pin = g.pin_now().unwrap();
    let old_ts = pin.ts();
    g.begin_write().unwrap();
    g.set_property_node(a, "name", Value::String("renamed".into()));
    g.commit_write();
    // The index is built after the overwrite and still posts every version,
    // so the pinned snapshot matches the old string.
    g.materialize_text_index("C", "name");
    let ids = g
        .lookup_string_contains_at(&["C".into()], "name", "needle", old_ts)
        .unwrap();
    assert_eq!(ids, vec![a]);
    let ids = g
        .lookup_string_contains_at(&["C".into()], "name", "needle", g.commit_ts())
        .unwrap();
    assert!(ids.is_empty());
    drop(pin);
}

#[test]
fn string_index_star_pair_covers_all_labels() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g
        .create_node(
            vec!["A".into()],
            BTreeMap::from([("name".into(), Value::String("needle-a".into()))]),
        )
        .unwrap();
    let b = g
        .create_node(
            vec!["B".into()],
            BTreeMap::from([("name".into(), Value::String("needle-b".into()))]),
        )
        .unwrap();
    g.commit_write();
    // An unlabeled CONTAINS is not covered by any one text index.
    assert!(g
        .lookup_string_contains_at(&[], "name", "needle", g.commit_ts())
        .is_none());
    g.materialize_text_index("A", "name");
    g.materialize_text_index("B", "name");
    let ids = g
        .lookup_string_contains_at(&["A".into()], "name", "needle", g.commit_ts())
        .unwrap();
    assert_eq!(ids, vec![a]);
    let ids = g
        .lookup_string_contains_at(&["B".into()], "name", "needle", g.commit_ts())
        .unwrap();
    assert_eq!(ids, vec![b]);
}

#[test]
fn range_index_orders_numbers_and_prefix() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let low = g
        .create_node(
            vec!["N".into()],
            BTreeMap::from([("x".into(), Value::Int(1))]),
        )
        .unwrap();
    let high = g
        .create_node(
            vec!["N".into()],
            BTreeMap::from([("x".into(), Value::Int(5))]),
        )
        .unwrap();
    g.commit_write();
    assert!(g
        .lookup_range_seek(
            &["N".into()],
            "x",
            &RangeSeek::Cmp {
                op: cypherlite_ir::BinaryOp::Gt,
                bound: Value::Int(1),
            },
            g.commit_ts(),
        )
        .is_none());
    g.materialize_range_index("N", "x");
    let ids = g
        .lookup_range_seek(
            &["N".into()],
            "x",
            &RangeSeek::Cmp {
                op: cypherlite_ir::BinaryOp::Gt,
                bound: Value::Int(1),
            },
            g.commit_ts(),
        )
        .unwrap();
    assert_eq!(ids, vec![high]);
    let ids = g
        .lookup_nodes(
            &["N".into()],
            &BTreeMap::from([("x".into(), Value::Int(1))]),
        )
        .unwrap();
    assert_eq!(ids, vec![low]);
}

#[test]
fn range_seek_scans_when_a_datetime_does_not_fit_the_order_key() {
    use chrono::NaiveDate;
    use cypherlite_core::Temporal;

    let g = Graph::default();
    g.begin_write().unwrap();
    let far = NaiveDate::from_ymd_opt(9999, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    g.create_node(
        vec!["N".into()],
        BTreeMap::from([("t".into(), Value::Temporal(Temporal::LocalDateTime(far)))]),
    )
    .unwrap();
    g.commit_write();
    g.materialize_range_index("N", "t");
    assert!(g
        .lookup_range_seek(
            &["N".into()],
            "t",
            &RangeSeek::Cmp {
                op: cypherlite_ir::BinaryOp::Gt,
                bound: Value::Int(0),
            },
            g.commit_ts(),
        )
        .is_none());
}

#[test]
fn point_index_answers_bbox() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let inside = g
        .create_node(
            vec!["P".into()],
            BTreeMap::from([(
                "loc".into(),
                Value::Point(cypherlite_core::PointValue {
                    srid: 7203,
                    x: 1.0,
                    y: 1.0,
                    z: None,
                }),
            )]),
        )
        .unwrap();
    g.create_node(
        vec!["P".into()],
        BTreeMap::from([(
            "loc".into(),
            Value::Point(cypherlite_core::PointValue {
                srid: 7203,
                x: 10.0,
                y: 10.0,
                z: None,
            }),
        )]),
    )
    .unwrap();
    g.commit_write();
    let seek = SpatialSeek::BBox {
        lo: Value::Point(cypherlite_core::PointValue {
            srid: 7203,
            x: 0.0,
            y: 0.0,
            z: None,
        }),
        hi: Value::Point(cypherlite_core::PointValue {
            srid: 7203,
            x: 2.0,
            y: 2.0,
            z: None,
        }),
    };
    assert!(g
        .lookup_spatial_seek(&["P".into()], "loc", &seek, g.commit_ts())
        .is_none());
    g.materialize_point_index("P", "loc");
    let ids = g
        .lookup_spatial_seek(&["P".into()], "loc", &seek, g.commit_ts())
        .unwrap();
    assert_eq!(ids, vec![inside]);
    let ids = g
        .lookup_spatial_seek(
            &["P".into()],
            "loc",
            &SpatialSeek::Distance {
                origin: Value::Point(cypherlite_core::PointValue {
                    srid: 7203,
                    x: 0.0,
                    y: 0.0,
                    z: None,
                }),
                max: 2.0,
                inclusive: true,
            },
            g.commit_ts(),
        )
        .unwrap();
    assert_eq!(ids, vec![inside]);
    let ids = g
        .lookup_spatial_seek(
            &["P".into()],
            "loc",
            &SpatialSeek::BBox {
                lo: Value::Point(cypherlite_core::PointValue {
                    srid: 7203,
                    x: -1.0e12,
                    y: -1.0e12,
                    z: None,
                }),
                hi: Value::Point(cypherlite_core::PointValue {
                    srid: 7203,
                    x: 1.0e12,
                    y: 1.0e12,
                    z: None,
                }),
            },
            g.commit_ts(),
        )
        .unwrap();
    assert_eq!(ids.len(), 2);
}

#[test]
fn catalog_index_detail_builds_and_drop_discards() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let book = g
        .create_node(
            vec!["Book".into()],
            BTreeMap::from([("title".into(), Value::String("War and Peace".into()))]),
        )
        .unwrap();
    let place = g
        .create_node(
            vec!["Place".into()],
            BTreeMap::from([
                ("name".into(), Value::String("Ada".into())),
                ("n".into(), Value::Int(4)),
                (
                    "loc".into(),
                    Value::Point(cypherlite_core::PointValue {
                        srid: 7203,
                        x: 1.0,
                        y: 1.0,
                        z: None,
                    }),
                ),
            ]),
        )
        .unwrap();
    g.commit_write();
    let before = g.used_bytes();
    assert_eq!(before, g.accounted_bytes());

    let text = crate::redo::IndexDetailSpec {
        kind: crate::redo::IndexKindSpec::Text,
        relationship: false,
        labels: vec!["Book".into()],
        properties: vec!["title".into()],
        with_properties: vec![],
    };
    g.apply_redo(
        vec![RedoOp::CatalogIndexDetail {
            name: "titleIndex".into(),
            detail: text,
            options: redo::props_json(&BTreeMap::new()),
        }],
        g.commit_ts() + 1,
    )
    .unwrap();
    let ids = g
        .lookup_string_contains_at(&["Book".into()], "title", "and", g.commit_ts())
        .unwrap();
    assert_eq!(ids, vec![book]);
    assert!(g.used_bytes() > before);
    assert_eq!(g.used_bytes(), g.accounted_bytes());
    g.rebuild_indexes();
    let ids = g
        .lookup_string_contains_at(&["Book".into()], "title", "and", g.commit_ts())
        .unwrap();
    assert_eq!(ids, vec![book]);

    g.apply_redo(
        vec![RedoOp::DropIndex {
            name: "titleIndex".into(),
        }],
        g.commit_ts() + 1,
    )
    .unwrap();
    assert!(g
        .lookup_string_contains_at(&["Book".into()], "title", "and", g.commit_ts())
        .is_none());
    assert_eq!(g.used_bytes(), before);
    assert_eq!(g.used_bytes(), g.accounted_bytes());

    let range = crate::redo::IndexDetailSpec {
        kind: crate::redo::IndexKindSpec::Range,
        relationship: false,
        labels: vec!["Place".into()],
        properties: vec!["n".into()],
        with_properties: vec![],
    };
    g.apply_redo(
        vec![RedoOp::CatalogIndexDetail {
            name: "nIndex".into(),
            detail: range,
            options: redo::props_json(&BTreeMap::new()),
        }],
        g.commit_ts() + 1,
    )
    .unwrap();
    let ids = g
        .lookup_range_seek(
            &["Place".into()],
            "n",
            &RangeSeek::Cmp {
                op: cypherlite_ir::BinaryOp::Gte,
                bound: Value::Int(4),
            },
            g.commit_ts(),
        )
        .unwrap();
    assert_eq!(ids, vec![place]);
    let ids = g.lookup_string_prefix_at(&["Place".into()], "name", "Ad", g.commit_ts());
    assert!(ids.is_none(), "name has no range index");

    let point = crate::redo::IndexDetailSpec {
        kind: crate::redo::IndexKindSpec::Point,
        relationship: false,
        labels: vec!["Place".into()],
        properties: vec!["loc".into()],
        with_properties: vec![],
    };
    g.apply_redo(
        vec![RedoOp::CatalogIndexDetail {
            name: "locIndex".into(),
            detail: point,
            options: redo::props_json(&BTreeMap::new()),
        }],
        g.commit_ts() + 1,
    )
    .unwrap();
    let ids = g
        .lookup_spatial_seek(
            &["Place".into()],
            "loc",
            &SpatialSeek::BBox {
                lo: Value::Point(cypherlite_core::PointValue {
                    srid: 7203,
                    x: 0.0,
                    y: 0.0,
                    z: None,
                }),
                hi: Value::Point(cypherlite_core::PointValue {
                    srid: 7203,
                    x: 2.0,
                    y: 2.0,
                    z: None,
                }),
            },
            g.commit_ts(),
        )
        .unwrap();
    assert_eq!(ids, vec![place]);
    assert_eq!(g.used_bytes(), g.accounted_bytes());
}

#[test]
fn mvcc_old_snapshot_hides_new_node() {
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    g.commit_write();
    assert_eq!(g.live_node_count(), 1);
    g.begin_write().unwrap();
    g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    assert_eq!(g.all_node_ids_at(g.commit_ts()).len(), 1);
    g.commit_write();
    assert_eq!(g.live_node_count(), 2);
}

#[test]
fn abort_then_apply_redo_is_the_commit() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    g.create_rel(a, b, "R".into(), BTreeMap::new()).unwrap();
    let ops = g.take_redo();
    let ts = g.pending_commit_ts();
    g.abort_write();
    assert_eq!(g.live_node_count(), 0);
    g.apply_redo(ops.clone(), ts).unwrap();
    assert_eq!(g.live_node_count(), 2);
    assert_eq!(g.neighbors(a, cypherlite_ir::Direction::Outgoing).len(), 1);
}

/// Commit the open write by aborting it and applying its redo at the
/// pending timestamp, as `abort_then_apply_redo_is_the_commit` pins down.
/// Returns the applied redo.
fn commit_via_replay(g: &Graph) -> Vec<RedoOp> {
    let ops = g.take_redo();
    let ts = g.pending_commit_ts();
    g.abort_write();
    g.apply_redo(ops.clone(), ts).unwrap();
    ops
}

#[test]
fn write_set_conflicts_on_later_set() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let id = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    g.set_property_node(id, "n", Value::Int(0));
    commit_via_replay(&g);

    let set = vec![RedoOp::SetNodeProp {
        id,
        key: "n".into(),
        value: Value::Int(1),
    }];
    assert!(!g.write_set_conflicts(g.commit_ts(), &set));
    assert!(g.write_set_conflicts(0, &set));
}

#[test]
fn write_set_allows_disjoint_create() {
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let ops = commit_via_replay(&g);

    let create = vec![RedoOp::CreateNode {
        id: 99,
        labels: Arc::new(vec!["B".into()]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    }];
    assert!(!g.write_set_conflicts(0, &create));
    assert!(g.write_set_conflicts(0, &ops));
}

#[test]
fn later_set_conflicts_with_set_not_with_disjoint_create() {
    let set_a = vec![RedoOp::SetNodeProp {
        id: 1,
        key: "n".into(),
        value: Value::Int(1),
    }];
    let set_b = vec![RedoOp::SetNodeProp {
        id: 1,
        key: "n".into(),
        value: Value::Int(2),
    }];
    let create = vec![RedoOp::CreateNode {
        id: 99,
        labels: Arc::new(vec!["B".into()]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    }];
    let rel = vec![RedoOp::CreateRel {
        id: 7,
        start: 1,
        end: 2,
        rel_type: "T".into(),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    }];
    assert!(Graph::redo_conflicts_with_later(&set_b, &set_a));
    assert!(!Graph::redo_conflicts_with_later(&create, &set_a));
    assert!(!Graph::redo_conflicts_with_later(&rel, &set_a));
    assert!(Graph::redo_conflicts_with_later(
        &rel,
        &[RedoOp::DeleteNode { id: 1 }]
    ));
}

#[test]
fn apply_does_not_change_pinned_snapshot() {
    use std::sync::Arc;
    let g = Graph::default();
    g.begin_write().unwrap();
    for i in 0..200 {
        g.create_node(
            vec!["A".into()],
            BTreeMap::from([("i".into(), Value::Int(i))]),
        )
        .unwrap();
    }
    commit_via_replay(&g);
    let pin = g.commit_ts();
    assert_eq!(g.all_node_ids_at(pin).len(), 200);

    let g = Arc::new(g);
    let reader = {
        let g = Arc::clone(&g);
        std::thread::spawn(move || {
            for _ in 0..50 {
                assert_eq!(g.all_node_ids_at(pin).len(), 200);
            }
        })
    };
    g.begin_write().unwrap();
    let extra = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    commit_via_replay(&g);
    reader.join().unwrap();
    assert_eq!(g.all_node_ids_at(pin).len(), 200);
    assert_eq!(g.live_node_count(), 201);
    assert!(g.node_at(extra, g.commit_ts()).is_some());
    assert!(g.node_at(extra, pin).is_none());
}

#[test]
fn gc_drops_superseded_version_after_pin_releases() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let id = g
        .create_node(
            vec!["A".into()],
            BTreeMap::from([("n".into(), Value::Int(1))]),
        )
        .unwrap();
    commit_via_replay(&g);

    let pin = g.pin_now().unwrap();
    g.begin_write().unwrap();
    g.set_property_node(id, "n", Value::Int(2));
    commit_via_replay(&g);

    g.gc_versions();
    assert_eq!(
        g.node_at(id, pin.ts()).unwrap().properties["n"],
        Value::Int(1)
    );
    assert_eq!(
        g.node_at(id, g.commit_ts()).unwrap().properties["n"],
        Value::Int(2)
    );
    drop(pin);
    g.gc_versions();
    assert!(g.node_at(id, 1).is_none());
    let found = g
        .lookup_nodes_at(
            &["A".into()],
            &BTreeMap::from([("n".into(), Value::Int(1))]),
            g.commit_ts(),
        )
        .unwrap();
    assert!(found.is_empty());
    let found = g
        .lookup_nodes_at(
            &["A".into()],
            &BTreeMap::from([("n".into(), Value::Int(2))]),
            g.commit_ts(),
        )
        .unwrap();
    assert_eq!(found, vec![id]);
}

#[test]
fn claiming_pin_blocks_gc_horizon() {
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    commit_via_replay(&g);
    assert_eq!(g.gc_horizon(), g.commit_ts());
    g.pins[0].store(PIN_CLAIMING, Ordering::Release);
    assert_eq!(g.gc_horizon(), 0);
    g.pins[0].store(PIN_FREE, Ordering::Release);
    assert_eq!(g.gc_horizon(), g.commit_ts());
}

#[test]
fn full_pin_table_fails_fast_with_snapshot_pin_and_recovers() {
    let g = Graph::default();
    let mut held = Vec::new();
    for _ in 0..PIN_SLOTS {
        held.push(g.pin_now().unwrap());
    }
    // Count the real acquisition loop's retries, not scheduler-dependent elapsed time.
    let mut retries = 0;
    let error = g
        .pin_now_with_retry(|| {
            retries += 1;
            assert!(
                retries <= PIN_ACQUIRE_RETRIES,
                "pin acquisition exceeded its retry budget"
            );
        })
        .err()
        .expect("a full pin table must fail");
    assert_eq!(retries, PIN_ACQUIRE_RETRIES);
    assert_eq!(
        error,
        cypherlite_core::Error::Execution(format!("snapshot pin: all {PIN_SLOTS} slots are held"))
    );
    // Releasing one pin lets the next acquisition succeed again (ADR-0111).
    held.pop();
    assert!(g.pin_now().is_ok());
}

#[test]
fn concurrent_pins_survive_gc() {
    use std::sync::{Arc, Barrier};
    let g = Graph::default();
    g.begin_write().unwrap();
    let id = g
        .create_node(
            vec!["A".into()],
            BTreeMap::from([("n".into(), Value::Int(1))]),
        )
        .unwrap();
    commit_via_replay(&g);

    let g = Arc::new(g);
    let held = g.pin_now().unwrap();
    let n = 4;
    let barrier = Arc::new(Barrier::new(n + 1));
    let mut readers = Vec::new();
    for _ in 0..n {
        let g = Arc::clone(&g);
        let barrier = Arc::clone(&barrier);
        readers.push(std::thread::spawn(move || {
            let pin = g.pin_now().unwrap();
            barrier.wait();
            assert_eq!(
                g.node_at(id, pin.ts()).unwrap().properties["n"],
                Value::Int(1)
            );
            barrier.wait();
            drop(pin);
        }));
    }
    barrier.wait();
    g.begin_write().unwrap();
    g.set_property_node(id, "n", Value::Int(2));
    commit_via_replay(&g);
    g.gc_versions();
    assert_eq!(
        g.node_at(id, held.ts()).unwrap().properties["n"],
        Value::Int(1)
    );
    barrier.wait();
    drop(held);
    for t in readers {
        t.join().unwrap();
    }
    g.gc_versions();
    assert_eq!(
        g.node_at(id, g.commit_ts()).unwrap().properties["n"],
        Value::Int(2)
    );
    assert!(g.node_at(id, 1).is_none());
}

#[test]
fn gc_removes_deleted_node_from_label_index() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let keep = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let gone = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    commit_via_replay(&g);

    g.begin_write().unwrap();
    g.delete_node(gone, true).unwrap();
    commit_via_replay(&g);

    g.gc_versions();
    let found = g
        .lookup_nodes_at(&["A".into()], &BTreeMap::new(), g.commit_ts())
        .unwrap();
    assert_eq!(found, vec![keep]);
    assert!(g.node_at(gone, g.commit_ts()).is_none());
}

#[test]
fn compact_transfer_payload_roundtrips() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(vec!["A".into()], BTreeMap::new()).unwrap();
    let b = g.create_node(vec!["B".into()], BTreeMap::new()).unwrap();
    let mut p = BTreeMap::new();
    p.insert("id".into(), Value::String("tx-1".into()));
    p.insert("amount".into(), Value::Float(12.5));
    p.insert("ts".into(), Value::Int(7));
    let rid = g.create_rel(a, b, "TRANSFER".into(), p).unwrap();
    g.commit_write();
    let r = g.rel(rid).unwrap();
    assert_eq!(r.rel_type, "TRANSFER");
    assert_eq!(r.properties.get("id"), Some(&Value::String("tx-1".into())));
    assert_eq!(r.properties.get("amount"), Some(&Value::Float(12.5)));
    assert_eq!(r.properties.get("ts"), Some(&Value::Int(7)));

    g.begin_write().unwrap();
    g.set_property_rel(rid, "note", Value::String("held".into()));
    g.commit_write();
    let r = g.rel(rid).unwrap();
    assert_eq!(
        r.properties.get("note"),
        Some(&Value::String("held".into()))
    );
    assert_eq!(r.properties.get("amount"), Some(&Value::Float(12.5)));
}

#[test]
fn stub_has_no_props_prop_lookup_does() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let mut p = BTreeMap::new();
    p.insert("name".into(), Value::String("Ada".into()));
    let id = g.create_node(vec!["P".into()], p).unwrap();
    g.commit_write();
    let stub = g.node_stub_at(id, g.commit_ts()).unwrap();
    assert!(stub.properties.is_empty());
    assert_eq!(
        g.node_prop_at(id, g.commit_ts(), "name"),
        Some(Value::String("Ada".into()))
    );
}

#[test]
fn label_stub_cache_is_charged_once() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let id = g
        .create_node(vec!["Person".into()], BTreeMap::new())
        .unwrap();
    g.commit_write();
    let before = g.used_bytes();
    let first = g.node_stub_at(id, g.commit_ts()).unwrap();
    let charged = g.used_bytes();
    assert!(charged > before, "before={before} charged={charged}");
    let second = g.node_stub_at(id, g.commit_ts()).unwrap();
    assert!(std::sync::Arc::ptr_eq(&first.labels, &second.labels));
    assert_eq!(g.used_bytes(), charged);
    assert_eq!(g.used_bytes(), g.accounted_bytes());
}

#[test]
fn used_bytes_matches_live_walk_after_mutate_and_delete() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let mut p = BTreeMap::new();
    p.insert("name".into(), Value::String("abcdefghij".into()));
    let a = g.create_node(vec!["Person".into()], p).unwrap();
    let b = g
        .create_node(vec!["Person".into()], BTreeMap::new())
        .unwrap();
    g.create_rel(a, b, "KNOWS".into(), BTreeMap::new()).unwrap();
    g.commit_write();
    assert_eq!(g.used_bytes(), g.accounted_bytes());
    assert!(g.used_bytes() > 0);

    g.begin_write().unwrap();
    g.set_property_node(a, "name", Value::String("x".into()));
    g.commit_write();
    assert_eq!(g.used_bytes(), g.accounted_bytes());

    g.begin_write().unwrap();
    g.delete_node(a, true).unwrap();
    g.commit_write();
    assert_eq!(g.used_bytes(), g.accounted_bytes());
}

/// A relationship stored as `Other` stays in property vectors when a later
/// `SET` makes the map look like `{id, amount, ts}`. `RelPayload` never
/// demotes `Other` to `Triple`, so the estimate and the incremental charge
/// have to keep those vectors or `used_bytes` falls behind the live walk.
#[test]
fn other_rel_that_gains_triple_keys_stays_on_the_floor() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g.create_node(Vec::new(), BTreeMap::new()).unwrap();
    let b = g.create_node(Vec::new(), BTreeMap::new()).unwrap();
    g.commit_write();

    let partial = BTreeMap::from([
        ("id".into(), Value::String("a".into())),
        ("amount".into(), Value::Int(1)),
    ]);
    let same_batch = vec![
        RedoOp::CreateRel {
            id: 1,
            start: a,
            end: b,
            rel_type: "PAYS".into(),
            properties: cypherlite_core::PropertyMap::share(partial),
        },
        RedoOp::SetRelProp {
            id: 1,
            key: "ts".into(),
            value: Value::Int(5),
        },
    ];
    let before = g.used_bytes();
    let estimate = g.estimate_redo_delta(&same_batch);
    g.apply_redo(same_batch, g.commit_ts() + 1).unwrap();
    assert_eq!(g.used_bytes(), g.accounted_bytes());
    assert_eq!(g.used_bytes() as i64 - before as i64, estimate);

    let before = g.used_bytes();
    let set_again = vec![RedoOp::SetRelProp {
        id: 1,
        key: "note".into(),
        value: Value::String("x".into()),
    }];
    let estimate = g.estimate_redo_delta(&set_again);
    g.apply_redo(set_again, g.commit_ts() + 1).unwrap();
    assert_eq!(g.used_bytes(), g.accounted_bytes());
    assert_eq!(g.used_bytes() as i64 - before as i64, estimate);

    let triple = BTreeMap::from([
        ("id".into(), Value::String("b".into())),
        ("amount".into(), Value::Int(2)),
        ("ts".into(), Value::Int(9)),
    ]);
    let created = vec![RedoOp::CreateRel {
        id: 2,
        start: a,
        end: b,
        rel_type: "PAYS".into(),
        properties: cypherlite_core::PropertyMap::share(triple),
    }];
    let before = g.used_bytes();
    let estimate = g.estimate_redo_delta(&created);
    g.apply_redo(created, g.commit_ts() + 1).unwrap();
    assert_eq!(g.used_bytes(), g.accounted_bytes());
    assert_eq!(g.used_bytes() as i64 - before as i64, estimate);
    assert_eq!(g.rel(1).unwrap().properties.len(), 4);
    assert_eq!(g.rel(2).unwrap().properties.len(), 3);
}

/// LABS-370 (security follow-up): the redo-delta estimate includes the
/// count family's entry growth — one rel between two multi-label nodes
/// materializes labels(start) × labels(end) pair entries plus the
/// start-side histogram entries, and a batch creating labeled nodes
/// materializes label entries — all charged BEFORE the commit applies, so
/// an oversized-label write is rejected at admission, not after its
/// entries land past the cap. Exact against the applied delta.
#[test]
fn estimate_redo_delta_includes_stats_entry_growth() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let a = g
        .create_node(vec!["X1".into(), "X2".into()], BTreeMap::new())
        .unwrap();
    let b = g
        .create_node(vec!["Y1".into(), "Y2".into()], BTreeMap::new())
        .unwrap();
    g.commit_write();
    let rel = vec![RedoOp::CreateRel {
        id: 1,
        start: a,
        end: b,
        rel_type: "KNOWS".into(),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    }];
    let before = g.used_bytes();
    let estimate = g.estimate_redo_delta(&rel);
    g.apply_redo(rel, g.commit_ts() + 1).unwrap();
    assert_eq!(g.used_bytes(), g.accounted_bytes());
    assert_eq!(g.used_bytes() as i64 - before as i64, estimate);
    // The stats term dominates the rel's own bytes: 2×2 = 4 pair entries
    // plus 2 (start-label, type) histogram entries.
    assert!(
        estimate >= (4 * Graph::PAIR_COUNT_ENTRY_BYTES + 2 * Graph::HISTOGRAM_ENTRY_BYTES) as i64
    );
    // A batch creating labeled nodes grows the label-count map too.
    let node = vec![RedoOp::CreateNode {
        id: 900,
        labels: Arc::new(vec!["Z".into()]),
        properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
    }];
    let before = g.used_bytes();
    let estimate = g.estimate_redo_delta(&node);
    g.apply_redo(node, g.commit_ts() + 1).unwrap();
    assert_eq!(g.used_bytes(), g.accounted_bytes());
    assert_eq!(g.used_bytes() as i64 - before as i64, estimate);
    assert!(estimate >= Graph::LABEL_COUNT_ENTRY_BYTES as i64);
}

/// A batch that creates an index and, in the same batch, deletes, rewrites,
/// and creates nodes under it must estimate exactly what applying it costs:
/// the deleted node stays charged until GC, the rewritten one keeps its old
/// version and adds a new one at the new charge, the untouched one picks up
/// the new charge, and the created one is charged from the start.
#[test]
fn estimate_redo_delta_is_exact_for_ddl_mixed_with_writes() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let title = |s: &str| BTreeMap::from([("title".into(), Value::String(s.into()))]);
    let doomed = g
        .create_node(vec!["Book".into()], title("Gone With the Wind"))
        .unwrap();
    let rewritten = g.create_node(vec!["Book".into()], title("Emma")).unwrap();
    g.create_node(vec!["Book".into()], title("War and Peace"))
        .unwrap();
    g.create_node(vec!["Person".into()], title("not indexed"))
        .unwrap();
    g.commit_write();
    let before = g.used_bytes();
    assert_eq!(before, g.accounted_bytes());

    let ops = vec![
        RedoOp::CatalogIndexDetail {
            name: "titleIndex".into(),
            detail: crate::redo::IndexDetailSpec {
                kind: crate::redo::IndexKindSpec::Text,
                relationship: false,
                labels: vec!["Book".into()],
                properties: vec!["title".into()],
                with_properties: vec![],
            },
            options: redo::props_json(&BTreeMap::new()),
        },
        RedoOp::DeleteNode { id: doomed },
        RedoOp::SetNodeProp {
            id: rewritten,
            key: "title".into(),
            value: Value::String("Emma, or the Misfortunes of Virtue".into()),
        },
        RedoOp::CreateNode {
            id: 1_000,
            labels: Arc::new(vec!["Book".into()]),
            properties: cypherlite_core::PropertyMap::share(title("Middlemarch")),
        },
    ];
    let estimate = g.estimate_redo_delta(&ops);
    g.apply_redo(ops, g.commit_ts() + 1).unwrap();
    assert_eq!(g.used_bytes(), g.accounted_bytes());
    assert_eq!(g.used_bytes() as i64 - before as i64, estimate);
}

fn vector_index_detail() -> crate::redo::IndexDetailSpec {
    crate::redo::IndexDetailSpec {
        kind: crate::redo::IndexKindSpec::Vector,
        relationship: false,
        labels: vec!["Doc".into()],
        properties: vec!["embedding".into()],
        with_properties: vec!["ttl".into()],
    }
}

#[test]
fn apply_redo_catalog_index_detail_stores_the_typed_model() {
    let g = Graph::default();
    let detail = vector_index_detail();
    let op = RedoOp::CatalogIndexDetail {
        name: "vec_idx".into(),
        detail: detail.clone(),
        options: redo::props_json(&BTreeMap::from([(
            "indexConfig".to_string(),
            Value::Map(
                BTreeMap::from([("vector.dimensions".to_string(), Value::Int(1536))]).into(),
            ),
        )])),
    };
    g.apply_redo(vec![op], 1).unwrap();
    let theater = g.theater.lock().unwrap();
    assert_eq!(theater.index_details.get("vec_idx"), Some(&detail));
    assert_eq!(
        theater.index_options["vec_idx"]["indexConfig"],
        Value::Map(BTreeMap::from([("vector.dimensions".to_string(), Value::Int(1536))]).into())
    );
}

#[test]
fn apply_redo_malformed_index_detail_options_returns_err() {
    let g = Graph::default();
    let op = RedoOp::CatalogIndexDetail {
        name: "idx".into(),
        detail: crate::redo::IndexDetailSpec {
            kind: crate::redo::IndexKindSpec::Range,
            relationship: false,
            labels: vec!["L".into()],
            properties: vec!["p".into()],
            with_properties: vec![],
        },
        options: serde_json::json!("corrupted"),
    };
    assert!(g.apply_redo(vec![op], 1).is_err());
    // Nothing was silently stored: the decode failure aborts the op.
    let theater = g.theater.lock().unwrap();
    assert!(theater.index_details.is_empty());
    assert!(theater.index_options.is_empty());
}

#[test]
fn apply_redo_malformed_index_detail_option_value_returns_err() {
    let g = Graph::default();
    let op = RedoOp::CatalogIndexDetail {
        name: "idx".into(),
        detail: vector_index_detail(),
        options: serde_json::json!({"indexConfig": {"not": "tagged"}}),
    };
    assert!(g.apply_redo(vec![op], 1).is_err());
    assert!(g.theater.lock().unwrap().index_details.is_empty());
}

#[test]
fn apply_redo_graph_type_ops_store_the_typed_model() {
    let g = Graph::default();
    let stmt = cypherlite_parser::parse(
        "CREATE GRAPH TYPE gt {
            (s:Site => {name :: STRING IS KEY}),
            (s)-[:LINK => {since :: DATE}]->(s)
        }",
    )
    .expect("parses");
    let Some(cypherlite_parser::Clause::CreateGraphType(cgt)) = stmt.clauses.first() else {
        panic!("expected CreateGraphType");
    };
    let spec =
        crate::graph_type::GraphTypeSpec::from_body(&cgt.body).expect("shape-validated body");

    g.apply_redo(
        vec![RedoOp::PutGraphType {
            name: "gt".into(),
            body: spec.clone(),
        }],
        1,
    )
    .unwrap();
    assert_eq!(g.theater.lock().unwrap().graph_types.get("gt"), Some(&spec));

    g.apply_redo(vec![RedoOp::SetCurrentGraphType { body: spec }], 2)
        .unwrap();
    let theater = g.theater.lock().unwrap();
    assert_eq!(theater.current_graph_type.to_body(), cgt.body);
    // The derived constraint set is recomputed from the carried model.
    // (`cypherlite_model::Constraint` has no PartialEq; compare the
    // stable projection.)
    let project = |m: &BTreeMap<String, cypherlite_model::Constraint>| {
        m.values()
            .map(|c| {
                (
                    c.name.clone(),
                    c.label.clone(),
                    c.properties.clone(),
                    c.constraint_type.as_str().to_string(),
                    c.property_type.clone(),
                    c.entity_type.clone(),
                    c.enforced_label.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    let expected: BTreeMap<_, _> = crate::graph_type::derive_constraints(&cgt.body)
        .expect("a validated stored body derives")
        .into_iter()
        .map(|c| (c.name.clone(), c))
        .collect();
    assert_eq!(project(&theater.derived_constraints), project(&expected));
    drop(theater);
    assert_eq!(
        g.graph_type_spec()
            .expect("a stored validated spec canonicalizes")
            .as_deref(),
        Some(
            crate::graph_type::canonicalize_graph_type_body(&cgt.body)
                .expect("a validated body canonicalizes")
                .as_str()
        )
    );
}

#[test]
fn apply_redo_graph_type_model_needs_no_parse() {
    // A body whose canonical string form would not re-parse to the
    // same state (non-literal OPTIONS rendered as `?`) still replays:
    // the typed model is stored verbatim.
    let g = Graph::default();
    let stmt = cypherlite_parser::parse(
        "CREATE GRAPH TYPE gt AS {
            CONSTRAINT FOR (p:Foo) REQUIRE p.a IS KEY OPTIONS {x: 1 + 1}
        }",
    )
    .expect("parses");
    let Some(cypherlite_parser::Clause::CreateGraphType(cgt)) = stmt.clauses.first() else {
        panic!("expected CreateGraphType");
    };
    let spec =
        crate::graph_type::GraphTypeSpec::from_body(&cgt.body).expect("shape-validated body");
    g.apply_redo(
        vec![RedoOp::PutGraphType {
            name: "gt".into(),
            body: spec.clone(),
        }],
        1,
    )
    .unwrap();
    let stored = g.theater.lock().unwrap().graph_types.get("gt").cloned();
    assert_eq!(stored, Some(spec));
}

#[test]
fn pull_identity_put_survives_apply_and_drops_on_delete() {
    let g = Graph::default();
    g.apply_redo(
        vec![
            RedoOp::CreateNode {
                id: 1,
                labels: Arc::new(vec!["Person".into()]),
                properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
            },
            RedoOp::PullIdentityPut {
                source: "http://src/db/neo4j/query/v2".into(),
                kind: crate::redo::PullKind::Node,
                remote_id: "4:aaa:0".into(),
                local_id: 1,
            },
        ],
        1,
    )
    .unwrap();
    assert_eq!(
        g.pull_identity_get(
            "http://src/db/neo4j/query/v2",
            crate::redo::PullKind::Node,
            "4:aaa:0"
        ),
        Some(1)
    );
    g.apply_redo(vec![RedoOp::DeleteNode { id: 1 }], 2).unwrap();
    assert_eq!(
        g.pull_identity_get(
            "http://src/db/neo4j/query/v2",
            crate::redo::PullKind::Node,
            "4:aaa:0"
        ),
        None
    );
}

#[test]
fn range_index_increases_the_node_floor() {
    let plain = {
        let g = Graph::default();
        g.begin_write().unwrap();
        g.create_node(
            vec!["Flat".into()],
            BTreeMap::from([("id".into(), Value::Int(1))]),
        )
        .unwrap();
        g.commit_write();
        g.used_bytes()
    };
    let g = Graph::default();
    g.apply_redo(
        vec![RedoOp::CatalogIndexDetail {
            name: "Flat_id".into(),
            detail: crate::redo::IndexDetailSpec {
                kind: crate::redo::IndexKindSpec::Range,
                relationship: false,
                labels: vec!["Flat".into()],
                properties: vec!["id".into()],
                with_properties: vec![],
            },
            options: redo::props_json(&BTreeMap::new()),
        }],
        1,
    )
    .unwrap();
    g.begin_write().unwrap();
    g.create_node(
        vec!["Flat".into()],
        BTreeMap::from([("id".into(), Value::Int(1))]),
    )
    .unwrap();
    g.commit_write();
    let indexed = g.used_bytes();
    assert!(
        indexed > plain,
        "plain={plain} indexed={indexed} delta={}",
        indexed as i64 - plain as i64
    );
}

#[test]
fn floor_reserves_node_ring_edge_and_dense_node() {
    let g = Graph::default();
    assert_eq!(g.used_bytes(), g.accounted_bytes());

    g.begin_write().unwrap();
    let hub = g
        .create_node(
            vec!["Dense".into()],
            BTreeMap::from([("id".into(), Value::Int(0))]),
        )
        .unwrap();
    let mut leaves = Vec::new();
    for i in 1..=20 {
        leaves.push(
            g.create_node(
                vec!["Dense".into()],
                BTreeMap::from([("id".into(), Value::Int(i))]),
            )
            .unwrap(),
        );
    }
    let ring_a = g
        .create_node(
            vec!["Ring".into()],
            BTreeMap::from([("id".into(), Value::Int(0))]),
        )
        .unwrap();
    let ring_b = g
        .create_node(
            vec!["Ring".into()],
            BTreeMap::from([("id".into(), Value::Int(1))]),
        )
        .unwrap();
    g.create_rel(ring_a, ring_b, "NEXT".into(), BTreeMap::new())
        .unwrap();
    for leaf in &leaves {
        g.create_rel(hub, *leaf, "LINK".into(), BTreeMap::new())
            .unwrap();
    }
    g.commit_write();
    assert_eq!(g.used_bytes(), g.accounted_bytes());

    let node = g.nodes.get(&hub).unwrap();
    assert_eq!(node.len(), 1);
    assert_eq!(node.capacity(), crate::memory::reserved_len(1));
    let live = &node.last().unwrap().live;
    assert_eq!(live.labels.len(), 1);
    assert_eq!(live.labels.capacity(), crate::memory::reserved_len(1));
    assert_eq!(live.props.len(), 1);
    assert_eq!(
        live.props.key_capacity(),
        crate::memory::reserved_len(live.props.len())
    );
    assert_eq!(
        live.props.val_capacity(),
        crate::memory::reserved_len(live.props.len())
    );

    let ring = g.out_adj.get(&ring_a).unwrap();
    assert_eq!(ring.chunks.len(), 1);
    assert_eq!(ring.chunks.capacity(), crate::memory::reserved_len(1));
    assert_eq!(ring.chunks[0].1.len(), 1);
    assert_eq!(ring.chunks[0].1.capacity(), crate::memory::reserved_len(1));

    let dense = g.out_adj.get(&hub).unwrap();
    assert_eq!(dense.chunks.len(), 1);
    assert_eq!(dense.chunks.capacity(), crate::memory::reserved_len(1));
    assert_eq!(dense.chunks[0].1.len(), 20);
    assert_eq!(
        dense.chunks[0].1.capacity(),
        crate::memory::reserved_len(20)
    );
}

// ---------------------------------------------------------------------
// ADR-0388 review (LABS-386): persisted graph-type bytes that never ran
// the body validator
// ---------------------------------------------------------------------

/// The hand-crafted persisted bytes of a poisoned graph type: a
/// `Constructor` default whose name the CIP-262 table never admitted --
/// the bytes an enabled follower of the gated trio could store, and this
/// engine then deserializes at WAL replay or checkpoint verification.
const POISONED_SPEC_BYTES: &str = r#"{
    "node_types": [{
        "alias": null,
        "identifying_label": "L",
        "implied_labels": [],
        "properties": [{
            "name": "p",
            "value_type": {"Predefined": {"kind": "String", "not_null": false}},
            "default_value": {"Constructor": {"name": "toString", "args": []}},
            "inline_constraint": null
        }]
    }]
}"#;

/// The poisoned spec decoded once for the family's five siblings.
fn poisoned_spec() -> crate::graph_type::GraphTypeSpec {
    serde_json::from_str(POISONED_SPEC_BYTES).expect("hand-crafted bytes decode")
}

/// The one assertion every poisoned-spec sibling makes: the driven path
/// fails with the typed storage-corruption error, never a panic.
fn assert_poisoned_storage_error<T: std::fmt::Debug, E: std::fmt::Display>(
    result: std::result::Result<T, E>,
) {
    let error = result.expect_err("a poisoned persisted default fails typed, not panic");
    assert!(
        error
            .to_string()
            .contains("persisted graph type carries an invalid default value"),
        "expected the typed storage-corruption error, got: {error}"
    );
}

#[test]
fn replay_of_a_poisoned_persisted_default_fails_typed_not_panic() {
    // ADR-0388 review (LABS-386): a gated construct stored by an enabled
    // follower, deserialized from persisted bytes, never ran
    // `validate_graph_type_body`. Committed replay -- WAL redo preflight
    // then apply, the `Database::open` catch-up path -- must surface it
    // as the typed storage-corruption error, never a panic.
    let graph = Graph::default();
    assert_poisoned_storage_error(graph.apply_redo(
        vec![crate::redo::RedoOp::SetCurrentGraphType {
            body: poisoned_spec(),
        }],
        1,
    ));
}

#[test]
fn visualize_of_a_poisoned_persisted_default_fails_typed_not_panic() {
    // The AS GRAPH sibling of the poisoned-spec family (the rust
    // review's High): the same hand-crafted bytes driving the SHOW and
    // checkpoint paths, through the SHOW ... AS GRAPH renderer -- typed
    // storage error, never the visualization panic it used to be.
    let spec: crate::graph_type::GraphTypeSpec =
        serde_json::from_str(POISONED_SPEC_BYTES).expect("hand-crafted persisted bytes decode");
    let body = spec.to_body();
    assert_poisoned_storage_error(crate::graph_type_visualize::render_graph_type_as_graph(
        &body,
        &[],
        cypherlite_parser::AnyNodeMode::Single,
    ));
}

#[test]
fn json_restore_of_a_poisoned_persisted_default_fails_typed_not_panic() {
    // The JSON-snapshot restore sibling (the round-7 finding): the third
    // persisted-spec entry point runs the graph-type validator with the
    // store's preview set before install -- typed family error, never a
    // silently-installed poisoned body.
    let graph = Graph::default();
    // The restore entry takes the body-text form ({ <body> }), the same
    // poisoned default as the serde bytes' `toString` constructor.
    assert_poisoned_storage_error(graph.restore_graph_type_spec(Some(
        "{ (:L => {p :: STRING DEFAULT ON COMMIT toString()})}",
    )));
}

#[test]
fn checkpoint_verify_of_a_poisoned_persisted_default_fails_typed_not_panic() {
    // The checkpoint-verification sibling of the replay test: the same
    // hand-crafted persisted bytes as a checkpoint theater image, restored
    // through the same verify path `Database::open` runs -- typed error,
    // never a panic.
    let image: super::theater::CheckpointTheater = serde_json::from_str(&format!(
        r#"{{"databases": {{}}, "index_details": {{}}, "index_options": {{}}, "graph_types": {{}}, "current_graph_type": {POISONED_SPEC_BYTES}, "derived_constraints": {{}}}}"#
    ))
    .expect("hand-crafted checkpoint bytes decode");
    let graph = Graph::default();
    assert_poisoned_storage_error(graph.restore_checkpoint_theater(image));
}

#[test]
fn checkpoint_writer_of_a_poisoned_persisted_default_fails_typed_not_panic() {
    // The checkpoint-writer sibling of the replay test: the snapshot
    // codec's spec string over a poisoned theater model (the bytes an
    // enabled follower could commit before this engine's replay-time
    // reject) is the same typed storage-corruption error, never a
    // panic.
    let graph = Graph::default();
    graph.theater.lock().unwrap().current_graph_type = poisoned_spec();
    assert_poisoned_storage_error(graph.graph_type_spec());
}

#[test]
fn show_graph_type_of_a_poisoned_persisted_default_fails_typed_not_panic() {
    // The SHOW GRAPH TYPE sibling: the stored-spec render path maps the
    // same poisoned bytes to the same typed error, never a panic.
    let graph = Graph::default();
    graph
        .theater
        .lock()
        .unwrap()
        .graph_types
        .insert("gt".to_string(), poisoned_spec());
    let view = crate::overlay::ExecGraph::read(&graph);
    let clause = cypherlite_parser::ShowGraphTypeClause {
        name: "gt".to_string(),
        yields: Default::default(),
    };
    assert_poisoned_storage_error(crate::exec::catalog_show::show_graph_type_rows(
        &view, &clause,
    ));
}

#[test]
fn show_current_graph_type_of_a_poisoned_persisted_default_fails_typed_not_panic() {
    // The SHOW CURRENT GRAPH TYPE sibling: the stored-spec render path
    // maps the same poisoned bytes to the same typed error, never a
    // panic.
    let graph = Graph::default();
    graph.theater.lock().unwrap().current_graph_type = poisoned_spec();
    let view = crate::overlay::ExecGraph::read(&graph);
    let clause = cypherlite_parser::ShowCurrentGraphTypeClause {
        yields: Default::default(),
        stages: Vec::new(),
        where_expr: None,
        as_graph: None,
    };
    assert_poisoned_storage_error(crate::exec::catalog_show::show_current_graph_type_rows(
        &view, &clause,
    ));
}
