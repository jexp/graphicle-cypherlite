//! JSON snapshots recover supported values and reject lossy temporal/Geometry properties.

use std::{collections::BTreeMap, error::Error as StdError, fs, time::Duration};

use cypherlite::{Database, Error, OpenOptions, SnapshotCodecKind, Temporal, Value};

fn main() -> Result<(), Box<dyn StdError>> {
    let dir = tempfile::tempdir()?;
    let uri = dir.path().to_str().expect("temporary path is UTF-8");
    {
        let db = Database::open_with(
            uri,
            OpenOptions {
                snapshot_codec: Some(SnapshotCodecKind::Json),
                checkpoint_interval: Some(Duration::ZERO),
                ..Default::default()
            },
        )?;
        db.connect()?.query(
            "CREATE (:Snapshot {name: 'Ada', p: point({x: 1.5, y: 2.5}), \
             v: vector([1, 2], 2, INTEGER)})",
        )?;
        db.checkpoint()?;
    }

    let file = dir.path().join("graph.json");
    let mut graph: serde_json::Value = serde_json::from_slice(&fs::read(&file)?)?;
    assert_eq!(graph["value_encoding"], 2);
    // Seed a v2 snapshot fixture: Cypher queries do not permit Map properties.
    graph["nodes"][0]["properties"]["meta"] =
        serde_json::json!({"__type":"map", "value":{"__type":"point", "x":1}});
    assert_eq!(graph["nodes"][0]["properties"]["meta"]["__type"], "map");
    assert_eq!(graph["nodes"][0]["properties"]["p"]["__type"], "point");
    assert_eq!(graph["nodes"][0]["properties"]["v"]["__type"], "vector");

    let node_id = graph["nodes"][0]["id"]
        .as_i64()
        .expect("checkpoint node ID");
    let geometry = serde_json::json!({
        "__type": "geometry", "srid": 4326,
        "geometry_type": "LineString", "coordinates": [[1.5, -2.25], [3.0, 4.0]]
    });
    for legacy in [false, true] {
        let mut snapshot = graph.clone();
        if legacy {
            snapshot.as_object_mut().unwrap().remove("value_encoding");
            // The escaped map is a v2-only representation.
            snapshot["nodes"][0]["properties"]
                .as_object_mut()
                .unwrap()
                .remove("meta");
        }
        fs::write(&file, serde_json::to_vec(&snapshot)?)?;
        let db = Database::open(uri)?;
        let result = db
            .connect()?
            .query("MATCH (n:Snapshot) RETURN n.p AS p, n.v AS v, n.meta AS meta")?;
        assert!(matches!(&result.rows[0][0], Value::Point(p) if p.x == 1.5 && p.y == 2.5));
        assert!(
            matches!(&result.rows[0][1], Value::Vector { values, .. } if **values == vec![Value::Int(1), Value::Int(2)])
        );
        if !legacy {
            assert!(matches!(result.rows[0][2], Value::Map(_)));
        }
        drop(db);

        for (payload, path) in [
            (geometry.clone(), "$.payload"),
            (
                serde_json::json!({"items": [geometry]}),
                "$.payload.items[0]",
            ),
        ] {
            let mut invalid = snapshot.clone();
            invalid["nodes"][0]["properties"]["payload"] = payload;
            let bytes = serde_json::to_vec(&invalid)?;
            fs::write(&file, &bytes)?;
            let err = Database::open(uri)
                .err()
                .expect("stored Geometry must reject the entire snapshot");
            assert!(matches!(err, Error::Storage(_)), "{err}");
            assert!(
                err.to_string().contains(&format!(
                    "node {node_id}: invalid graph property {path}: unsupported Geometry"
                )),
                "{err}"
            );
            assert_eq!(fs::read(&file)?, bytes);
        }
    }

    // A recognized tag with an invalid coordinate fails open instead of
    // fabricating a replacement point or loading only the other properties.
    fs::write(&file, serde_json::to_vec(&graph)?)?;
    graph["nodes"][0]["properties"]["p"]["y"] = serde_json::Value::Null;
    fs::write(&file, serde_json::to_vec(&graph)?)?;
    assert!(matches!(Database::open(uri), Err(Error::Storage(_))));
    temporal_checkpoint_example()?;
    Ok(())
}

