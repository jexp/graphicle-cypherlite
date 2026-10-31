use super::*;
use crate::value::{GeometryValue, IsoDuration, PointValue, VectorCoordKind};
use chrono::{FixedOffset, NaiveDate, NaiveTime, TimeZone};

/// The fixed `elapsed_ms` the envelope fixtures pin (ADR-0221): real
/// surfaces pass the engine-reported value; fixtures use `0.0` so the
/// expected envelope is deterministic.
pub const FIXTURE_ELAPSED_MS: f64 = 0.0;

/// A single named value fixture: a `CypherValue` and its expected canonical
/// wire JSON.
pub struct ValueFixture {
    pub name: &'static str,
    pub value: CypherValue,
    pub expected: Value,
}

fn node_alice() -> CypherValue {
    let mut props = IndexMap::new();
    props.insert("name".to_string(), CypherValue::Str("Alice".into()));
    props.insert("age".to_string(), CypherValue::Int(30));
    CypherValue::Node {
        id: 7,
        labels: vec!["Person".into(), "User".into()],
        properties: props,
    }
}

fn node_bob() -> CypherValue {
    let mut props = IndexMap::new();
    props.insert("name".to_string(), CypherValue::Str("Bob".into()));
    CypherValue::Node {
        id: 8,
        labels: vec!["Person".into()],
        properties: props,
    }
}

fn rel_knows() -> CypherValue {
    let mut props = IndexMap::new();
    props.insert("since".to_string(), CypherValue::Int(2020));
    CypherValue::Relationship {
        id: 11,
        start_id: 7,
        end_id: 8,
        rel_type: "KNOWS".into(),
        properties: props,
    }
}

/// Every variant (plus a heterogeneous list) paired with its expected wire
/// JSON. The expected values are hand-authored (not derived from
/// [`value_to_wire_json`]) so they genuinely pin the contract.
pub fn value_fixtures() -> Vec<ValueFixture> {
    let offset = FixedOffset::east_opt(2 * 3600).unwrap();
    let ndt = NaiveDate::from_ymd_opt(2024, 3, 14)
        .unwrap()
        .and_hms_opt(12, 30, 45)
        .unwrap();
    let zoned = offset.from_local_datetime(&ndt).unwrap();

    vec![
        ValueFixture {
            name: "null",
            value: CypherValue::Null,
            expected: Value::Null,
        },
        ValueFixture {
            name: "bool",
            value: CypherValue::Bool(true),
            expected: serde_json::json!(true),
        },
        ValueFixture {
            name: "int",
            value: CypherValue::Int(42),
            expected: serde_json::json!(42),
        },
        ValueFixture {
            name: "float",
            value: CypherValue::Float(3.5),
            expected: serde_json::json!(3.5),
        },
        // ADR-0280: non-finite floats encode as their `Display` spellings,
        // never `null`.
        ValueFixture {
            name: "float_nan",
            value: CypherValue::Float(f64::NAN),
            expected: serde_json::json!("NaN"),
        },
        ValueFixture {
            name: "float_positive_infinity",
            value: CypherValue::Float(f64::INFINITY),
            expected: serde_json::json!("Inf"),
        },
        ValueFixture {
            name: "float_negative_infinity",
            value: CypherValue::Float(f64::NEG_INFINITY),
            expected: serde_json::json!("-Inf"),
        },
        ValueFixture {
            name: "string",
            value: CypherValue::Str("hello".into()),
            expected: serde_json::json!("hello"),
        },
        ValueFixture {
            name: "list_heterogeneous",
            value: CypherValue::List(vec![
                CypherValue::Int(1),
                CypherValue::Str("two".into()),
                CypherValue::Bool(true),
                CypherValue::Null,
            ]),
            expected: serde_json::json!([1, "two", true, null]),
        },
        ValueFixture {
            name: "map",
            value: {
                let mut m = IndexMap::new();
                m.insert("a".to_string(), CypherValue::Int(1));
                m.insert("b".to_string(), CypherValue::Str("x".into()));
                CypherValue::Map(m)
            },
            expected: serde_json::json!({"a": 1, "b": "x"}),
        },
        ValueFixture {
            name: "node",
            value: node_alice(),
            expected: serde_json::json!({
                "kind": "node",
                "id": 7,
                "labels": ["Person", "User"],
                "properties": {"name": "Alice", "age": 30}
            }),
        },
        ValueFixture {
            name: "relationship",
            value: rel_knows(),
            expected: serde_json::json!({
                "kind": "relationship",
                "id": 11,
                "type": "KNOWS",
                "start_node": 7,
                "end_node": 8,
                "properties": {"since": 2020}
            }),
        },
        ValueFixture {
            name: "path",
            value: CypherValue::Path {
                nodes: vec![node_alice(), node_bob()],
                relationships: vec![rel_knows()],
            },
            expected: serde_json::json!([
                {
                    "kind": "node",
                    "id": 7,
                    "labels": ["Person", "User"],
                    "properties": {"name": "Alice", "age": 30}
                },
                {
                    "kind": "relationship",
                    "id": 11,
                    "type": "KNOWS",
                    "start_node": 7,
                    "end_node": 8,
                    "properties": {"since": 2020}
                },
                {
                    "kind": "node",
                    "id": 8,
                    "labels": ["Person"],
                    "properties": {"name": "Bob"}
                }
            ]),
        },
        ValueFixture {
            name: "date",
            value: CypherValue::Date(NaiveDate::from_ymd_opt(2024, 3, 14).unwrap()),
            expected: serde_json::json!("2024-03-14"),
        },
        ValueFixture {
            name: "time",
            value: CypherValue::Time(NaiveTime::from_hms_opt(12, 30, 45).unwrap(), Some(offset)),
            expected: serde_json::json!("12:30:45+02:00"),
        },
        ValueFixture {
            name: "datetime",
            value: CypherValue::DateTime(zoned, Some("Europe/Stockholm".into())),
            expected: serde_json::json!("2024-03-14T12:30:45+02:00[Europe/Stockholm]"),
        },
        ValueFixture {
            name: "localdatetime",
            value: CypherValue::LocalDateTime(ndt),
            expected: serde_json::json!("2024-03-14T12:30:45"),
        },
        ValueFixture {
            name: "duration",
            value: CypherValue::Duration(IsoDuration::new(0, 0, 3661, 0)),
            expected: serde_json::json!("PT1H1M1S"),
        },
        ValueFixture {
            name: "point_cartesian",
            value: CypherValue::Point(PointValue::cartesian_2d(1.0, 2.0)),
            expected: serde_json::json!({
                "srid": 7203,
                "crs": "cartesian",
                "x": 1.0,
                "y": 2.0
            }),
        },
        ValueFixture {
            name: "point_geographic",
            value: CypherValue::Point(PointValue::wgs84_2d(-73.98, 40.75)),
            expected: serde_json::json!({
                "srid": 4326,
                "crs": "wgs-84",
                "longitude": -73.98,
                "latitude": 40.75
            }),
        },
        ValueFixture {
            name: "geometry",
            value: CypherValue::Geometry(GeometryValue::LineString {
                coordinates: vec![[0.0, 0.0], [1.0, 1.0]],
                srid: 7203,
            }),
            expected: serde_json::json!({
                "type": "LineString",
                "coordinates": [[0.0, 0.0], [1.0, 1.0]]
            }),
        },
        ValueFixture {
            name: "vector",
            value: CypherValue::Vector {
                coord: VectorCoordKind::Float32,
                dim: 3,
                values: vec![
                    CypherValue::Float(1.0),
                    CypherValue::Float(2.0),
                    CypherValue::Float(3.0),
                ],
            },
            expected: serde_json::json!({
                "type": "FLOAT32",
                "dim": 3,
                "values": [1.0, 2.0, 3.0]
            }),
        },
    ]
}

