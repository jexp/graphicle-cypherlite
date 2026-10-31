use super::*;
use chrono::{Duration, TimeZone};

#[test]
#[allow(clippy::approx_constant)]
fn test_roundtrip_simple() {
    let values = vec![
        CypherValue::Null,
        CypherValue::Bool(true),
        CypherValue::Int(42),
        CypherValue::Float(3.14),
        CypherValue::Str("hello".into()),
    ];
    for val in values {
        let json = cypher_value_to_json(&val).unwrap();
        let back = try_json_to_cypher_value(&json).unwrap();
        assert_eq!(format!("{val:?}"), format!("{:?}", back));
    }
}

#[test]
fn test_roundtrip_date() {
    let val = CypherValue::Date(NaiveDate::from_ymd_opt(2024, 3, 14).unwrap());
    let json = cypher_value_to_json(&val).unwrap();
    let back = try_json_to_cypher_value(&json).unwrap();
    assert_eq!(format!("{val:?}"), format!("{:?}", back));
}

#[test]
fn test_roundtrip_point() {
    let val = CypherValue::Point(PointValue::wgs84_2d(-73.98, 40.75));
    let json = cypher_value_to_json(&val).unwrap();
    let back = try_json_to_cypher_value(&json).unwrap();
    match &back {
        CypherValue::Point(p) => {
            assert_eq!(p.srid, 4326);
            assert!((p.x - (-73.98)).abs() < 1e-10);
        }
        _ => panic!("expected Point"),
    }
}

#[test]
fn test_roundtrip_geometry() {
    let val = CypherValue::Geometry(GeometryValue::Polygon {
        rings: vec![vec![
            [0.0, 0.0],
            [4.0, 0.0],
            [4.0, 3.0],
            [0.0, 3.0],
            [0.0, 0.0],
        ]],
        srid: 7203,
    });
    let json = cypher_value_to_json(&val).unwrap();
    let back = try_json_to_cypher_value(&json).unwrap();
    match &back {
        CypherValue::Geometry(GeometryValue::Polygon { rings, srid }) => {
            assert_eq!(*srid, 7203);
            assert_eq!(rings.len(), 1);
            assert_eq!(rings[0].len(), 5);
            assert!((rings[0][0][0] - 0.0).abs() < 1e-10);
            assert!((rings[0][1][0] - 4.0).abs() < 1e-10);
        }
        _ => panic!("expected Polygon geometry, got {back:?}"),
    }
}

#[test]
fn test_roundtrip_list_map() {
    let mut map = IndexMap::new();
    map.insert("key".into(), CypherValue::Int(1));
    let val = CypherValue::Map(map);
    let json = cypher_value_to_json(&val).unwrap();
    let back = try_json_to_cypher_value(&json).unwrap();
    match &back {
        CypherValue::Map(m) => {
            assert_eq!(m.get("key"), Some(&CypherValue::Int(1)));
        }
        _ => panic!("expected Map"),
    }
}

#[test]
fn numeric_offsets_accept_signed_ascii_hours_and_minutes_in_chrono_range() {
    for (text, seconds) in [
        ("+02:30", 9_000),
        ("-05:45", -20_700),
        ("+23:59", 86_340),
        ("-23:59", -86_340),
        ("-00:00", 0),
    ] {
        let offset = parse_numeric_offset(text).unwrap();
        assert_eq!(offset.local_minus_utc(), seconds, "{text}");
        let time = serde_json::json!({"__type":"time", "value":"12:00:00", "offset":text});
        assert_eq!(
            try_json_to_cypher_value(&time).unwrap(),
            CypherValue::Time(NaiveTime::from_hms_opt(12, 0, 0).unwrap(), Some(offset))
        );
    }
}

#[test]
fn numeric_offsets_reject_malformed_unicode_and_out_of_range_input() {
    for text in [
        "+00:60",
        "+24:00",
        "+99:00",
        "+999999999999999999999:00",
        "+2:30",
        "+002:30",
        "+02:3",
        "+02:300",
        "02:30",
        "Z",
        "UTC",
        "é+02:30",
        "é2:30",
        "+é2:30",
        "+é:30",
        "+02:é0",
        "+0x:30",
        "+02:x0",
        "+02-30",
        " +02:30",
        "+02:30 ",
    ] {
        assert_eq!(parse_numeric_offset(text), None, "{text:?}");
        let time = serde_json::json!({"__type":"time", "value":"12:00:00", "offset":text});
        let error = try_json_to_cypher_value(&time).unwrap_err();
        assert_eq!(error.path, "$.offset", "{text:?}");
        assert_eq!(
            error.message, "expected ASCII ±HH:MM offset within 24 hours",
            "{text:?}"
        );
    }
}

