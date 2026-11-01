use super::*;
use crate::memory_store_with_codec;
use crate::snapshot::get_object;
use cypherlite_catalog::ConstraintDef;
use cypherlite_core::temporal::parse_iso;
use cypherlite_core::{PointValue, Temporal, Value, VectorCoordKind};
use cypherlite_executor::RedoOp;
use cypherlite_model::ConstraintType;
use std::collections::BTreeMap;

#[path = "tests/corrupt_reads.rs"]
mod corrupt_reads;
#[path = "tests/failed_writes.rs"]
mod failed_writes;
#[path = "tests/fixtures.rs"]
mod fixtures;
#[path = "tests/roundtrip.rs"]
mod roundtrip;

// Persistence hardening merged from the storage-hardening branch: the typed
// checkpoint theater, legacy-unknown enforcement markers, and the boundary
// guards for exhausted timestamps and negative ID cursors.

/// ADR-0450: the JSON row carries the enforced label (the write side
/// sources it from the theater). A checkpoint roundtrip installs that value
/// into the catalog row instead of dropping it to `None`, so the reloaded
/// graph's constraint identity matches the stored one.
#[test]
fn json_checkpoint_roundtrip_preserves_the_enforced_label() {
    let target = memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    let graph = Graph::default();
    let def = ConstraintDef {
        name: "rule".into(),
        relationship: false,
        label: "Person".into(),
        properties: vec![],
        kind: "nodeLabelExistence".into(),
        property_type: None,
        enforced_label: Some("Verified".into()),
    };
    graph
        .apply_redo(
            vec![
                RedoOp::create_constraint(def.clone()),
                RedoOp::CatalogDerivedConstraint {
                    constraint: def.to_model(ConstraintType::NodeLabelExistence),
                },
            ],
            1,
        )
        .unwrap();
    dump(&target, &graph, 1, 1, None).unwrap();
    let loaded = load(&target).unwrap();
    let def = loaded
        .named_constraints()
        .into_iter()
        .find(|def| def.name == "rule")
        .expect("the roundtrip keeps the constraint");
    assert_eq!(def.enforced_label.as_deref(), Some("Verified"));
}

