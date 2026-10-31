use super::*;
use crate::redo::wal_format_json::value_json;
use std::sync::Arc;

fn sample_ops() -> Vec<RedoOp> {
    vec![
        RedoOp::CatalogIndexDetail {
            name: "vec_idx".into(),
            detail: IndexDetailSpec {
                kind: IndexKindSpec::Vector,
                relationship: false,
                labels: vec!["Doc".into()],
                properties: vec!["embedding".into()],
                with_properties: vec!["ttl".into()],
            },
            options: props_json(&BTreeMap::from([(
                "indexConfig".to_string(),
                Value::Map(
                    BTreeMap::from([("vector.dimensions".to_string(), Value::Int(1536))]).into(),
                ),
            )])),
        },
        RedoOp::PutGraphType {
            name: "gt".into(),
            body: GraphTypeSpec {
                node_types: vec![],
                edge_types: vec![],
                constraints: vec![],
                drop_constraint_names: vec![],
            },
        },
        RedoOp::SetCurrentGraphType {
            body: GraphTypeSpec::default(),
        },
        RedoOp::PullIdentityPut {
            source: "http://example/db/neo4j/query/v2".into(),
            kind: PullKind::Node,
            remote_id: "4:aaa:0".into(),
            local_id: 1,
        },
        RedoOp::OntologySet {
            database: "neo4j".into(),
            spec: "{\"classes\":[]}".into(),
        },
        RedoOp::OntologyDrop {
            database: "neo4j".into(),
        },
    ]
}

/// `is_catalog`, derived from `write_target`, keeps the ADR-0172 §8 split:
/// the ten entity ops, `PullIdentityPut` and the store-level ontology ops
/// (ADR-0120 §3) are not catalog ops, every other op is.
#[test]
fn is_catalog_splits_entity_ops_from_catalog_ops() {
    let entity = [
        RedoOp::create_node(1, vec!["L".into()], BTreeMap::new()),
        RedoOp::create_rel(2, 1, 1, "R".into(), BTreeMap::new()),
        RedoOp::delete_node(1),
        RedoOp::delete_rel(2),
        RedoOp::set_node_prop(1, "k".into(), Value::Int(1)),
        RedoOp::remove_node_prop(1, "k".into()),
        RedoOp::set_rel_prop(2, "k".into(), Value::Int(1)),
        RedoOp::remove_rel_prop(2, "k".into()),
        RedoOp::add_labels(1, vec!["L".into()]),
        RedoOp::remove_labels(1, vec!["L".into()]),
    ];
    // PullIdentityPut, OntologySet, OntologyDrop.
    let non_catalog: Vec<RedoOp> = entity
        .into_iter()
        .chain(sample_ops().into_iter().skip(3))
        .collect();
    let mut catalog = vec![
        RedoOp::CreateIndex {
            name: "i".into(),
            label: "L".into(),
            property: "k".into(),
        },
        RedoOp::DropIndex { name: "i".into() },
        RedoOp::CreateConstraint {
            name: "c".into(),
            relationship: false,
            label: "L".into(),
            properties: vec!["k".into()],
            kind: "unique".into(),
            property_type: None,
        },
        RedoOp::DropConstraint { name: "c".into() },
        RedoOp::CatalogDerivedConstraint {
            constraint: cypherlite_model::Constraint {
                name: "c".into(),
                label: "L".into(),
                properties: vec!["k".into()],
                constraint_type: cypherlite_model::ConstraintType::Unique,
                property_type: None,
                entity_type: "NODE".into(),
                enforced_label: None,
            },
        },
        RedoOp::PutDatabase {
            name: "neo4j".into(),
            language: 5,
            composite: false,
        },
        RedoOp::DropDatabase {
            name: "analytics".into(),
        },
    ];
    // CatalogIndexDetail, PutGraphType, SetCurrentGraphType.
    catalog.extend(sample_ops().into_iter().take(3));
    for op in &non_catalog {
        assert!(!op.is_catalog(), "{op:?}");
    }
    for op in &catalog {
        assert!(op.is_catalog(), "{op:?}");
    }
    assert_eq!(
        non_catalog.len() + catalog.len(),
        23,
        "every RedoOp variant"
    );
}