#[test]
fn numeric_offsets_reject_ascii_punctuation_below_zero_at_every_digit() {
    assert_eq!(parse_numeric_offset("+0/:30"), None);
    for digit_index in [1, 2, 4, 5] {
        for punctuation in b'!'..=b'/' {
            let mut offset = *b"+02:30";
            offset[digit_index] = punctuation;
            let offset = std::str::from_utf8(&offset).unwrap();
            assert_eq!(parse_numeric_offset(offset), None, "{offset:?}");
        }
    }
}

#[test]
fn punctuation_offsets_return_time_serde_deserialize_errors() {
    for offset in ["+0/:30", "+/2:30", "+02:/0", "+02:3/"] {
        let json = serde_json::json!({"__type":"time", "value":"12:00:00", "offset":offset});
        let encoded = json.to_string();
        let parsed = std::panic::catch_unwind(|| serde_json::from_str::<CypherValue>(&encoded));
        assert!(parsed.is_ok(), "panicked for {offset:?}");
        assert!(parsed.unwrap().is_err(), "accepted {offset:?}");
        let error = try_json_to_cypher_value(&json).unwrap_err();
        assert_eq!(error.path, "$.offset", "{offset:?}");
    }
}

#[test]
fn invalid_offsets_are_serde_errors_not_panics() {
    for offset in [
        "é+02:00",
        "+999999999999999999999:00",
        "+24:00",
        "+02:60",
        "+2:00",
        "  +02:00",
        "+02:00junk",
    ] {
        let json = serde_json::json!({"__type":"time", "value":"12:00:00", "offset":offset});
        let encoded = json.to_string();
        let parsed = std::panic::catch_unwind(|| serde_json::from_str::<CypherValue>(&encoded));
        assert!(parsed.is_ok(), "panicked for {offset:?}");
        assert!(parsed.unwrap().is_err(), "accepted {offset:?}");
    }
}

#[test]
fn subminute_time_and_datetime_offsets_fail_json_and_serde_writes() {
    let time = NaiveTime::from_hms_opt(12, 34, 56).unwrap();
    for seconds in [1, -1] {
        let offset = FixedOffset::east_opt(seconds).unwrap();
        let dt = offset.with_ymd_and_hms(2024, 3, 14, 12, 34, 56).unwrap();
        for value in [
            CypherValue::Time(time, Some(offset)),
            CypherValue::DateTime(dt, Some("Europe/Stockholm".into())),
        ] {
            let error = cypher_value_to_json(&value).unwrap_err();
            assert_eq!(error.path, "$", "{value:?}");
            assert!(error.message.contains("minutes"), "{error}");
            for serde_error in [
                serde_json::to_value(&value).unwrap_err().to_string(),
                serde_json::to_string(&value).unwrap_err().to_string(),
            ] {
                assert!(serde_error.contains("$: "), "{serde_error}");
                assert!(serde_error.contains("minutes"), "{serde_error}");
            }
        }
        // A DateTime without a timezone name still has a fixed offset to validate.
        assert!(cypher_value_to_json(&CypherValue::DateTime(dt, None)).is_err());
    }
}

#[test]
fn subminute_offsets_report_nested_map_list_and_property_paths() {
    let offset = FixedOffset::west_opt(1).unwrap();
    let time = CypherValue::Time(NaiveTime::from_hms_opt(12, 0, 0).unwrap(), Some(offset));
    let datetime = CypherValue::DateTime(
        offset.with_ymd_and_hms(2024, 3, 14, 12, 0, 0).unwrap(),
        None,
    );
    for value in [time, datetime] {
        let nested = CypherValue::Map(IndexMap::from([
            ("__type".into(), CypherValue::Str("user data".into())),
            ("items".into(), CypherValue::List(vec![value])),
        ]));
        let outer = CypherValue::Map(IndexMap::from([("payload".into(), nested.clone())]));
        assert_eq!(
            cypher_value_to_json(&outer).unwrap_err().path,
            "$.payload.items[0]"
        );
        assert_eq!(
            props_to_json(&IndexMap::from([("payload".into(), nested)]))
                .unwrap_err()
                .path,
            "$.payload.items[0]"
        );
        let error = serde_json::to_string(&outer).unwrap_err().to_string();
        assert!(error.contains("$.payload.items[0]"), "{error}");
    }
}

