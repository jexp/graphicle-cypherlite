use super::*;

#[test]
fn every_value_fixture_matches_canonical_encoding() {
    // The core golden assertion against the module itself.
    golden::assert_all_match(value_to_wire_json);
}

#[test]
fn no_structured_variant_is_a_display_string() {
    // Guards the historical bug: Node/Rel/Path/Point/Geometry/Vector must
    // be JSON objects/arrays, never stringified. (Temporals are ISO
    // strings by design and are excluded here.)
    for f in golden::value_fixtures() {
        let got = value_to_wire_json(&f.value);
        let is_structured_variant = matches!(
            f.value,
            CypherValue::Node { .. }
                | CypherValue::Relationship { .. }
                | CypherValue::Path { .. }
                | CypherValue::Point(_)
                | CypherValue::Geometry(_)
                | CypherValue::Vector { .. }
                | CypherValue::List(_)
                | CypherValue::Map(_)
        );
        if is_structured_variant {
            assert!(
                got.is_object() || got.is_array(),
                "fixture `{}` must encode structured, got {}",
                f.name,
                got
            );
        }
    }
}

#[test]
fn vector_is_structured_not_display_string() {
    let v = CypherValue::Vector {
        coord: crate::value::VectorCoordKind::Int8,
        dim: 2,
        values: vec![CypherValue::Int(1), CypherValue::Int(-2)],
    };
    let got = value_to_wire_json(&v);
    assert_eq!(
        got,
        serde_json::json!({"type": "INTEGER8", "dim": 2, "values": [1, -2]})
    );
}

#[test]
fn stats_includes_all_eleven_with_zeros() {
    let (snap, expected) = golden::stats_fixture();
    let got = stats_to_wire_json(&snap);
    assert_eq!(got, expected);
    // All 11 keys present even where the value is zero.
    let obj = got.as_object().unwrap();
    assert_eq!(obj.len(), 11);
    assert_eq!(obj.get("labels_removed"), Some(&Value::Number(0u64.into())));
}

#[test]
fn default_stats_are_all_zero_but_present() {
    let got = stats_to_wire_json(&WireStatsSnapshot::default());
    let obj = got.as_object().unwrap();
    assert_eq!(obj.len(), 11);
    for (_k, v) in obj {
        assert_eq!(v, &Value::Number(0u64.into()));
    }
}

/// [`stats_from_wire_json`] is the encoder's inverse: a snapshot whose 11
/// counters are all distinct round-trips, so a counter wired to the wrong
/// field fails here.
#[test]
fn stats_from_wire_json_inverts_the_encoder() {
    let snap = WireStatsSnapshot {
        nodes_created: 1,
        nodes_deleted: 2,
        relationships_created: 3,
        relationships_deleted: 4,
        properties_set: 5,
        labels_added: 6,
        labels_removed: 7,
        indexes_added: 8,
        indexes_removed: 9,
        constraints_added: 10,
        constraints_removed: 11,
    };
    let wire = stats_to_wire_json(&snap);
    assert_eq!(
        stats_from_wire_json(wire.as_object().unwrap()).unwrap(),
        snap
    );
}

/// A canonical counter that is missing or not an unsigned integer is the
/// inverse's error, naming the entry — the text the recording reader
/// surfaces verbatim. Entries outside the canonical set are ignored.
#[test]
fn stats_from_wire_json_names_the_offending_counter() {
    let mut stats = stats_to_wire_json(&WireStatsSnapshot::default())
        .as_object()
        .unwrap()
        .clone();
    stats.remove("properties_set");
    assert_eq!(
        stats_from_wire_json(&stats).unwrap_err(),
        "stats entry `properties_set` is missing or not an unsigned integer"
    );
    stats.insert("properties_set".into(), Value::String("5".into()));
    assert_eq!(
        stats_from_wire_json(&stats).unwrap_err(),
        "stats entry `properties_set` is missing or not an unsigned integer"
    );
    stats.insert("properties_set".into(), Value::from(5));
    stats.insert("not_a_counter".into(), Value::from(1));
    assert!(stats_from_wire_json(&stats).is_ok());
}

/// Every counter's Bolt key is its envelope key respelled to Neo4j's
/// kebab-case wire vocabulary (ADR-0309), derived by
/// [`StatCounter::bolt_key`]: the expected spellings are pinned here so a
/// change to the derivation or to an envelope key fails here, not on the
/// wire.
#[test]
fn every_counter_carries_its_bolt_wire_key() {
    let expected = [
        "nodes-created",
        "nodes-deleted",
        "relationships-created",
        "relationships-deleted",
        "properties-set",
        "labels-added",
        "labels-removed",
        "indexes-added",
        "indexes-removed",
        "constraints-added",
        "constraints-removed",
    ];
    for (counter, expected) in STAT_COUNTERS.iter().zip(expected) {
        assert_eq!(counter.bolt_key(), expected, "{}", counter.key);
    }
}