#[test]
fn catalog_ddl_ops_serde_roundtrip() {
    let ops = sample_ops();
    let json = serde_json::to_vec(&ops).expect("ops serialize");
    let decoded: Vec<RedoOp> = serde_json::from_slice(&json).expect("ops deserialize");
    assert_eq!(decoded, ops);
}

/// `catalog_names` derives every name an op mentions: the data writes, the
/// DDL ops that name a label/type/key without writing data, and the monotone
/// rule — a remove still mentions the name it removes. Store metadata (an
/// index, constraint or database name) is not a graph catalog name.
#[test]
fn catalog_names_cover_data_and_ddl_names_monotonically() {
    fn mentions(op: &RedoOp, wanted: CatalogName<'_>) -> bool {
        op.catalog_names().contains(&wanted)
    }

    let node = RedoOp::create_node(
        1,
        vec!["L".into()],
        BTreeMap::from([("k".to_string(), Value::Int(1))]),
    );
    assert!(mentions(&node, CatalogName::Label("L")));
    assert!(mentions(&node, CatalogName::PropertyKey("k")));

    let rel = RedoOp::create_rel(2, 1, 1, "T".into(), BTreeMap::new());
    assert!(mentions(&rel, CatalogName::RelType("T")));

    // Monotone: the remove ops mention the very name they take away.
    assert!(mentions(
        &RedoOp::remove_labels(1, vec!["L".into()]),
        CatalogName::Label("L")
    ));
    assert!(mentions(
        &RedoOp::remove_node_prop(1, "k".into()),
        CatalogName::PropertyKey("k")
    ));

    // DDL names: the index's label and key (bare or legacy-encoded), the
    // constraint's subject and properties, and the typed detail's
    // properties plus `WITH` keys.
    let index = RedoOp::CreateIndex {
        name: "i".into(),
        label: "L".into(),
        property: legacy_property_encoding("k", IndexKindSpec::Range, "i"),
    };
    assert!(mentions(&index, CatalogName::Label("L")));
    assert!(mentions(&index, CatalogName::PropertyKey("k")));

    let constraint = RedoOp::CreateConstraint {
        name: "c".into(),
        relationship: true,
        label: "T".into(),
        properties: vec!["k".into()],
        kind: "unique".into(),
        property_type: None,
    };
    assert!(mentions(&constraint, CatalogName::RelType("T")));
    assert!(mentions(&constraint, CatalogName::PropertyKey("k")));

    let detail = RedoOp::CatalogIndexDetail {
        name: "vi".into(),
        detail: IndexDetailSpec {
            kind: IndexKindSpec::Vector,
            relationship: false,
            labels: vec!["L".into()],
            properties: vec!["embedding".into()],
            with_properties: vec!["ttl".into()],
        },
        options: props_json(&BTreeMap::new()),
    };
    assert!(mentions(&detail, CatalogName::Label("L")));
    assert!(mentions(&detail, CatalogName::PropertyKey("embedding")));
    assert!(mentions(&detail, CatalogName::PropertyKey("ttl")));

    let derived = RedoOp::CatalogDerivedConstraint {
        constraint: cypherlite_model::Constraint {
            name: "c".into(),
            label: "L".into(),
            properties: vec!["k".into()],
            constraint_type: cypherlite_model::ConstraintType::Exists,
            property_type: None,
            entity_type: "NODE".into(),
            enforced_label: Some("Owner".into()),
        },
    };
    assert!(mentions(&derived, CatalogName::Label("L")));
    assert!(mentions(&derived, CatalogName::Label("Owner")));
    assert!(mentions(&derived, CatalogName::PropertyKey("k")));

    // A drop and the store-metadata ops mention no catalog name.
    for op in [
        RedoOp::DropIndex { name: "i".into() },
        RedoOp::DropConstraint { name: "c".into() },
        RedoOp::delete_node(1),
        RedoOp::PutDatabase {
            name: "neo4j".into(),
            language: 5,
            composite: false,
        },
        RedoOp::OntologyDrop {
            database: "neo4j".into(),
        },
    ] {
        assert!(op.catalog_names().is_empty(), "{op:?}");
    }
}