#[test]
fn datetime_out_of_range_local_time_fails_json_serde_and_nested_properties() {
    for (utc, seconds) in [(NaiveDateTime::MAX, 3600), (NaiveDateTime::MIN, -3600)] {
        let offset = FixedOffset::east_opt(seconds).unwrap();
        let datetime = offset.from_utc_datetime(&utc);
        for name in [None, Some("Europe/Stockholm".to_string())] {
            let value = CypherValue::DateTime(datetime, name);
            let error = cypher_value_to_json(&value).unwrap_err();
            assert_eq!(error.path, "$");
            assert!(error.message.contains("local time out of range"), "{error}");
            let nested = CypherValue::Map(IndexMap::from([(
                "items".into(),
                CypherValue::List(vec![value]),
            )]));
            let outer = CypherValue::Map(IndexMap::from([("payload".into(), nested.clone())]));
            assert_eq!(
                cypher_value_to_json(&outer).unwrap_err().path,
                "$.payload.items[0]"
            );
            assert_eq!(
                props_to_json(&IndexMap::from([("payload".into(), nested)]))
                    .unwrap_err()
                    .path,
                "$.payload.items[0]"
            );
            for error in [
                serde_json::to_string(&outer).unwrap_err().to_string(),
                serde_json::to_value(&outer).unwrap_err().to_string(),
            ] {
                assert!(error.contains("$.payload.items[0]"), "{error}");
            }
        }
    }
}

#[test]
fn datetime_near_local_range_limits_roundtrips_with_offset() {
    for (utc, seconds) in [
        (NaiveDateTime::MAX - Duration::hours(1), 3600),
        (NaiveDateTime::MIN + Duration::hours(1), -3600),
    ] {
        let offset = FixedOffset::east_opt(seconds).unwrap();
        let value = CypherValue::DateTime(offset.from_utc_datetime(&utc), None);
        let json = cypher_value_to_json(&value).unwrap();
        assert_eq!(try_json_to_cypher_value_v2(&json).unwrap(), value, "{json}");
        assert_eq!(
            serde_json::from_value::<CypherValue>(json.clone()).unwrap(),
            value
        );
        assert_eq!(serde_json::to_value(&value).unwrap(), json);
    }
}

#[test]
fn minute_aligned_time_and_datetime_keep_v2_shape_and_instant() {
    let time = NaiveTime::from_hms_nano_opt(12, 34, 56, 789_000_000).unwrap();
    let local = NaiveDate::from_ymd_opt(2024, 3, 14)
        .unwrap()
        .and_hms_nano_opt(12, 34, 56, 789_000_000)
        .unwrap();
    for (seconds, text) in [(9_000, "+02:30"), (-9_000, "-02:30"), (0, "+00:00")] {
        let offset = FixedOffset::east_opt(seconds).unwrap();
        let zoned_time = CypherValue::Time(time, Some(offset));
        let time_json = cypher_value_to_json(&zoned_time).unwrap();
        assert_eq!(
            time_json,
            serde_json::json!({"__type":"time", "value":"12:34:56.789", "offset":text})
        );
        assert_eq!(try_json_to_cypher_value_v2(&time_json).unwrap(), zoned_time);
        assert_eq!(serde_json::to_value(&zoned_time).unwrap(), time_json);

        let original = offset.from_local_datetime(&local).single().unwrap();
        for name in [None, Some("Europe/Stockholm".to_string())] {
            let datetime = CypherValue::DateTime(original, name.clone());
            let json = cypher_value_to_json(&datetime).unwrap();
            assert_eq!(json["__type"], "datetime");
            assert_eq!(json["value"], format!("2024-03-14T12:34:56.789{text}"));
            match &name {
                Some(name) => assert_eq!(json["timezone"], *name),
                None => assert!(json.get("timezone").is_none()),
            }
            let CypherValue::DateTime(decoded, decoded_name) =
                try_json_to_cypher_value_v2(&json).unwrap()
            else {
                panic!("expected DateTime")
            };
            assert_eq!(decoded.offset(), original.offset());
            assert_eq!(decoded.naive_local(), original.naive_local());
            assert_eq!(decoded.timestamp(), original.timestamp());
            assert_eq!(
                decoded.timestamp_subsec_nanos(),
                original.timestamp_subsec_nanos()
            );
            assert_eq!(decoded_name, name);
            assert_eq!(serde_json::to_value(&datetime).unwrap(), json);
        }
    }
    let unzoned = CypherValue::Time(time, None);
    let json = cypher_value_to_json(&unzoned).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"__type":"time", "value":"12:34:56.789"})
    );
    assert_eq!(try_json_to_cypher_value_v2(&json).unwrap(), unzoned);
}