/// A stats snapshot exercising all 11 counters — including a deliberate
/// zero (`labels_removed`) to pin "zeros are included" — paired with its
/// expected wire JSON.
pub fn stats_fixture() -> (WireStatsSnapshot, Value) {
    let snap = WireStatsSnapshot {
        nodes_created: 3,
        nodes_deleted: 1,
        relationships_created: 2,
        relationships_deleted: 1,
        properties_set: 5,
        labels_added: 4,
        labels_removed: 0,
        indexes_added: 1,
        indexes_removed: 1,
        constraints_added: 2,
        constraints_removed: 1,
    };
    let expected = serde_json::json!({
        "nodes_created": 3,
        "nodes_deleted": 1,
        "relationships_created": 2,
        "relationships_deleted": 1,
        "properties_set": 5,
        "labels_added": 4,
        "labels_removed": 0,
        "indexes_added": 1,
        "indexes_removed": 1,
        "constraints_added": 2,
        "constraints_removed": 1
    });
    (snap, expected)
}

/// The ADR-0224 `columnTypes` of the golden row: one
/// [`crate::wire::column_type_name`] per column, in column order. The
/// golden row IS the first row, so this derivation is the fixture's
/// `columnTypes` array — surfaces reproducing the envelope call this
/// instead of re-deriving it.
pub fn column_types(row: &IndexMap<String, CypherValue>) -> Vec<String> {
    row.values()
        .map(|v| crate::wire::column_type_name(v).to_string())
        .collect()
}