/// ADR-0120 §3: the document is limited to 16 MiB, and the limit is
/// checked by the one pre-persist validation entry point
/// ([`RedoOp::validate_ids`]) before the commit is written. The boundary
/// is exact.
#[test]
fn ontology_document_size_gate_is_the_16_mib_envelope() {
    let set = |len: usize| RedoOp::OntologySet {
        database: "neo4j".into(),
        spec: "x".repeat(len),
    };
    assert!(set(MAX_ONTOLOGY_SPEC_BYTES).validate_ids().is_ok());
    let err = set(MAX_ONTOLOGY_SPEC_BYTES + 1)
        .validate_ids()
        .expect_err("one byte over the envelope fails");
    assert!(matches!(err, cypherlite_core::Error::Storage(_)), "{err}");
    assert!(err.to_string().contains("16 MiB"), "{err}");
    // OntologyDrop carries no document; non-ontology ops pass too.
    assert!(RedoOp::OntologyDrop {
        database: "neo4j".into()
    }
    .validate_ids()
    .is_ok());
    assert!(RedoOp::delete_node(1).validate_ids().is_ok());
}

/// The typed ontology classifier: `Set` borrows the verbatim document,
/// `Drop` carries nothing, every other op classifies as `None` — and
/// `is_ontology_op` derives from it.
#[test]
fn ontology_op_classifies_the_two_store_ops() {
    let set = RedoOp::OntologySet {
        database: "neo4j".into(),
        spec: "doc".into(),
    };
    assert_eq!(set.ontology_op(), Some(OntologyOp::Set("doc")));
    assert!(set.is_ontology_op());
    let drop = RedoOp::OntologyDrop {
        database: "neo4j".into(),
    };
    assert_eq!(drop.ontology_op(), Some(OntologyOp::Drop));
    assert!(drop.is_ontology_op());
    let node = RedoOp::delete_node(1);
    assert_eq!(node.ontology_op(), None);
    assert!(!node.is_ontology_op());
}

/// A WAL/snapshot written while `DROP DATABASE` was still executable
/// may carry `{"op":"DropDatabase",...}`; the variant is kept
/// decode-only so those stores keep opening (ADR-0114).
#[test]
fn legacy_drop_database_op_still_decodes() {
    let raw = br#"[{"op":"DropDatabase","name":"analytics"}]"#;
    let decoded: Vec<RedoOp> = serde_json::from_slice(raw).expect("legacy op decodes");
    assert_eq!(
        decoded,
        vec![RedoOp::DropDatabase {
            name: "analytics".into()
        }]
    );
}

#[test]
fn legacy_string_payload_no_longer_decodes() {
    // The pre-typed shape carried a `specification` Cypher string that
    // replay re-parsed. A record in that shape is now rejected at
    // decode time instead of being silently re-parsed-or-dropped.
    let raw = br#"[{"op":"PutGraphType","name":"gt","specification":"(:`A`)"}]"#;
    assert!(serde_json::from_slice::<Vec<RedoOp>>(raw).is_err());
    let raw = br#"[{"op":"SetCurrentGraphType","specification":"(:`A`)"}]"#;
    assert!(serde_json::from_slice::<Vec<RedoOp>>(raw).is_err());
}

/// Mirror of the pre-`bd3836d` on-disk shape. Before that commit the
/// property-carrying `RedoOp` fields were plain tagged-JSON
/// `serde_json::Value`s (produced by [`props_json`] / [`value_json`]);
/// these mirrors reproduce that shape -- tag first, then fields in
/// declaration order -- so the golden tests can compare the exact WAL
/// bytes the old representation emitted.
mod legacy_shape {
    use cypherlite_core::{NodeId, RelId};
    use serde::Serialize;

    #[derive(Serialize)]
    pub(super) struct CreateNode {
        pub op: &'static str,
        pub id: NodeId,
        pub labels: Vec<String>,
        pub properties: serde_json::Value,
    }