#[test]
fn typed_decode_rejects_missing_wrong_and_out_of_range_fields() {
    let bad = [
        (
            serde_json::json!({"__type":"date","value":"2024-99-01"}),
            "$.value",
        ),
        (
            serde_json::json!({"__type":"time","value":"nonsense"}),
            "$.value",
        ),
        (
            serde_json::json!({"__type":"datetime","value":"yesterday"}),
            "$.value",
        ),
        (
            serde_json::json!({"__type":"localdatetime","value":"today"}),
            "$.value",
        ),
        (
            serde_json::json!({"__type":"duration","months":1,"days":0,"seconds":0}),
            "$.nanoseconds",
        ),
        (
            serde_json::json!({"__type":"duration","months":1.5,"days":0,"seconds":0,"nanoseconds":0}),
            "$.months",
        ),
        (
            serde_json::json!({"__type":"point","srid":4294967295u64,"x":1,"y":2}),
            "$.srid",
        ),
        (
            serde_json::json!({"__type":"point","srid":4326,"x":1}),
            "$.y",
        ),
        (
            serde_json::json!({"__type":"point","srid":4326,"x":1,"y":2,"z":null}),
            "$.z",
        ),
        (
            serde_json::json!({"__type":"point","srid":4326,"x":1,"y":2,"unexpected":0}),
            "$.unexpected",
        ),
        (
            serde_json::json!({"__type":"geometry","geometry_type":"LineString","srid":7203,"coordinates":[[1,2,3]]}),
            "$.coordinates[0]",
        ),
        (
            serde_json::json!({"__type":"geometry","geometry_type":"MultiPolygon","srid":7203,"coordinates":[[[[1,2]],null]]}),
            "$.coordinates[0][1]",
        ),
        (
            serde_json::json!({"__type":"geometry","geometry_type":"GeometryCollection","srid":7203,"geometries":[{"__type":"point","srid":4326,"x":1,"y":2}]}),
            "$.geometries[0]",
        ),
        (
            serde_json::json!({"__type":"geometry","geometry_type":"unknown","srid":7203,"coordinates":[]}),
            "$.geometry_type",
        ),
        (
            serde_json::json!({"__type":"vector","coord":"INTEGER8","dim":1,"values":[128]}),
            "$.values[0]",
        ),
        (
            serde_json::json!({"__type":"vector","coord":"FLOAT32","dim":1,"values":[1.0e100]}),
            "$.values[0]",
        ),
        (
            serde_json::json!({"__type":"vector","coord":"FLOAT","dim":1,"values":[1]}),
            "$.values[0]",
        ),
        (
            serde_json::json!({"__type":"vector","coord":"INTEGER","dim":0,"values":[1]}),
            "$.dim",
        ),
        (
            serde_json::json!({"__type":"vector","coord":"INTEGER64","dim":1,"values":[1]}),
            "$.coord",
        ),
        (
            serde_json::json!({"__type":"map","value":{"a":1},"extra":2}),
            "$.extra",
        ),
        (serde_json::json!({"__type":"map","value":[]}), "$.value"),
    ];
    for (json, path) in bad {
        assert_eq!(
            try_json_to_cypher_value(&json).unwrap_err().path,
            path,
            "{json}"
        );
        assert!(serde_json::from_value::<CypherValue>(json).is_err());
    }
    let nested =
        serde_json::json!({"profile":{"history":[0,{"__type":"point","srid":7203,"x":1}]}});
    assert_eq!(
        try_json_to_cypher_value(&nested).unwrap_err().path,
        "$.profile.history[1].y"
    );
    let props = serde_json::json!({"location":{"__type":"point","srid":7203,"x":1}});
    assert_eq!(
        try_json_to_props(props.as_object().unwrap())
            .unwrap_err()
            .path,
        "$.location.y"
    );
}