#[test]
fn success_envelope_shape() {
    let columns = vec!["a".to_string(), "b".to_string()];
    let mut row = IndexMap::new();
    row.insert("a".to_string(), CypherValue::Int(1));
    row.insert("b".to_string(), CypherValue::Str("x".into()));
    let rows = vec![row_to_wire_json(&columns, &row)];
    let stats = stats_to_wire_json(&WireStatsSnapshot::default());
    let column_types = vec!["INTEGER".to_string(), "STRING".to_string()];
    let env = response_envelope(
        &columns,
        &column_types,
        rows,
        stats,
        &[],
        golden::FIXTURE_ELAPSED_MS,
        None,
    );

    let obj = env.as_object().unwrap();
    // Exactly the eight canonical keys, with `rows` (not `data`).
    assert_eq!(obj.len(), 8);
    // LABS-418: the structured plan is always present, null here.
    assert!(obj["plan"].is_null());
    assert!(obj.contains_key("columns"));
    assert!(obj.contains_key("rows"));
    assert!(obj.contains_key("stats"));
    // ADR-0363 decision 9: always present, empty on a plain run.
    assert_eq!(env[notification_keys::NOTIFICATIONS], serde_json::json!([]));
    // ADR-0224: the per-column type names, beside `columns`, same order.
    assert_eq!(
        env.get("columnTypes"),
        Some(&serde_json::json!(["INTEGER", "STRING"]))
    );
    // ADR-0221: the engine-reported statement elapsed, in milliseconds.
    assert_eq!(obj.get("elapsed_ms"), Some(&Value::from(0.0)));
    assert_eq!(obj.get("error"), Some(&Value::Null));
    assert_eq!(env["rows"][0], serde_json::json!({"a": 1, "b": "x"}));
}

/// ADR-0363 decision 9: the envelope's `notifications` array carries the
/// engine-native objects, in order, and stays present when non-empty.
#[test]
fn success_envelope_carries_native_notification_objects() {
    let notification = super::Notification::new(
        super::NotificationKind::CartesianProduct,
        "the MATCH patterns share no join anchor",
    )
    .with_operator_id(2);
    let env = response_envelope(
        &[],
        &[],
        Vec::new(),
        stats_to_wire_json(&WireStatsSnapshot::default()),
        std::slice::from_ref(&notification),
        golden::FIXTURE_ELAPSED_MS,
        None,
    );
    let entries = env[notification_keys::NOTIFICATIONS]
        .as_array()
        .expect("the array is present");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["code"], notification.code());
    assert_eq!(entries[0]["gql_status"], "03N90");
    assert_eq!(entries[0]["operator_id"], 2);
    assert_eq!(entries[0]["remedy"], notification.remedy());
}

#[test]
fn column_type_name_covers_the_adr_0224_set() {
    // One name per variant of the golden row's values, in column order; the
    // golden envelope pins the same derivation, this pins the mapping itself.
    let (_, row, _) = golden::golden_row();
    let names: Vec<&str> = row.values().map(super::column_type_name).collect();
    assert!(names.iter().all(|n| {
        [
            "STRING",
            "INTEGER",
            "FLOAT",
            "BOOLEAN",
            "NODE",
            "RELATIONSHIP",
            "PATH",
            "LIST",
            "MAP",
            "POINT",
            "DATETIME",
            "DATE",
            "TIME",
            "DURATION",
            "VECTOR",
            "UUID",
            "BINARY",
            "ANY",
        ]
        .contains(n)
    }));
    // Null stamps ANY (ADR-0224 stamping rule).
    assert_eq!(super::column_type_name(&CypherValue::Null), "ANY");
}

#[test]
fn row_missing_column_is_null() {
    let columns = vec!["a".to_string(), "missing".to_string()];
    let mut row = IndexMap::new();
    row.insert("a".to_string(), CypherValue::Int(1));
    let got = row_to_wire_json(&columns, &row);
    assert_eq!(got, serde_json::json!({"a": 1, "missing": null}));
}

#[test]
fn error_envelope_shape() {
    let env = error_envelope("boom");
    assert_eq!(
        env,
        serde_json::json!({
            "columns": [],
            "rows": [],
            "stats": {},
            "error": "boom"
        })
    );
}

#[test]
fn golden_row_envelope_round_trips_through_helpers() {
    let (columns, row, expected) = golden::golden_row();
    let (snap, _) = golden::stats_fixture();
    let rows = rows_for_result(&columns, std::slice::from_ref(&row));
    let column_types = golden::column_types(&row);
    let env = response_envelope(
        &columns,
        &column_types,
        rows,
        stats_to_wire_json(&snap),
        &[],
        golden::FIXTURE_ELAPSED_MS,
        None,
    );
    assert_eq!(env, expected);
}

#[test]
fn rows_for_result_drops_empty_columns_rows() {
    // Write-only: no columns, one empty driving row -> empty rows array
    // (never `[{}]`).
    let empty_row = IndexMap::new();
    let rows = rows_for_result(&[], std::slice::from_ref(&empty_row));
    assert!(
        rows.is_empty(),
        "write-only rows must be empty, got {rows:?}"
    );

    // With columns, every row is encoded.
    let cols = vec!["a".to_string()];
    let mut r = IndexMap::new();
    r.insert("a".to_string(), CypherValue::Int(1));
    let rows = rows_for_result(&cols, std::slice::from_ref(&r));
    assert_eq!(rows, vec![serde_json::json!({"a": 1})]);
}

