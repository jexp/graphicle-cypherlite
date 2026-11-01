use std::time::Duration;

use cypherlite::{Database, OpenOptions};
use cypherlite_core::Error;
use cypherlite_storage::SnapshotCodecKind;

#[test]
fn database_open_rejects_corrupt_json_snapshot_without_rewriting_it() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    {
        let db = Database::open_with(
            uri,
            OpenOptions {
                checkpoint_interval: Some(Duration::ZERO),
                snapshot_codec: Some(SnapshotCodecKind::Json),
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let mut conn = db.connect().unwrap();
        conn.query("CREATE (:T {name: 'good'})").unwrap();
        conn.query("CREATE (:T {point: point({x: 1.5, y: -2.25})})")
            .unwrap();
        db.checkpoint().unwrap();
    }

    let path = dir.path().join("graph.json");
    let mut data: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(data["value_encoding"], 2);
    assert_eq!(data["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(data["nodes"][1]["properties"]["point"]["__type"], "point");
    data["nodes"][1]["properties"]["point"] =
        serde_json::json!({"__type":"point","srid":7203,"x":1.5});
    let corrupt_bytes = serde_json::to_vec(&data).unwrap();
    std::fs::write(&path, &corrupt_bytes).unwrap();

    for _ in 0..2 {
        let err = Database::open(uri)
            .err()
            .expect("a corrupt second node must fail the whole database open");
        assert!(matches!(err, Error::Storage(_)), "{err}");
        assert!(
            err.to_string().contains("invalid graph property $.point.y"),
            "{err}"
        );
    }
    assert_eq!(std::fs::read(path).unwrap(), corrupt_bytes);
}

#[test]
fn database_open_rejects_valid_geometry_in_v1_v2_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    {
        let db = Database::open_with(
            uri,
            OpenOptions {
                checkpoint_interval: Some(Duration::ZERO),
                snapshot_codec: Some(SnapshotCodecKind::Json),
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let mut conn = db.connect().unwrap();
        conn.query("CREATE (:T {name: 'good'})").unwrap();
        conn.query("CREATE (:T {point: point({x: 1.5, y: -2.25})})")
            .unwrap();
        db.checkpoint().unwrap();
    }

    let path = dir.path().join("graph.json");
    let baseline: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let node_id = baseline["nodes"][1]["id"].as_i64().unwrap();
    let manifest_path = dir.path().join("manifest.json");
    let manifest = std::fs::read(&manifest_path).unwrap();
    let geometry = serde_json::json!({
        "__type": "geometry", "srid": 4326,
        "geometry_type": "LineString", "coordinates": [[1.5, -2.25], [3.0, 4.0]]
    });

    for legacy in [true, false] {
        for nested in [false, true] {
            let mut data = baseline.clone();
            if legacy {
                data.as_object_mut().unwrap().remove("value_encoding");
            }
            data["nodes"][1]["properties"]["payload"] = if nested {
                serde_json::json!({"items": [geometry]})
            } else {
                geometry.clone()
            };
            let bytes = serde_json::to_vec(&data).unwrap();
            std::fs::write(&path, &bytes).unwrap();

            for _ in 0..2 {
                let err = Database::open(uri)
                    .err()
                    .expect("unsupported Geometry must prevent database open");
                assert!(matches!(err, Error::Storage(_)), "{err}");
                let property = if nested {
                    "$.payload.items[0]"
                } else {
                    "$.payload"
                };
                assert!(
                    err.to_string().contains(&format!(
                        "node {node_id}: invalid graph property {property}: unsupported Geometry"
                    )),
                    "{err}"
                );
            }
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(std::fs::read(&manifest_path).unwrap(), manifest);
        }
    }
}