#[test]
fn escaped_user_maps_round_trip_recursively() {
    let mut inner = IndexMap::new();
    inner.insert("__type".into(), CypherValue::Str("point".into()));
    inner.insert("x".into(), CypherValue::Int(1));
    let mut outer = IndexMap::new();
    outer.insert("lookalike".into(), CypherValue::Map(inner.clone()));
    outer.insert(
        "other".into(),
        CypherValue::List(vec![CypherValue::Map(inner.clone())]),
    );
    let json = cypher_value_to_json(&CypherValue::Map(inner.clone())).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"__type":"map","value":{"__type":"point","x":1}})
    );
    assert_eq!(
        try_json_to_cypher_value_v2(&json).unwrap(),
        CypherValue::Map(inner)
    );
    let json = cypher_value_to_json(&CypherValue::Map(outer.clone())).unwrap();
    assert_eq!(json["lookalike"]["__type"], "map");
    assert_eq!(json["other"][0]["__type"], "map");
    assert_eq!(
        serde_json::from_value::<CypherValue>(json).unwrap(),
        CypherValue::Map(outer)
    );
    let mut complex_key = IndexMap::new();
    complex_key.insert(
        "__type".into(),
        CypherValue::Date(NaiveDate::from_ymd_opt(2024, 3, 14).unwrap()),
    );
    let json = cypher_value_to_json(&CypherValue::Map(complex_key.clone())).unwrap();
    assert_eq!(json["value"]["__type"]["__type"], "date");
    assert_eq!(
        try_json_to_cypher_value(&json).unwrap(),
        CypherValue::Map(complex_key)
    );
}

#[test]
fn legacy_tags_and_unknown_maps_remain_readable() {
    let legacy = [
        serde_json::json!({"__type":"date","value":"2024-03-14"}),
        serde_json::json!({"__type":"time","value":"12:30:00","offset":"-00:30"}),
        serde_json::json!({"__type":"datetime","value":"2024-03-14T12:30:00+02:00","timezone":"Europe/Stockholm"}),
        serde_json::json!({"__type":"localdatetime","value":"2024-03-14T12:30:00"}),
        serde_json::json!({"__type":"duration","months":1,"days":2,"seconds":3,"nanoseconds":4}),
        serde_json::json!({"__type":"point","srid":4326,"x":1.0,"y":2.0}),
        serde_json::json!({"__type":"geometry","geometry_type":"GeometryCollection","srid":7203,"geometries":[{"__type":"geometry","geometry_type":"MultiPoint","srid":7203,"coordinates":[[1,2]]}]}),
        serde_json::json!({"__type":"vector","coord":"FLOAT32","dim":1,"values":[1.5]}),
    ];
    for json in legacy {
        let value = try_json_to_cypher_value_legacy(&json).unwrap();
        assert_eq!(try_json_to_cypher_value(&json).unwrap(), value);
        assert_eq!(
            try_json_to_cypher_value(&cypher_value_to_json(&value).unwrap()).unwrap(),
            value
        );
    }
    let historical_map = serde_json::json!({"__type":"map","value":{"x":1}});
    assert_eq!(
        try_json_to_cypher_value_legacy(&historical_map).unwrap(),
        CypherValue::Map(IndexMap::from([
            ("__type".into(), CypherValue::Str("map".into())),
            (
                "value".into(),
                CypherValue::Map(IndexMap::from([("x".into(), CypherValue::Int(1))]))
            )
        ]))
    );
    assert_eq!(
        try_json_to_cypher_value_v2(&historical_map).unwrap(),
        CypherValue::Map(IndexMap::from([("x".into(), CypherValue::Int(1))]))
    );
    let unknown = serde_json::json!({"__type":"future","payload":true});
    assert!(matches!(
        try_json_to_cypher_value(&unknown),
        Ok(CypherValue::Map(_))
    ));
    assert!(matches!(
        try_json_to_cypher_value(&serde_json::json!({"__type":7,"value":true})),
        Ok(CypherValue::Map(_))
    ));
    // Known tags fail in both versions rather than being coerced to maps.
    let malformed = serde_json::json!({"__type":"point","x":1});
    assert!(try_json_to_cypher_value(&malformed).is_err());
    assert!(try_json_to_cypher_value_legacy(&malformed).is_err());
}