#[test]
fn write_only_envelope_matches_golden() {
    // The shared write-only fixture assembled through the shared helpers is
    // the exact envelope every surface must emit (rows: [], not [{}]).
    let (columns, rows, stats, expected) = golden::envelope_write_only();
    let wire_rows = rows_for_result(&columns, &rows);
    let env = response_envelope(
        &columns,
        &[],
        wire_rows,
        stats_to_wire_json(&stats),
        &[],
        golden::FIXTURE_ELAPSED_MS,
        None,
    );
    assert_eq!(env, expected);
    assert_eq!(env["rows"], serde_json::json!([]));
}

#[test]
fn non_finite_floats_encode_as_display_strings() {
    // ADR-0280: non-finite floats render as the `CypherValue` `Display`
    // spellings — `"NaN"`/`"Inf"`/`"-Inf"` — so a non-finite float is
    // never indistinguishable from a genuine `null` on the wire.
    assert_eq!(
        value_to_wire_json(&CypherValue::Float(f64::NAN)),
        Value::String("NaN".into())
    );
    assert_eq!(
        value_to_wire_json(&CypherValue::Float(f64::INFINITY)),
        Value::String("Inf".into())
    );
    assert_eq!(
        value_to_wire_json(&CypherValue::Float(f64::NEG_INFINITY)),
        Value::String("-Inf".into())
    );
}

#[test]
fn point_geographic_3d_has_height_and_marker() {
    let p = CypherValue::Point(PointValue::wgs84_3d(1.0, 2.0, 3.0));
    assert_eq!(
        value_to_wire_json(&p),
        serde_json::json!({
            "srid": 4979,
            "crs": "wgs-84-3d",
            "longitude": 1.0,
            "latitude": 2.0,
            "height": 3.0
        })
    );
}

#[test]
fn point_cartesian_3d_has_z_and_marker() {
    let p = CypherValue::Point(PointValue::cartesian_3d(1.0, 2.0, 3.0));
    assert_eq!(
        value_to_wire_json(&p),
        serde_json::json!({
            "srid": 9157,
            "crs": "cartesian-3d",
            "x": 1.0,
            "y": 2.0,
            "z": 3.0
        })
    );
}

/// The typed warnings vocabulary (typed node warnings, LABS-461: one
/// warning type): every variant carries its wire name and message text,
/// and the parse rejects a name or payload shape outside the vocabulary
/// the wire accessor fails loudly on.
#[test]
fn plan_warning_covers_the_typed_vocabulary() {
    use super::plan_warning::{
        WARNING_ALL_NODES_SCAN, WARNING_CARTESIAN_PRODUCT, WARNING_ESTIMATION_MISS,
    };
    use super::PlanWarning;
    assert_eq!(PlanWarning::EstimationMiss.name(), WARNING_ESTIMATION_MISS);
    assert_eq!(
        PlanWarning::EstimationMiss.message(),
        "estimated rows 10x or more above the actual rows"
    );
    assert_eq!(
        PlanWarning::CartesianProduct.name(),
        WARNING_CARTESIAN_PRODUCT
    );
    assert_eq!(
        PlanWarning::CartesianProduct.message(),
        "the MATCH patterns share no join anchor"
    );
    assert_eq!(
        PlanWarning::AllNodesScan {
            index: ":Person(name)".to_string()
        }
        .name(),
        WARNING_ALL_NODES_SCAN
    );
    assert_eq!(
        PlanWarning::AllNodesScan {
            index: ":Person(name)".to_string()
        }
        .message(),
        "the scan matches every node; consider the property index (:Person(name))"
    );
    // The parse: the vocabulary's names with the payload shape their
    // variant carries, and `None` on anything outside — a wire-contract
    // break the accessor keeps loud.
    assert_eq!(
        PlanWarning::from_wire_parts("EstimationMiss", None),
        Some(PlanWarning::EstimationMiss)
    );
    assert_eq!(
        PlanWarning::from_wire_parts("CartesianProduct", None),
        Some(PlanWarning::CartesianProduct)
    );
    assert_eq!(
        PlanWarning::from_wire_parts("AllNodesScan", Some(":Person(name)")),
        Some(PlanWarning::AllNodesScan {
            index: ":Person(name)".to_string()
        })
    );
    assert_eq!(
        PlanWarning::from_wire_parts("NotAWarning", None),
        None,
        "outside the vocabulary"
    );
    assert_eq!(
        PlanWarning::from_wire_parts("AllNodesScan", None),
        None,
        "the scan warning without its index payload"
    );
    assert_eq!(
        PlanWarning::from_wire_parts("CartesianProduct", Some(":Person(name)")),
        None,
        "a payload on a payload-less variant"
    );
}
