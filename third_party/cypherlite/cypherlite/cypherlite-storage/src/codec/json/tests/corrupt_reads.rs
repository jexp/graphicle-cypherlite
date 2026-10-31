use super::fixtures::graph_with_typed_and_user_maps;
use super::*;

fn assert_corrupt_snapshots_fail_open(store: &StoreTarget, reopened: &StoreTarget) {
    let graph = graph_with_typed_and_user_maps();
    crate::checkpoint(store, &graph, None).unwrap();
    let original: serde_json::Value =
        serde_json::from_slice(&crate::snapshot::get_object(store, GRAPH_JSON).unwrap()).unwrap();

    for legacy in [false, true] {
        let mut baseline = original.clone();
        if legacy {
            baseline.as_object_mut().unwrap().remove("value_encoding");
            // In version 1 the previously unknown `map` tag is ordinary data.
            baseline["nodes"][1]["properties"]["payload"] =
                serde_json::json!({"__type":"map","value":{"x":1}});
            let mut legacy_rel =
                baseline["relationships"][0]["properties"]["payload"]["value"].clone();
            legacy_rel["nested"][0] = serde_json::json!({"__type":"custom","v":1});
            baseline["relationships"][0]["properties"]["payload"] = legacy_rel;
            let bytes = serde_json::to_vec(&baseline).unwrap();
            put_object(store, GRAPH_JSON, &bytes).unwrap();
            let (loaded, _wal) = crate::open_store(reopened).unwrap();
            let Value::Map(map) = &loaded.node(2).unwrap().properties["payload"] else {
                panic!("legacy map tag must remain ordinary data")
            };
            assert_eq!(map["__type"], Value::String("map".into()));
            assert_eq!(
                map["value"],
                Value::Map(BTreeMap::from([("x".into(), Value::Int(1))]).into())
            );
            assert_eq!(
                loaded.node(1).unwrap().properties["point"],
                graph.node(1).unwrap().properties["point"]
            );
            let loaded_rel = loaded.rel(3).unwrap();
            let original_rel = graph.rel(3).unwrap();
            let Value::Map(loaded_map) = &loaded_rel.properties["payload"] else {
                panic!("legacy relationship payload must be a map")
            };
            let Value::Map(original_map) = &original_rel.properties["payload"] else {
                panic!("original relationship payload must be a map")
            };
            assert_eq!(loaded_map["duration"], original_map["duration"]);
        }

        for (node_property, bad_value, expected) in [
            (
                true,
                serde_json::json!({"__type":"point","srid":4326,"x":1.5}),
                "node 2: invalid graph property $.payload[0].y",
            ),
            (
                false,
                serde_json::json!({"__type":"duration","months":1,"days":2,"nanoseconds":3}),
                "relationship 3: invalid graph property $.payload.nested[0].seconds",
            ),
            (
                false,
                serde_json::json!({"__type":"geometry","srid":4326,"geometry_type":"GeometryCollection","geometries":[{"__type":"geometry","srid":4326,"geometry_type":"LineString","coordinates":[[1]]}]}),
                "relationship 3: invalid graph property $.payload.nested[0].geometries[0].coordinates[0]",
            ),
        ] {
            let mut corrupted = baseline.clone();
            if node_property {
                corrupted["nodes"][1]["properties"]["payload"] =
                    serde_json::json!([bad_value]);
            } else {
                let payload = &mut corrupted["relationships"][0]["properties"]["payload"];
                let path = if legacy {
                    &mut payload["nested"][0]
                } else {
                    &mut payload["value"]["nested"][0]
                };
                *path = bad_value;
            }
            let corrupt_bytes = serde_json::to_vec(&corrupted).unwrap();
            put_object(store, GRAPH_JSON, &corrupt_bytes).unwrap();
            for _ in 0..2 {
                let err = crate::open_store(reopened).err().expect("corrupt snapshot must fail open");
                assert!(matches!(err, Error::Storage(_)), "{err}");
                assert!(err.to_string().contains(expected), "{err}");
            }
            assert_eq!(crate::snapshot::get_object(store, GRAPH_JSON).unwrap(), corrupt_bytes);
        }
    }
}

#[test]
fn json_snapshot_rejects_corrupt_values_native_vfs() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store = crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Json)).unwrap();
    let reopened = store.clone();
    assert_corrupt_snapshots_fail_open(&store, &reopened);
}