    #[derive(Serialize)]
    pub(super) struct CreateRel {
        pub op: &'static str,
        pub id: RelId,
        pub start: NodeId,
        pub end: NodeId,
        pub rel_type: String,
        pub properties: serde_json::Value,
    }

    #[derive(Serialize)]
    pub(super) struct SetNodeProp {
        pub op: &'static str,
        pub id: NodeId,
        pub key: String,
        pub value: serde_json::Value,
    }

    #[derive(Serialize)]
    pub(super) struct SetRelProp {
        pub op: &'static str,
        pub id: RelId,
        pub key: String,
        pub value: serde_json::Value,
    }
}

/// Representative tagged values: every scalar encoding plus the tagged
/// ones (NaN/Infinity/-Infinity, temporal, point) and nesting.
fn golden_values() -> Vec<(&'static str, Value)> {
    vec![
        ("string", Value::String("hi".into())),
        ("int", Value::Int(42)),
        ("float", Value::Float(1.5)),
        ("nan", Value::Float(f64::NAN)),
        ("infinity", Value::Float(f64::INFINITY)),
        ("neg_infinity", Value::Float(f64::NEG_INFINITY)),
        ("neg_zero", Value::Float(-0.0)),
        (
            "date",
            Value::Temporal(cypherlite_core::temporal::parse_iso("date", "2020-01-02").unwrap()),
        ),
        (
            "datetime",
            Value::Temporal(
                cypherlite_core::temporal::parse_iso("datetime", "2020-01-02T03:04:05Z").unwrap(),
            ),
        ),
        (
            "duration",
            Value::Temporal(
                cypherlite_core::temporal::parse_iso("duration", "P1Y2M3DT4H5M6S").unwrap(),
            ),
        ),
        (
            "point",
            Value::Point(cypherlite_core::PointValue::cartesian_3d(1.0, 2.0, 3.0)),
        ),
        ("bool", Value::Bool(true)),
        ("null", Value::Null),
        (
            "vector",
            Value::Vector {
                coord: cypherlite_core::VectorCoordKind::Float64,
                values: vec![Value::Int(1), Value::Float(2.5)].into(),
            },
        ),
        (
            "nested_list",
            Value::List(
                vec![
                    Value::Int(1),
                    Value::List(vec![Value::String("x".into()), Value::Null].into()),
                ]
                .into(),
            ),
        ),
        (
            "nested_map",
            Value::Map(
                BTreeMap::from([(
                    "a".to_string(),
                    Value::Map(BTreeMap::from([("b".to_string(), Value::Int(1))]).into()),
                )])
                .into(),
            ),
        ),
    ]
}