/// One result row exercising every variant, plus the expected full success
/// envelope for it. Columns are the fixture names, in order. Any surface
/// that assembles a response from [`value_to_wire_json`],
/// [`stats_to_wire_json`], and [`response_envelope`] must reproduce this
/// envelope exactly — including the ADR-0224 `columnTypes` array, one
/// [`crate::wire::column_type_name`] per column, in column order
/// ([`column_types`]).
pub fn golden_row() -> (Vec<String>, IndexMap<String, CypherValue>, Value) {
    let fixtures = value_fixtures();
    let columns: Vec<String> = fixtures.iter().map(|f| f.name.to_string()).collect();
    let mut row = IndexMap::new();
    let mut row_json = Map::new();
    for f in &fixtures {
        row.insert(f.name.to_string(), f.value.clone());
        row_json.insert(f.name.to_string(), f.expected.clone());
    }
    let (_snap, stats_expected) = stats_fixture();

    let mut envelope = Map::new();
    envelope.insert(
        "columns".into(),
        Value::Array(columns.iter().map(|c| Value::String(c.clone())).collect()),
    );
    // ADR-0224: the engine stamps each column from the first row; the golden
    // row IS the first row, so callers reproduce the fixture by passing
    // `column_types(&row)` to `response_envelope`.
    let column_types: Vec<Value> = column_types(&row).into_iter().map(Value::String).collect();
    envelope.insert("columnTypes".into(), Value::Array(column_types));
    envelope.insert("rows".into(), Value::Array(vec![Value::Object(row_json)]));
    envelope.insert("stats".into(), stats_expected);
    // ADR-0363 decision 9: the success envelope always carries the list;
    // this fixture is a plain run, so it is empty.
    envelope.insert(
        notification_keys::NOTIFICATIONS.into(),
        Value::Array(Vec::new()),
    );
    // ADR-0221: the envelope carries the engine-reported elapsed; the
    // fixture pins the shape with a fixed `0.0`, so callers reproduce the
    // fixture by passing `FIXTURE_ELAPSED_MS` to `response_envelope`.
    envelope.insert("elapsed_ms".into(), Value::from(FIXTURE_ELAPSED_MS));
    // LABS-418: the structured plan document is always present, `null` on a
    // plain run.
    envelope.insert("plan".into(), Value::Null);
    envelope.insert("error".into(), Value::Null);

    (columns, row, Value::Object(envelope))
}

/// The canonical wire envelope for a **write-only** statement (a
/// `CREATE`/`MERGE`/`SET`/… without `RETURN`): no result columns, and the
/// single empty driving row the engine seeds must render as an **empty**
/// `rows` array — never `[{}]`. Returns the raw `(columns, rows, stats)` a
/// surface would hand to [`rows_for_result`]/[`response_envelope`] plus the
/// expected full envelope (with ADR-0224 `"columnTypes": []` — no columns,
/// so no column types), so every surface can pin the empty-columns
/// envelope-assembly path (which the value fixtures never exercise).
pub fn envelope_write_only() -> (
    Vec<String>,
    Vec<IndexMap<String, CypherValue>>,
    WireStatsSnapshot,
    Value,
) {
    // No columns, one empty driving row — exactly what the engine yields for
    // a top-level write with no RETURN.
    let columns: Vec<String> = Vec::new();
    let rows = vec![IndexMap::new()];
    let stats = WireStatsSnapshot {
        nodes_created: 1,
        labels_added: 1,
        ..Default::default()
    };
    let expected = serde_json::json!({
        "columns": [],
        // ADR-0224: no columns, so no column types.
        "columnTypes": [],
        "rows": [],
        "stats": {
            "nodes_created": 1,
            "nodes_deleted": 0,
            "relationships_created": 0,
            "relationships_deleted": 0,
            "properties_set": 0,
            "labels_added": 1,
            "labels_removed": 0,
            "indexes_added": 0,
            "indexes_removed": 0,
            "constraints_added": 0,
            "constraints_removed": 0
        },
        // ADR-0363 decision 9: always present, empty on a plain run.
        (notification_keys::NOTIFICATIONS): [],
        "elapsed_ms": FIXTURE_ELAPSED_MS,
        // LABS-418: always present, null on a plain run.
        "plan": null,
        "error": null
    });
    (columns, rows, stats, expected)
}

/// Assert a candidate value serializer reproduces the canonical wire JSON
/// for every fixture. Surfaces call this with their own
/// `&CypherValue -> serde_json::Value` function so drift becomes a test
/// failure; surfaces pass [`value_to_wire_json`] itself to prove
/// the shared module is reachable and correct from each surface's build.
pub fn assert_all_match<F>(serialize: F)
where
    F: Fn(&CypherValue) -> Value,
{
    for f in value_fixtures() {
        let got = serialize(&f.value);
        assert_eq!(
            got, f.expected,
            "wire encoding drift for fixture `{}`:\n  got:      {}\n  expected: {}",
            f.name, got, f.expected
        );
    }
}
