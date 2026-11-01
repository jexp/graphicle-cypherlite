use super::fixtures::{
    graph_with_invalid_temporal, graph_with_lossless_temporals, graph_with_typed_and_user_maps,
};
use super::*;
use cypherlite_model::{serde_value::try_json_to_cypher_value_v2, CypherValue};
use std::sync::Arc;

fn assert_invalid_write_does_not_publish(store: &StoreTarget, reopened: &StoreTarget) {
    let empty = Graph::default();
    empty
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec![]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                "bad".into(),
                Value::Map(
                    BTreeMap::from([(
                        "nested".into(),
                        Value::List(vec![Value::Float(f64::NAN)].into()),
                    )])
                    .into(),
                ),
            )])),
        })
        .unwrap();
    empty.set_commit_ts(1);
    let error = crate::checkpoint(store, &empty, None).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(
        error
            .to_string()
            .contains("node 1: invalid graph property $.bad.nested[0]"),
        "{error}"
    );
    assert!(!object_exists(store, GRAPH_JSON).unwrap());
    assert!(!object_exists(store, "manifest.json").unwrap());

    let baseline = graph_with_typed_and_user_maps();
    crate::checkpoint(store, &baseline, None).unwrap();
    let graph_before = crate::snapshot::get_object(store, GRAPH_JSON).unwrap();
    let manifest_before = crate::snapshot::get_object(store, "manifest.json").unwrap();

    let invalid = Graph::default();
    invalid
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec!["N".into()]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                "ok".into(),
                Value::Int(1),
            )])),
        })
        .unwrap();
    invalid
        .load_checkpoint_node(Node {
            id: 2,
            labels: Arc::new(vec!["N".into()]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                "point".into(),
                Value::Point(PointValue {
                    srid: 4326,
                    x: f64::NAN,
                    y: 2.0,
                    z: None,
                }),
            )])),
        })
        .unwrap();
    invalid.set_commit_ts(1);
    let error = crate::checkpoint(store, &invalid, None).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(
        error
            .to_string()
            .contains("node 2: invalid graph property $.point.x"),
        "{error}"
    );

    // A nested non-finite scalar in a relationship also fails the entire write.
    let nested = Graph::default();
    nested
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec![]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        })
        .unwrap();
    nested
        .load_checkpoint_node(Node {
            id: 2,
            labels: Arc::new(vec![]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        })
        .unwrap();
    nested
        .load_checkpoint_rel(Relationship {
            id: 3,
            start: 1,
            end: 2,
            rel_type: "R".into(),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                "payload".into(),
                Value::List(vec![Value::Float(f64::INFINITY)].into()),
            )])),
        })
        .unwrap();
    nested.set_commit_ts(1);
    let error = crate::checkpoint(store, &nested, None).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("relationship 3: invalid graph property $.payload[0]"),
        "{error}"
    );

    let invalid_vector = Graph::default();
    invalid_vector
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec![]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        })
        .unwrap();
    invalid_vector
        .load_checkpoint_node(Node {
            id: 2,
            labels: Arc::new(vec![]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        })
        .unwrap();
    invalid_vector
        .load_checkpoint_rel(Relationship {
            id: 3,
            start: 1,
            end: 2,
            rel_type: "R".into(),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                "payload".into(),
                Value::Map(
                    BTreeMap::from([(
                        "nested".into(),
                        Value::List(
                            vec![Value::Vector {
                                coord: VectorCoordKind::Float32,
                                values: vec![Value::Float(1.5), Value::Float(0.1)].into(),
                            }]
                            .into(),
                        ),
                    )])
                    .into(),
                ),
            )])),
        })
        .unwrap();
    invalid_vector.set_commit_ts(1);
    let error = crate::checkpoint(store, &invalid_vector, None).unwrap_err();
    assert!(matches!(error, Error::Storage(_)), "{error}");
    assert!(
        error
            .to_string()
            .contains("relationship 3: invalid graph property $.payload.nested[0].values[1]"),
        "{error}"
    );

    assert_eq!(
        crate::snapshot::get_object(store, GRAPH_JSON).unwrap(),
        graph_before
    );
    assert_eq!(
        crate::snapshot::get_object(store, "manifest.json").unwrap(),
        manifest_before
    );
    let (loaded, _wal) = crate::open_store(reopened).unwrap();
    assert_eq!(
        loaded.node(1).unwrap().properties,
        baseline.node(1).unwrap().properties
    );
    assert_eq!(
        loaded.rel(3).unwrap().properties,
        baseline.rel(3).unwrap().properties
    );
}

#[test]
fn json_snapshot_invalid_writes_preserve_native_vfs_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store = crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Json)).unwrap();
    let reopened = store.clone();
    assert_invalid_write_does_not_publish(&store, &reopened);
}