#[test]
fn oversized_json_integers_fail_instead_of_changing_type() {
    let huge = serde_json::json!(u64::MAX);
    assert_eq!(try_json_to_cypher_value_v2(&huge).unwrap_err().path, "$");
    assert_eq!(
        try_json_to_cypher_value_legacy(&huge).unwrap_err().path,
        "$"
    );
    assert!(serde_json::from_value::<CypherValue>(huge).is_err());

    let nested = serde_json::json!({"data":{"__type":"map","value":{"count":u64::MAX}}});
    assert_eq!(
        try_json_to_cypher_value_v2(&nested).unwrap_err().path,
        "$.data.count"
    );
    let props = serde_json::json!({"count":u64::MAX});
    assert_eq!(
        try_json_to_props_v2(props.as_object().unwrap())
            .unwrap_err()
            .path,
        "$.count"
    );
    assert_eq!(
        try_json_to_props_legacy(props.as_object().unwrap())
            .unwrap_err()
            .path,
        "$.count"
    );
    assert_eq!(try_json_to_param_value(&props).unwrap_err().path, "$.count");
}

#[test]
fn parameter_maps_never_infer_persistence_tags() {
    let input =
        serde_json::json!({"__type":"point","x":1,"nested":[{"__type":"duration","months":1}]});
    let CypherValue::Map(map) = try_json_to_param_value(&input).unwrap() else {
        panic!("expected map")
    };
    assert_eq!(map["__type"], CypherValue::Str("point".into()));
    assert_eq!(map["x"], CypherValue::Int(1));
    assert!(
        matches!(&map["nested"], CypherValue::List(items) if matches!(&items[0], CypherValue::Map(_)))
    );

    // Even a complete point and a v2 map envelope are plain parameter data.
    let point = serde_json::json!({"__type":"point","srid":4326,"x":1,"y":2});
    let envelope = serde_json::json!({"__type":"map","value":{"nested":point}});
    let CypherValue::Map(params) = try_json_to_param_value(&envelope).unwrap() else {
        panic!("expected parameter map")
    };
    assert_eq!(params["__type"], CypherValue::Str("map".into()));
    assert!(matches!(&params["value"], CypherValue::Map(value)
        if matches!(&value["nested"], CypherValue::Map(point)
            if point["__type"] == CypherValue::Str("point".into()))));
    assert!(matches!(
        try_json_to_cypher_value_v2(&envelope).unwrap(),
        CypherValue::Map(_)
    ));
    assert!(matches!(
        try_json_to_cypher_value_legacy(&envelope).unwrap(),
        CypherValue::Map(_)
    ));

    let malformed = serde_json::json!({"items":[{"__type":"point","x":1}]});
    assert!(try_json_to_param_value(&malformed).is_ok());
    assert_eq!(
        try_json_to_cypher_value_v2(&malformed).unwrap_err().path,
        "$.items[0].srid"
    );
    assert_eq!(
        try_json_to_cypher_value_legacy(&malformed)
            .unwrap_err()
            .path,
        "$.items[0].srid"
    );
}