fn temporal_checkpoint_example() -> Result<(), Box<dyn StdError>> {
    let dir = tempfile::tempdir()?;
    let uri = dir.path().to_str().expect("temporary path is UTF-8");
    let db = Database::open_with(
        uri,
        OpenOptions {
            snapshot_codec: Some(SnapshotCodecKind::Json),
            checkpoint_interval: Some(Duration::ZERO),
            ..Default::default()
        },
    )?;
    let mut conn = db.connect()?;
    conn.query(
        "CREATE (:TemporalSnapshot {name: 'valid', d: date('2024-02-29'), \
         t: time('12:34:56.123+01:30'), dt: datetime('2024-02-29T12:34:56.123-05:30'), \
         dur: duration('P2DT3S'), p: point({x: 1.5, y: -2.25}), \
         v: vector([1, 2], 2, INTEGER)})",
    )?;
    let read = "MATCH (n:TemporalSnapshot) RETURN n.d, n.t, n.dt, n.dur, n.p, n.v";
    let expected = conn.query(read)?.rows;
    assert_eq!(expected.len(), 1);
    assert!(expected[0][..4]
        .iter()
        .all(|v| matches!(v, Value::Temporal(_))));
    assert!(matches!(&expected[0][4], Value::Point(p) if p.x == 1.5 && p.y == -2.25));
    assert!(
        matches!(&expected[0][5], Value::Vector { values, .. } if **values == vec![Value::Int(1), Value::Int(2)])
    );
    db.checkpoint()?;
    drop(conn);
    drop(db);

    let db = Database::open(uri)?;
    let mut conn = db.connect()?;
    assert_eq!(conn.query(read)?.rows, expected);
    let graph_path = dir.path().join("graph.json");
    let manifest_path = dir.path().join("manifest.json");
    let prior_graph = fs::read(&graph_path)?;
    let prior_manifest = fs::read(&manifest_path)?;
    let snapshot: serde_json::Value = serde_json::from_slice(&prior_graph)?;
    let node_id = snapshot["nodes"][0]["id"]
        .as_i64()
        .expect("checkpoint node ID");

    let Value::Temporal(Temporal::Time { time, .. }) = &expected[0][1] else {
        unreachable!("the Time query produced a temporal value")
    };
    let Value::Temporal(Temporal::DateTime { utc, zone, .. }) = &expected[0][2] else {
        unreachable!("the DateTime query produced a temporal value")
    };
    // Pass core values directly: parsing an ISO string cannot construct these
    // unrepresentable variants/offsets, but Rust clients can supply them.
    let invalid = [
        (
            Value::Temporal(Temporal::CivilDate {
                year: i64::MAX,
                month: 1,
                day: 1,
            }),
            "$.bad",
            "unsupported CivilDate",
        ),
        (
            Value::List(
                vec![Value::Temporal(Temporal::CivilDateTime {
                    year: 2024,
                    month: 2,
                    day: 29,
                    time: *time,
                })]
                .into(),
            ),
            "$.bad[0]",
            "unsupported CivilDateTime",
        ),
        (
            Value::Temporal(Temporal::Duration {
                months: 0,
                days: 0,
                nanos: (i64::MAX as i128 + 1) * 1_000_000_000,
            }),
            "$.bad",
            "Duration seconds out of range",
        ),
        (
            Value::Temporal(Temporal::Duration {
                months: 0,
                days: 0,
                nanos: (i64::MIN as i128 - 1) * 1_000_000_000,
            }),
            "$.bad",
            "Duration seconds out of range",
        ),
        (
            Value::List(
                vec![Value::Temporal(Temporal::Time {
                    time: *time,
                    offset: 1,
                })]
                .into(),
            ),
            "$.bad[0]",
            "invalid temporal offset",
        ),
        (
            Value::Temporal(Temporal::Time {
                time: *time,
                offset: 86_400,
            }),
            "$.bad",
            "invalid temporal offset",
        ),
        (
            Value::List(
                vec![Value::Temporal(Temporal::DateTime {
                    utc: *utc,
                    offset: -1,
                    zone: zone.clone(),
                })]
                .into(),
            ),
            "$.bad[0]",
            "invalid temporal offset",
        ),
        (
            Value::Temporal(Temporal::DateTime {
                utc: *utc,
                offset: -86_400,
                zone: zone.clone(),
            }),
            "$.bad",
            "invalid temporal offset",
        ),
    ];
    for (value, path, reason) in invalid {
        conn.query_with_params(
            "MATCH (n:TemporalSnapshot) SET n.bad = $bad",
            BTreeMap::from([("bad".into(), value)]),
        )?;
        let err = db.checkpoint().expect_err("lossy temporal checkpoint");
        assert!(matches!(err, Error::Storage(_)), "{err}");
        assert!(
            err.to_string().contains(&format!(
                "node {node_id}: invalid graph property {path}: {reason}"
            )),
            "{err}"
        );
        assert_eq!(fs::read(&graph_path)?, prior_graph);
        assert_eq!(fs::read(&manifest_path)?, prior_manifest);
    }

    // The committed bad property can be corrected and a later checkpoint retried.
    conn.query("MATCH (n:TemporalSnapshot) SET n.bad = null")?;
    db.checkpoint()?;
    assert_eq!(conn.query(read)?.rows, expected);
    Ok(())
}