#[test]
fn json_snapshot_rejects_corrupt_values_memory_vfs() {
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    assert_corrupt_snapshots_fail_open(&store, &store.clone());
}

fn assert_geometry_snapshots_fail_open(store: &StoreTarget, reopened: &StoreTarget) {
    let graph = graph_with_typed_and_user_maps();
    crate::checkpoint(store, &graph, None).unwrap();
    let original: serde_json::Value =
        serde_json::from_slice(&crate::snapshot::get_object(store, GRAPH_JSON).unwrap()).unwrap();
    let manifest = crate::snapshot::get_object(store, "manifest.json").unwrap();
    let geometry = serde_json::json!({
        "__type": "geometry", "srid": 4326,
        "geometry_type": "LineString", "coordinates": [[1.5, -2.25], [3.0, 4.0]]
    });

    for legacy in [true, false] {
        let mut baseline = original.clone();
        if legacy {
            baseline.as_object_mut().unwrap().remove("value_encoding");
            // The v2 escaped point-looking map is not a valid v1 map.
            baseline["nodes"][1]["properties"]["payload"] = serde_json::json!({"safe": [1]});
            baseline["relationships"][0]["properties"]["payload"] =
                serde_json::json!({"__type": "custom", "items": [1]});
        }
        let baseline_bytes = serde_json::to_vec(&baseline).unwrap();
        put_object(store, GRAPH_JSON, &baseline_bytes).unwrap();
        let (loaded, _wal) = crate::open_store(reopened).unwrap();
        assert_eq!(
            loaded.node(1).unwrap().properties,
            graph.node(1).unwrap().properties,
            "Point and Vector must remain readable in both versions"
        );
        if legacy {
            assert_eq!(
                loaded.node(2).unwrap().properties["payload"],
                Value::Map(
                    BTreeMap::from([("safe".into(), Value::List(vec![Value::Int(1)].into()))])
                        .into()
                )
            );
            assert_eq!(
                loaded.rel(3).unwrap().properties["payload"],
                Value::Map(
                    BTreeMap::from([
                        ("__type".into(), Value::String("custom".into())),
                        ("items".into(), Value::List(vec![Value::Int(1)].into())),
                    ])
                    .into()
                )
            );
        } else {
            assert_eq!(
                loaded.node(2).unwrap().properties,
                graph.node(2).unwrap().properties
            );
            assert_eq!(
                loaded.rel(3).unwrap().properties,
                graph.rel(3).unwrap().properties
            );
        }

        for (location, payload, expected) in [
            (
                "node",
                geometry.clone(),
                "node 2: invalid graph property $.payload: unsupported Geometry",
            ),
            (
                "node",
                serde_json::json!({"items": [{"inner": geometry}]}),
                "node 2: invalid graph property $.payload.items[0].inner: unsupported Geometry",
            ),
            (
                "relationship",
                if legacy {
                    serde_json::json!({"__type": "custom", "items": [geometry]})
                } else {
                    serde_json::json!({
                        "__type": "map", "value": {
                            "__type": "point", "items": [geometry]
                        }
                    })
                },
                "relationship 3: invalid graph property $.payload.items[0]: unsupported Geometry",
            ),
        ] {
            let mut with_geometry = baseline.clone();
            if location == "node" {
                with_geometry["nodes"][1]["properties"]["payload"] = payload;
            } else {
                with_geometry["relationships"][0]["properties"]["payload"] = payload;
            }
            let bytes = serde_json::to_vec(&with_geometry).unwrap();
            put_object(store, GRAPH_JSON, &bytes).unwrap();
            for _ in 0..2 {
                let err = crate::open_store(reopened)
                    .err()
                    .expect("valid Geometry must fail the whole graph load");
                assert!(matches!(err, Error::Storage(_)), "{err}");
                assert!(err.to_string().contains(expected), "{err}");
            }
            assert_eq!(
                crate::snapshot::get_object(store, GRAPH_JSON).unwrap(),
                bytes
            );
            assert_eq!(
                crate::snapshot::get_object(store, "manifest.json").unwrap(),
                manifest
            );
        }
    }
}

#[test]
fn json_snapshot_rejects_geometry_v1_v2_native_vfs() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store = crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Json)).unwrap();
    let reopened = store.clone();
    assert_geometry_snapshots_fail_open(&store, &reopened);
}

#[test]
fn json_snapshot_rejects_geometry_v1_v2_memory_vfs() {
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    assert_geometry_snapshots_fail_open(&store, &store.clone());
}