#[test]
fn valid_geometry_and_vector_shapes_round_trip() {
    let geometries = [
        GeometryValue::LineString {
            coordinates: vec![[1.0, 2.0]],
            srid: 7203,
        },
        GeometryValue::Polygon {
            rings: vec![vec![[1.0, 2.0]]],
            srid: 7203,
        },
        GeometryValue::MultiPoint {
            points: vec![[1.0, 2.0]],
            srid: 7203,
        },
        GeometryValue::MultiLineString {
            lines: vec![vec![[1.0, 2.0]]],
            srid: 7203,
        },
        GeometryValue::MultiPolygon {
            polygons: vec![vec![vec![[1.0, 2.0]]]],
            srid: 7203,
        },
    ];
    for geometry in geometries {
        let value = CypherValue::Geometry(geometry);
        let json = cypher_value_to_json(&value).unwrap();
        assert_eq!(try_json_to_cypher_value_legacy(&json).unwrap(), value);
    }
    for (coord, item) in [
        (VectorCoordKind::Int8, CypherValue::Int(-128)),
        (VectorCoordKind::Int16, CypherValue::Int(32767)),
        (VectorCoordKind::Int32, CypherValue::Int(i32::MAX.into())),
        (VectorCoordKind::Int64, CypherValue::Int(i64::MAX)),
        (VectorCoordKind::Float32, CypherValue::Float(1.5)),
        (VectorCoordKind::Float64, CypherValue::Float(1.25)),
    ] {
        let value = CypherValue::Vector {
            coord,
            dim: 1,
            values: vec![item],
        };
        assert_eq!(
            try_json_to_cypher_value(&cypher_value_to_json(&value).unwrap()).unwrap(),
            value
        );
    }
}

#[test]
fn float32_exact_preserves_finite_bits_only() {
    for value in [0.0, -0.0, 1.5, f32::MIN_POSITIVE as f64] {
        assert!(float32_exact(value), "{value:?}");
        let encoded = cypher_value_to_json(&CypherValue::Vector {
            coord: VectorCoordKind::Float32,
            dim: 1,
            values: vec![CypherValue::Float(value)],
        })
        .unwrap();
        let CypherValue::Vector { values, .. } = try_json_to_cypher_value(&encoded).unwrap() else {
            panic!("expected vector")
        };
        let CypherValue::Float(decoded) = values[0] else {
            panic!("expected float")
        };
        assert_eq!(decoded.to_bits(), value.to_bits());
    }
    for value in [0.1, f64::MAX, f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
        assert!(!float32_exact(value), "{value:?}");
    }
    assert_eq!(
        try_json_to_cypher_value(&serde_json::json!({
            "__type":"vector", "coord":"FLOAT32", "dim":1, "values":[0.1]
        }))
        .unwrap_err()
        .path,
        "$.values[0]"
    );
}

#[test]
fn persistence_encoder_rejects_non_finite_and_graph_entities_at_nested_paths() {
    let bad = [
        (CypherValue::Float(f64::NAN), "$"),
        (
            CypherValue::Point(PointValue {
                srid: 4326,
                x: 1.0,
                y: f64::INFINITY,
                z: None,
            }),
            "$.y",
        ),
        (
            CypherValue::Point(PointValue {
                srid: 4979,
                x: 1.0,
                y: 2.0,
                z: Some(f64::NAN),
            }),
            "$.z",
        ),
        (
            CypherValue::Geometry(GeometryValue::MultiPolygon {
                polygons: vec![vec![vec![[1.0, f64::NEG_INFINITY]]]],
                srid: 7203,
            }),
            "$.coordinates[0][0][0][1]",
        ),
        (
            CypherValue::Geometry(GeometryValue::GeometryCollection {
                geometries: vec![GeometryValue::LineString {
                    coordinates: vec![[f64::NAN, 2.0]],
                    srid: 7203,
                }],
                srid: 7203,
            }),
            "$.geometries[0].coordinates[0][0]",
        ),
        (
            CypherValue::Node {
                id: 1,
                labels: vec![],
                properties: IndexMap::new(),
            },
            "$",
        ),
        (
            CypherValue::Relationship {
                id: 1,
                start_id: 1,
                end_id: 2,
                rel_type: "R".into(),
                properties: IndexMap::new(),
            },
            "$",
        ),
        (
            CypherValue::Path {
                nodes: vec![],
                relationships: vec![],
            },
            "$",
        ),
    ];
    for (value, path) in bad {
        assert_eq!(
            cypher_value_to_json(&value).unwrap_err().path,
            path,
            "{value:?}"
        );
        assert!(serde_json::to_value(&value).is_err(), "{value:?}");
        assert!(serde_json::to_string(&value).is_err(), "{value:?}");
    }
    let nested = CypherValue::Map(IndexMap::from([(
        "payload".into(),
        CypherValue::List(vec![CypherValue::Float(f64::NAN)]),
    )]));
    assert_eq!(
        cypher_value_to_json(&nested).unwrap_err().path,
        "$.payload[0]"
    );
    assert_eq!(
        props_to_json(&IndexMap::from([("bad".into(), nested)]))
            .unwrap_err()
            .path,
        "$.bad.payload[0]"
    );
}

