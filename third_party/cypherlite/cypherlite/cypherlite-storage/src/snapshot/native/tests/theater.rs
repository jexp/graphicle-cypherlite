use super::super::*;
use super::harness::checkpoint_at;
use super::harness::*;
use crate::memory_store;
use cypherlite_executor::{Graph, RedoOp};

#[test]
fn parquet_checkpoint_theater_rejects_corrupt_payload_and_accepts_legacy_absence() {
    let target = memory_store();
    let graph = Graph::default();
    graph
        .apply_redo(
            vec![
                RedoOp::CreateConstraint {
                    name: "rule".into(),
                    relationship: false,
                    label: "Person".into(),
                    properties: vec![],
                    kind: "nodeLabelExistence".into(),
                    property_type: None,
                },
                RedoOp::CatalogDerivedConstraint {
                    constraint: cypherlite_model::Constraint {
                        name: "rule".into(),
                        label: "Person".into(),
                        properties: vec![],
                        constraint_type: cypherlite_model::ConstraintType::NodeLabelExistence,
                        property_type: None,
                        entity_type: "NODE".into(),
                        enforced_label: Some("Verified".into()),
                    },
                },
                RedoOp::CreateConstraint {
                    name: "source_rule".into(),
                    relationship: true,
                    label: "KNOWS".into(),
                    properties: vec![],
                    kind: "sourceLabelExistence".into(),
                    property_type: None,
                },
                RedoOp::CatalogDerivedConstraint {
                    constraint: cypherlite_model::Constraint {
                        name: "source_rule".into(),
                        label: "KNOWS".into(),
                        properties: vec![],
                        constraint_type: cypherlite_model::ConstraintType::SourceLabelExistence,
                        property_type: None,
                        entity_type: "RELATIONSHIP".into(),
                        enforced_label: Some("Verified".into()),
                    },
                },
            ],
            1,
        )
        .unwrap();
    checkpoint_at(&target, &graph, 1, 1, DEFAULT_SHARD_TARGET_BYTES).unwrap();
    let bytes = get_object(&target, MANIFEST_JSON).unwrap();
    let original: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        load(&target)
            .unwrap()
            .checkpoint_theater()
            .enforced_label("rule"),
        Some("Verified")
    );
    assert_eq!(
        load(&target)
            .unwrap()
            .checkpoint_theater()
            .enforced_label("source_rule"),
        Some("Verified")
    );

    for name in ["rule", "source_rule"] {
        let mut incomplete = original.clone();
        incomplete["theater"]["derived_constraints"]
            .as_object_mut()
            .unwrap()
            .remove(name);
        put_manifest(&target, &incomplete);
        assert!(matches!(load(&target), Err(Error::Storage(_))), "{name}");
    }
    let mut wrong_schema = original.clone();
    wrong_schema["theater"]["derived_constraints"]["source_rule"]["label"] =
        serde_json::json!("OTHER");
    put_manifest(&target, &wrong_schema);
    assert!(matches!(load(&target), Err(Error::Storage(_))));

    for bad in [
        serde_json::Value::Null,
        serde_json::json!({"databases": {}}),
    ] {
        let mut manifest = original.clone();
        manifest["theater"] = bad;
        put_manifest(&target, &manifest);
        assert!(matches!(load(&target), Err(Error::Storage(_))));
    }
    let mut inconsistent = original.clone();
    inconsistent["theater"]["derived_constraints"]["rule"]["enforced_label"] =
        serde_json::Value::Null;
    put_manifest(&target, &inconsistent);
    assert!(matches!(load(&target), Err(Error::Storage(_))));

    let mut legacy = original;
    legacy.as_object_mut().unwrap().remove("theater");
    put_manifest(&target, &legacy);
    assert_eq!(load(&target).unwrap().named_constraints().len(), 2);
}

#[test]
fn oversized_checkpoint_theater_keeps_previous_manifest() {
    let target = memory_store();
    let graph = Graph::default();
    checkpoint_at(&target, &graph, 0, 0, DEFAULT_SHARD_TARGET_BYTES).unwrap();
    let previous = get_object(&target, MANIFEST_JSON).unwrap();
    // ADR-0116 §8: a `PutDatabase` redo for a non-fixed name is ignored
    // on apply, so the oversized theater entry comes from a graph type
    // instead.
    graph
        .apply_redo(
            vec![RedoOp::PutGraphType {
                name: "x".repeat(MAX_JSON_OBJECT as usize),
                body: cypherlite_executor::GraphTypeSpec::default(),
            }],
            1,
        )
        .unwrap();
    let err = checkpoint_at(&target, &graph, 1, 1, DEFAULT_SHARD_TARGET_BYTES).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert!(err.to_string().contains("16 MiB"), "{err}");
    assert_eq!(get_object(&target, MANIFEST_JSON).unwrap(), previous);
}