fn golden_props() -> BTreeMap<String, Value> {
    golden_values()
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// The typed op must emit exactly the bytes the pre-typed representation
/// (a `serde_json::Value` from `value_json`) emitted.
#[test]
fn set_prop_bytes_match_legacy_value_json() {
    for (name, value) in golden_values() {
        let op = RedoOp::SetNodeProp {
            id: 7,
            key: "k".into(),
            value: value.clone(),
        };
        let legacy = legacy_shape::SetNodeProp {
            op: "SetNodeProp",
            id: 7,
            key: "k".into(),
            value: value_json(&value),
        };
        assert_eq!(
            serde_json::to_vec(&op).unwrap(),
            serde_json::to_vec(&legacy).unwrap(),
            "SetNodeProp byte mismatch for {name}"
        );

        let op = RedoOp::SetRelProp {
            id: 8,
            key: "k".into(),
            value: value.clone(),
        };
        let legacy = legacy_shape::SetRelProp {
            op: "SetRelProp",
            id: 8,
            key: "k".into(),
            value: value_json(&value),
        };
        assert_eq!(
            serde_json::to_vec(&op).unwrap(),
            serde_json::to_vec(&legacy).unwrap(),
            "SetRelProp byte mismatch for {name}"
        );
    }
}

/// The typed property map must emit exactly the bytes `props_json`
/// produced for the same logical map.
#[test]
fn create_ops_bytes_match_legacy_props_json() {
    let props = golden_props();

    let op = RedoOp::CreateNode {
        id: 1,
        labels: Arc::new(vec!["N".into()]),
        properties: cypherlite_core::PropertyMap::share(props.clone()),
    };
    let legacy = legacy_shape::CreateNode {
        op: "CreateNode",
        id: 1,
        labels: vec!["N".into()],
        properties: props_json(&props),
    };
    assert_eq!(
        serde_json::to_vec(&op).unwrap(),
        serde_json::to_vec(&legacy).unwrap(),
        "CreateNode byte mismatch"
    );

    let op = RedoOp::CreateRel {
        id: 2,
        start: 1,
        end: 3,
        rel_type: "R".into(),
        properties: cypherlite_core::PropertyMap::share(props.clone()),
    };
    let legacy = legacy_shape::CreateRel {
        op: "CreateRel",
        id: 2,
        start: 1,
        end: 3,
        rel_type: "R".into(),
        properties: props_json(&props),
    };
    assert_eq!(
        serde_json::to_vec(&op).unwrap(),
        serde_json::to_vec(&legacy).unwrap(),
        "CreateRel byte mismatch"
    );
}

/// Pin the exact tagged bytes so any codec drift (a renamed tag, a
/// different NaN rendering, a changed temporal/point shape) fails loudly.
#[test]
fn tagged_encodings_are_byte_pinned() {
    fn pinned(value: Value) -> String {
        serde_json::to_string(&RedoOp::SetNodeProp {
            id: 7,
            key: "k".into(),
            value,
        })
        .unwrap()
    }

    assert_eq!(
        pinned(Value::Float(f64::NAN)),
        r#"{"op":"SetNodeProp","id":7,"key":"k","value":{"t":"float","v":"NaN"}}"#
    );
    assert_eq!(
        pinned(Value::Float(f64::INFINITY)),
        r#"{"op":"SetNodeProp","id":7,"key":"k","value":{"t":"float","v":"Infinity"}}"#
    );
    assert_eq!(
        pinned(Value::Float(f64::NEG_INFINITY)),
        r#"{"op":"SetNodeProp","id":7,"key":"k","value":{"t":"float","v":"-Infinity"}}"#
    );
    assert_eq!(
        pinned(Value::Float(-0.0)),
        r#"{"op":"SetNodeProp","id":7,"key":"k","value":{"t":"float","v":-0.0}}"#
    );
    assert_eq!(
        pinned(Value::Temporal(
            cypherlite_core::temporal::parse_iso("date", "2020-01-02").unwrap()
        )),
        r#"{"op":"SetNodeProp","id":7,"key":"k","value":{"t":"date","v":"2020-01-02"}}"#
    );
    assert_eq!(
        pinned(Value::Temporal(
            cypherlite_core::temporal::parse_iso("datetime", "2020-01-02T03:04:05Z").unwrap()
        )),
        r#"{"op":"SetNodeProp","id":7,"key":"k","value":{"t":"datetime","v":"2020-01-02T03:04:05Z"}}"#
    );
    assert_eq!(
        pinned(Value::Temporal(
            cypherlite_core::temporal::parse_iso("duration", "P1Y2M3DT4H5M6S").unwrap()
        )),
        r#"{"op":"SetNodeProp","id":7,"key":"k","value":{"t":"duration","v":"P1Y2M3DT4H5M6S"}}"#
    );
    assert_eq!(
        pinned(Value::Point(cypherlite_core::PointValue::cartesian_3d(
            1.0, 2.0, 3.0
        ))),
        r#"{"op":"SetNodeProp","id":7,"key":"k","value":{"t":"point","v":{"srid":9157,"x":1.0,"y":2.0,"z":3.0}}}"#
    );
}

/// Round-trip guard: replaying the pinned bytes yields the original op.
#[test]
fn tagged_encodings_roundtrip_through_codec() {
    for (name, value) in golden_values() {
        let op = RedoOp::SetNodeProp {
            id: 7,
            key: "k".into(),
            value: value.clone(),
        };
        let bytes = serde_json::to_vec(&op).unwrap();
        let decoded: RedoOp = serde_json::from_slice(&bytes).unwrap();
        let RedoOp::SetNodeProp { value: got, .. } = decoded else {
            panic!("wrong variant for {name}");
        };
        if let Value::Float(f) = &value {
            assert_eq!(
                f.is_nan(),
                matches!(got, Value::Float(g) if g.is_nan()),
                "{name}"
            );
            if !f.is_nan() {
                assert_eq!(got, value, "{name}");
            }
        } else {
            assert_eq!(got, value, "{name}");
        }
    }
}

#[test]
fn props_from_json_rejects_non_object_payloads() {
    assert!(props_from_json(&serde_json::json!("corrupted")).is_err());
    assert!(props_from_json(&serde_json::json!([1, 2])).is_err());
    assert!(props_from_json(&serde_json::json!({"k": {"x": 1}})).is_err());
    assert!(props_from_json(&serde_json::json!({})).unwrap().is_empty());
}

#[test]
fn index_kind_display_name_round_trips() {
    for kind in IndexKindSpec::ALL {
        assert_eq!(
            IndexKindSpec::from_display_name(kind.display_name()),
            Some(kind)
        );
    }
    assert_eq!(IndexKindSpec::from_display_name("range"), None);
    assert_eq!(IndexKindSpec::from_display_name("NOT A KIND"), None);
}

#[test]
fn legacy_property_encoding_round_trips() {
    for kind in IndexKindSpec::ALL {
        let encoded = legacy_property_encoding("name", kind, "idx_name");
        assert_eq!(encoded, format!("name#{}#idx_name", kind.display_name()));
        assert_eq!(
            split_legacy_property_encoding(&encoded),
            ("name", Some(kind))
        );
    }
}

#[test]
fn split_legacy_property_encoding_decodes_bare_and_unknown_kind() {
    // A bare property name (no `#`) is its own head with no kind.
    assert_eq!(split_legacy_property_encoding("name"), ("name", None));
    // An unrecognized KIND segment decodes to no kind, head intact.
    assert_eq!(
        split_legacy_property_encoding("name#NOTAKIND#idx"),
        ("name", None)
    );
    // Comma-joined multi-property heads survive the split.
    assert_eq!(
        split_legacy_property_encoding("a,b#RANGE#idx"),
        ("a,b", Some(IndexKindSpec::Range))
    );
}

fn detail(
    relationship: bool,
    labels: &[&str],
    properties: &[&str],
    kind: IndexKindSpec,
) -> IndexDetailSpec {
    IndexDetailSpec {
        kind,
        relationship,
        labels: labels.iter().map(|label| label.to_string()).collect(),
        properties: properties.iter().map(|p| p.to_string()).collect(),
        with_properties: Vec::new(),
    }
}

/// Every component of a composite range index serves an equality seek: the
/// executor materializes and seeks each covered pair, so a trailing property
/// is covered too.
#[test]
fn range_seek_pairs_cover_every_composite_component() {
    let composite = detail(false, &["Person"], &["first", "last"], IndexKindSpec::Range);
    assert_eq!(
        composite.range_seek_pairs(),
        [("Person", "first"), ("Person", "last")]
    );
    assert_eq!(
        composite.covered_pairs().len(),
        2,
        "every component still materializes a posting"
    );
}

/// A relationship index never contributes a node `(label, property)` pair,
/// even though its type name could collide with a node label.
#[test]
fn range_seek_pairs_exclude_relationship_indexes() {
    let relationship = detail(true, &["Person"], &["x"], IndexKindSpec::Range);
    assert!(relationship.range_seek_pairs().is_empty());
}

/// A property name is one name: a comma inside it is never split, and a
/// non-range kind serves no equality seek.
#[test]
fn range_seek_pairs_keep_a_comma_bearing_property_name_whole() {
    let comma = detail(false, &["Order"], &["ship,date"], IndexKindSpec::Range);
    assert_eq!(comma.range_seek_pairs(), [("Order", "ship,date")]);
    let text = detail(false, &["Order"], &["ship,date"], IndexKindSpec::Text);
    assert!(text.range_seek_pairs().is_empty());
}