#[test]
fn persistence_encoder_validates_vector_shape_variant_and_range() {
    let cases = [
        (VectorCoordKind::Int8, 2, vec![CypherValue::Int(1)], "$.dim"),
        (
            VectorCoordKind::Int8,
            1,
            vec![CypherValue::Int(128)],
            "$.values[0]",
        ),
        (
            VectorCoordKind::Int16,
            1,
            vec![CypherValue::Int(32768)],
            "$.values[0]",
        ),
        (
            VectorCoordKind::Int32,
            1,
            vec![CypherValue::Int(i64::from(i32::MAX) + 1)],
            "$.values[0]",
        ),
        (
            VectorCoordKind::Int64,
            1,
            vec![CypherValue::Float(1.0)],
            "$.values[0]",
        ),
        (
            VectorCoordKind::Float32,
            1,
            vec![CypherValue::Float(0.1)],
            "$.values[0]",
        ),
        (
            VectorCoordKind::Float32,
            1,
            vec![CypherValue::Float(f64::INFINITY)],
            "$.values[0]",
        ),
        (
            VectorCoordKind::Float64,
            1,
            vec![CypherValue::Int(1)],
            "$.values[0]",
        ),
        (
            VectorCoordKind::Float64,
            1,
            vec![CypherValue::Float(f64::NAN)],
            "$.values[0]",
        ),
    ];
    for (coord, dim, values, path) in cases {
        let value = CypherValue::Vector { coord, dim, values };
        assert_eq!(
            cypher_value_to_json(&value).unwrap_err().path,
            path,
            "{value:?}"
        );
    }
}

#[test]
fn canonical_spatial_vector_and_escaped_map_json_shapes() {
    let point = CypherValue::Point(PointValue::cartesian_3d(1.5, -2.25, 3.0));
    assert_eq!(
        cypher_value_to_json(&point).unwrap(),
        serde_json::json!({
            "__type": "point", "srid": 9157, "x": 1.5, "y": -2.25, "z": 3.0,
        })
    );
    let geometry = CypherValue::Geometry(GeometryValue::GeometryCollection {
        geometries: vec![GeometryValue::LineString {
            coordinates: vec![[1.5, -2.25]],
            srid: 7203,
        }],
        srid: 7203,
    });
    assert_eq!(
        cypher_value_to_json(&geometry).unwrap(),
        serde_json::json!({
            "__type": "geometry", "geometry_type": "GeometryCollection", "srid": 7203,
            "geometries": [{"__type": "geometry", "geometry_type": "LineString",
                "srid": 7203, "coordinates": [[1.5, -2.25]]}],
        })
    );
    let vector = CypherValue::Vector {
        coord: VectorCoordKind::Float32,
        dim: 2,
        values: vec![CypherValue::Float(-0.0), CypherValue::Float(1.5)],
    };
    let encoded = cypher_value_to_json(&vector).unwrap();
    assert_eq!(
        encoded,
        serde_json::json!({
            "__type": "vector", "coord": "FLOAT32", "dim": 2, "values": [-0.0, 1.5],
        })
    );
    assert_eq!(
        encoded["values"][0].as_f64().unwrap().to_bits(),
        (-0.0f64).to_bits()
    );

    let map = CypherValue::Map(IndexMap::from([
        ("__type".into(), CypherValue::Str("point".into())),
        ("nested".into(), geometry),
    ]));
    let encoded = cypher_value_to_json(&map).unwrap();
    assert_eq!(encoded["__type"], "map");
    assert_eq!(
        encoded["value"]["nested"]["geometry_type"],
        "GeometryCollection"
    );
    assert_eq!(try_json_to_cypher_value(&encoded).unwrap(), map);
}