#[test]
fn json_snapshot_invalid_writes_preserve_memory_vfs_checkpoint() {
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    assert_invalid_write_does_not_publish(&store, &store.clone());
}

fn assert_temporal_write_guard(store: &StoreTarget, reopened: &StoreTarget) {
    let Temporal::Time { time, .. } = parse_iso("time", "12:34:56+01:00").unwrap() else {
        panic!("expected time")
    };
    let Temporal::DateTime { utc, zone, .. } =
        parse_iso("datetime", "2024-02-29T12:34:56+01:00").unwrap()
    else {
        panic!("expected datetime")
    };
    let cases = [
        (
            "CivilDate direct",
            Value::Temporal(Temporal::CivilDate {
                year: i64::MAX,
                month: 1,
                day: 1,
            }),
            false,
            false,
            "unsupported CivilDate",
        ),
        (
            "CivilDate nested",
            Value::Temporal(Temporal::CivilDate {
                year: 2024,
                month: 2,
                day: 29,
            }),
            true,
            true,
            "unsupported CivilDate",
        ),
        (
            "CivilDateTime direct",
            Value::Temporal(Temporal::CivilDateTime {
                year: i64::MIN,
                month: 1,
                day: 1,
                time,
            }),
            false,
            false,
            "unsupported CivilDateTime",
        ),
        (
            "CivilDateTime nested",
            Value::Temporal(Temporal::CivilDateTime {
                year: 2024,
                month: 2,
                day: 29,
                time,
            }),
            true,
            true,
            "unsupported CivilDateTime",
        ),
        (
            "Duration positive overflow",
            Value::Temporal(Temporal::Duration {
                months: 0,
                days: 0,
                nanos: (i64::MAX as i128 + 1) * 1_000_000_000,
            }),
            false,
            false,
            "Duration seconds out of range",
        ),
        (
            "Duration negative overflow nested",
            Value::Temporal(Temporal::Duration {
                months: 0,
                days: 0,
                nanos: (i64::MIN as i128 - 1) * 1_000_000_000,
            }),
            true,
            true,
            "Duration seconds out of range",
        ),
        (
            "Time +1 second",
            Value::Temporal(Temporal::Time { time, offset: 1 }),
            false,
            false,
            "invalid temporal offset",
        ),
        (
            "Time -1 second nested",
            Value::Temporal(Temporal::Time { time, offset: -1 }),
            true,
            true,
            "invalid temporal offset",
        ),
        (
            "DateTime +1 second",
            Value::Temporal(Temporal::DateTime {
                utc,
                offset: 1,
                zone: zone.clone(),
            }),
            false,
            false,
            "invalid temporal offset",
        ),
        (
            "DateTime -1 second nested",
            Value::Temporal(Temporal::DateTime {
                utc,
                offset: -1,
                zone: zone.clone(),
            }),
            true,
            true,
            "invalid temporal offset",
        ),
        (
            "Time unconstructible offset",
            Value::Temporal(Temporal::Time {
                time,
                offset: 86_400,
            }),
            false,
            false,
            "invalid temporal offset",
        ),
        (
            "DateTime unconstructible offset nested",
            Value::Temporal(Temporal::DateTime {
                utc,
                offset: -86_400,
                zone,
            }),
            true,
            true,
            "invalid temporal offset",
        ),
    ];

    let baseline = graph_with_lossless_temporals();
    for existing in [false, true] {
        let previous = if existing {
            crate::checkpoint(store, &baseline, None).unwrap();
            Some((
                crate::snapshot::get_object(store, GRAPH_JSON).unwrap(),
                crate::snapshot::get_object(store, "manifest.json").unwrap(),
            ))
        } else {
            None
        };
        for (name, value, relationship, nested, reason) in &cases {
            let invalid = graph_with_invalid_temporal(value.clone(), *relationship, *nested);
            let error = crate::checkpoint(store, &invalid, None).expect_err(name);
            let entity = if *relationship {
                "relationship 3"
            } else {
                "node 1"
            };
            let path = if *nested {
                "$.payload.items[0].bad"
            } else {
                "$.bad"
            };
            assert!(matches!(error, Error::Storage(_)), "{name}: {error}");
            assert!(
                error.to_string().contains(&format!(
                    "{entity}: invalid graph property {path}: {reason}"
                )),
                "{name}: {error}"
            );
            if let Some((graph_before, manifest_before)) = &previous {
                assert_eq!(
                    crate::snapshot::get_object(store, GRAPH_JSON).unwrap(),
                    *graph_before,
                    "{name}"
                );
                assert_eq!(
                    crate::snapshot::get_object(store, "manifest.json").unwrap(),
                    *manifest_before,
                    "{name}"
                );
                let (loaded, _wal) = crate::open_store(reopened).unwrap();
                for id in [1, 2] {
                    assert_eq!(
                        loaded.node(id).unwrap().properties,
                        baseline.node(id).unwrap().properties,
                        "{name}"
                    );
                }
                assert_eq!(
                    loaded.rel(3).unwrap().properties,
                    baseline.rel(3).unwrap().properties,
                    "{name}"
                );
            } else {
                assert!(!object_exists(store, GRAPH_JSON).unwrap(), "{name}");
                assert!(!object_exists(store, "manifest.json").unwrap(), "{name}");
            }
        }
    }
}