#[test]
fn json_typed_theater_requires_every_named_enforced_label() {
    let target = memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    let graph = Graph::default();
    graph
        .apply_redo(
            vec![
                RedoOp::create_constraint(ConstraintDef {
                    name: "rule".into(),
                    relationship: false,
                    label: "Person".into(),
                    properties: vec![],
                    kind: "nodeLabelExistence".into(),
                    property_type: None,
                    enforced_label: None,
                }),
                RedoOp::CatalogDerivedConstraint {
                    constraint: cypherlite_model::Constraint {
                        name: "rule".into(),
                        label: "Person".into(),
                        properties: vec![],
                        constraint_type: ConstraintType::NodeLabelExistence,
                        property_type: None,
                        entity_type: "NODE".into(),
                        enforced_label: Some("Verified".into()),
                    },
                },
            ],
            1,
        )
        .unwrap();
    dump(&target, &graph, 1, 1, None).unwrap();
    let original: serde_json::Value =
        serde_json::from_slice(&get_object(&target, GRAPH_JSON).unwrap()).unwrap();
    assert_eq!(
        load(&target)
            .unwrap()
            .checkpoint_theater()
            .enforced_label("rule"),
        Some("Verified")
    );

    let mut incomplete = original.clone();
    incomplete["theater"]["derived_constraints"]
        .as_object_mut()
        .unwrap()
        .remove("rule");
    put_object(
        &target,
        GRAPH_JSON,
        &serde_json::to_vec(&incomplete).unwrap(),
    )
    .unwrap();
    assert!(matches!(load(&target), Err(Error::Storage(_))));

    let mut legacy = original;
    legacy.as_object_mut().unwrap().remove("theater");
    put_object(&target, GRAPH_JSON, &serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(
        load(&target)
            .unwrap()
            .checkpoint_theater()
            .enforced_label("rule"),
        Some("Verified")
    );
}

#[test]
fn json_legacy_enforced_label_import_and_invalid_theater() {
    let target = memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    let legacy = serde_json::json!({
        "nodes": [], "relationships": [], "next_node_id": 0, "next_rel_id": 0,
        "constraints": [{"name":"rule", "label":"Person", "properties":[],
            "type":"nodeLabelExistence", "enforced_label":"Verified"}]
    });
    put_object(&target, GRAPH_JSON, &serde_json::to_vec(&legacy).unwrap()).unwrap();
    let imported = load(&target).unwrap();
    assert_eq!(
        imported.checkpoint_theater().enforced_label("rule"),
        Some("Verified")
    );
    dump(&target, &imported, 1, 0, None).unwrap();
    let mut mismatch: serde_json::Value =
        serde_json::from_slice(&get_object(&target, GRAPH_JSON).unwrap()).unwrap();
    mismatch["constraints"][0]["enforced_label"] = serde_json::json!("Other");
    put_object(&target, GRAPH_JSON, &serde_json::to_vec(&mismatch).unwrap()).unwrap();
    assert!(matches!(load(&target), Err(Error::Storage(_))));
    let mut corrupt = legacy.clone();
    corrupt["theater"] = serde_json::Value::Null;
    put_object(&target, GRAPH_JSON, &serde_json::to_vec(&corrupt).unwrap()).unwrap();
    assert!(matches!(load(&target), Err(Error::Storage(_))));

    let mut graph = legacy;
    graph.as_object_mut().unwrap().remove("constraints");
    put_object(&target, GRAPH_JSON, &serde_json::to_vec(&graph).unwrap()).unwrap();
    assert_eq!(load(&target).unwrap().named_constraints().len(), 0);
}

#[test]
fn load_rejects_max_manifest_timestamp_before_replay() {
    let target = memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    for has_graph in [false, true] {
        if has_graph {
            put_object(
                &target,
                GRAPH_JSON,
                br#"{"nodes":[],"relationships":[],"next_node_id":0,"next_rel_id":0}"#,
            )
            .unwrap();
        }
        for ts in [u64::MAX, u64::MAX - 1] {
            let manifest = Envelope {
                codec: SnapshotCodecKind::Json,
                checkpoint_lsn: 0,
                commit_ts: ts,
            };
            put_object(
                &target,
                "manifest.json",
                &serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            if ts == u64::MAX {
                let err = load(&target).unwrap_err();
                assert!(matches!(err, Error::Storage(_)), "{err}");
                assert!(err.to_string().contains("timestamp"), "{err}");
            } else {
                let graph = load(&target).unwrap();
                assert_eq!(graph.commit_ts(), if has_graph { ts } else { 0 });
            }
        }
    }
}

#[test]
fn dump_rejects_max_timestamp_without_changing_snapshot() {
    let target = memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    let graph = Graph::default();
    graph
        .create_node(vec!["First".into()], Default::default())
        .unwrap();
    dump(&target, &graph, 5, 1, None).unwrap();
    let body = get_object(&target, GRAPH_JSON).unwrap();
    let manifest = get_object(&target, "manifest.json").unwrap();
    graph
        .create_node(vec!["Second".into()], Default::default())
        .unwrap();
    assert_eq!(graph.dirty_node_ids().len(), 1);

    let err = dump(&target, &graph, 6, u64::MAX, None).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert_eq!(get_object(&target, GRAPH_JSON).unwrap(), body);
    assert_eq!(get_object(&target, "manifest.json").unwrap(), manifest);
    assert_eq!(graph.dirty_node_ids().len(), 1);
    assert_eq!(load(&target).unwrap().live_node_count(), 1);
}

#[test]
fn dump_rejects_negative_next_ids_without_replacing_checkpoint() {
    let target = memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    let graph = Graph::default();
    graph
        .create_node(vec!["First".into()], Default::default())
        .unwrap();
    dump(&target, &graph, 5, 1, None).unwrap();
    let body = get_object(&target, GRAPH_JSON).unwrap();
    let manifest = get_object(&target, "manifest.json").unwrap();
    graph
        .create_node(vec!["Second".into()], Default::default())
        .unwrap();

    for (node, rel) in [(-1, 0), (2, -1)] {
        graph.set_next_node_id(node);
        graph.set_next_rel_id(rel);
        let err = dump(&target, &graph, 6, 2, None).unwrap_err();
        assert!(matches!(err, Error::Storage(_)), "{err}");
        assert!(err.to_string().contains("next ID"), "{err}");
        assert_eq!(get_object(&target, GRAPH_JSON).unwrap(), body);
        assert_eq!(get_object(&target, "manifest.json").unwrap(), manifest);
        assert_eq!(graph.dirty_node_ids().len(), 1);
        assert_eq!(load(&target).unwrap().live_node_count(), 1);
    }
}

/// A constraint row written before `ConstraintDef` carried the enforced
/// label decodes it as `None` (the legacy identity-unknown policy), a
/// current row round-trips it, and a non-enforced row keeps the pre-field
/// byte shape (no `enforced_label` key).
#[test]
fn constraint_def_enforced_label_is_an_optional_durable_field() {
    let legacy: cypherlite_catalog::ConstraintDef = serde_json::from_str(
        r#"{"name":"rule","relationship":false,"label":"Person","properties":[],"kind":"nodeLabelExistence","property_type":null}"#,
    )
    .unwrap();
    assert_eq!(legacy.enforced_label, None);

    let current = cypherlite_catalog::ConstraintDef {
        enforced_label: Some("Verified".into()),
        ..legacy
    };
    let json = serde_json::to_string(&current).unwrap();
    assert!(json.contains(r#""enforced_label":"Verified""#));
    let decoded: cypherlite_catalog::ConstraintDef = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, current);

    let unique = cypherlite_catalog::ConstraintDef {
        name: "u".into(),
        relationship: false,
        label: "P".into(),
        properties: vec!["id".into()],
        kind: "unique".into(),
        property_type: None,
        enforced_label: None,
    };
    assert!(!serde_json::to_string(&unique)
        .unwrap()
        .contains("enforced_label"));
}