#[test]
fn json_snapshot_rejects_lossy_temporals_native_vfs() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store = crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Json)).unwrap();
    let reopened = store.clone();
    assert_temporal_write_guard(&store, &reopened);
}

#[test]
fn json_snapshot_rejects_lossy_temporals_memory_vfs() {
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    assert_temporal_write_guard(&store, &store.clone());
}

fn fixed_offset_datetime(instant: &str, offset: i32) -> Value {
    let CypherValue::LocalDateTime(utc) = try_json_to_cypher_value_v2(&serde_json::json!({
        "__type": "localdatetime", "value": instant,
    }))
    .unwrap() else {
        panic!("expected local datetime");
    };
    Value::Temporal(Temporal::DateTime {
        utc,
        offset,
        zone: Some("Fixed/Offset".into()),
    })
}

fn assert_local_datetime_range_checkpoint(store: &StoreTarget, reopened: &StoreTarget) {
    // These are chrono's NaiveDateTime::MAX/MIN and instants one hour inside
    // either limit, represented as the model's extended-year persistence dates.
    let cases = [
        ("+262142-12-31T23:59:59.999999999", 3600, false, false),
        ("-262143-01-01T00:00:00", -3600, true, true),
        ("+262142-12-31T23:59:59.999999999", 3600, true, true),
        ("-262143-01-01T00:00:00", -3600, false, false),
    ];
    let baseline = Graph::default();
    baseline
        .load_checkpoint_node(Node {
            id: 1,
            labels: Arc::new(vec![]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                "upper".into(),
                fixed_offset_datetime("+262142-12-31T22:59:59.999999999", 3600),
            )])),
        })
        .unwrap();
    baseline
        .load_checkpoint_node(Node {
            id: 2,
            labels: Arc::new(vec![]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::new()),
        })
        .unwrap();
    baseline
        .load_checkpoint_rel(Relationship {
            id: 3,
            start: 1,
            end: 2,
            rel_type: "R".into(),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                "lower".into(),
                fixed_offset_datetime("-262143-01-01T01:00:00", -3600),
            )])),
        })
        .unwrap();
    baseline.set_commit_ts(1);

    for existing in [false, true] {
        let previous = if existing {
            crate::checkpoint(store, &baseline, None).unwrap();
            Some((
                crate::snapshot::get_object(store, GRAPH_JSON).unwrap(),
                crate::snapshot::get_object(store, "manifest.json").unwrap(),
            ))
        } else {
            None
        };
        for (utc, offset, relationship, nested) in cases {
            let graph = graph_with_invalid_temporal(
                fixed_offset_datetime(utc, offset),
                relationship,
                nested,
            );
            let error = crate::checkpoint(store, &graph, None).unwrap_err();
            let (entity, path) = if nested {
                ("relationship 3", "$.payload.items[0].bad")
            } else {
                ("node 1", "$.bad")
            };
            assert!(matches!(error, Error::Storage(_)), "{error}");
            assert!(
                error.to_string().contains(&format!(
                    "{entity}: invalid graph property {path}: DateTime local time out of range"
                )),
                "{error}"
            );
            if let Some((graph_before, manifest_before)) = &previous {
                assert_eq!(
                    crate::snapshot::get_object(store, GRAPH_JSON).unwrap(),
                    *graph_before
                );
                assert_eq!(
                    crate::snapshot::get_object(store, "manifest.json").unwrap(),
                    *manifest_before
                );
                let (loaded, _wal) = crate::open_store(reopened).unwrap();
                assert_eq!(
                    loaded.node(1).unwrap().properties,
                    baseline.node(1).unwrap().properties
                );
                assert_eq!(
                    loaded.rel(3).unwrap().properties,
                    baseline.rel(3).unwrap().properties
                );
            } else {
                assert!(!object_exists(store, GRAPH_JSON).unwrap());
                assert!(!object_exists(store, "manifest.json").unwrap());
            }
        }
    }
}

#[test]
fn json_snapshot_local_datetime_range_native_vfs() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store = crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Json)).unwrap();
    let reopened = store.clone();
    assert_local_datetime_range_checkpoint(&store, &reopened);
}

#[test]
fn json_snapshot_local_datetime_range_memory_vfs() {
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    assert_local_datetime_range_checkpoint(&store, &store.clone());
}
