use super::*;
use std::sync::Arc;

// CIP-271: the toString() display form, pinned against Neo4j 2026.09.0
// probes (unquoted strings, sorted map keys, recursive rendering).
#[test]
fn to_display_string_matches_neo4j_2026_09() {
    let m = |pairs: &[(&str, Value)]| {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<BTreeMap<_, _>>()
                .into(),
        )
    };
    let cases: Vec<(Value, &str)> = vec![
        (
            m(&[
                ("a", Value::Int(1)),
                ("b", Value::String("x y".into())),
                ("c", Value::List(vec![Value::Int(1), Value::Int(2)].into())),
                ("d", m(&[("e", Value::Bool(true))])),
            ]),
            "{a: 1, b: x y, c: [1, 2], d: {e: true}}",
        ),
        // Sorted key order regardless of construction order.
        (
            m(&[("z", Value::Int(1)), ("a", Value::Int(2))]),
            "{a: 2, z: 1}",
        ),
        (Value::Map(BTreeMap::new().into()), "{}"),
        (Value::List(vec![].into()), "[]"),
        (
            Value::List(vec![Value::Int(1), Value::Null, Value::String("a".into())].into()),
            "[1, null, a]",
        ),
        (Value::Float(1.0), "1.0"),
        (Value::Float(1.5), "1.5"),
        (Value::Null, "null"),
        (Value::Bool(true), "true"),
        (
            Value::from_node(Node {
                id: 1,
                labels: Arc::new(vec!["X".into()]),
                properties: crate::PropertyMap::share(BTreeMap::new()),
            }),
            "(:X)",
        ),
        (
            Value::from_relationship(Relationship {
                id: 1,
                start: 1,
                end: 2,
                rel_type: "REL".into(),
                properties: crate::PropertyMap::share(BTreeMap::new()),
            }),
            "[:REL]",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(input.to_display_string(), expected, "input {input:?}");
    }
}

// ADR-0170: cloning a String, List, Map or Vector shares its payload.
#[test]
fn clone_shares_arc_payloads() {
    let s = Value::String("shared".into());
    let (Value::String(a), Value::String(b)) = (&s, &s.clone()) else {
        panic!("clone changed the variant");
    };
    assert!(Arc::ptr_eq(a, b));

    let list = Value::List(vec![Value::Int(1), Value::String("x".into())].into());
    let (Value::List(a), Value::List(b)) = (&list, &list.clone()) else {
        panic!("clone changed the variant");
    };
    assert!(Arc::ptr_eq(a, b));

    let map = Value::Map(BTreeMap::from([("k".to_string(), Value::Int(1))]).into());
    let (Value::Map(a), Value::Map(b)) = (&map, &map.clone()) else {
        panic!("clone changed the variant");
    };
    assert!(Arc::ptr_eq(a, b));

    let vector = Value::Vector {
        coord: VectorCoordKind::Float32,
        values: vec![Value::Float(1.0), Value::Float(2.0)].into(),
    };
    let (Value::Vector { values: a, .. }, Value::Vector { values: b, .. }) =
        (&vector, &vector.clone())
    else {
        panic!("clone changed the variant");
    };
    assert!(Arc::ptr_eq(a, b));
}

#[test]
fn int_float_eq() {
    assert!(Value::Int(1).eq_non_null(&Value::Float(1.0)));
    assert_eq!(Value::Int(1).cypher_eq(&Value::Null), Value::Null);
}

#[test]
fn int_float_eq_and_compare_are_exact_past_2_pow_53() {
    // 2^53 + 1 is an exact i64 but rounds to 2^53 as an f64; a lossy
    // `(a as f64) == b` cast (the old code) called these EQUAL.
    let big = Value::Int(9_007_199_254_740_993);
    let near = Value::Float(9_007_199_254_740_992.0);
    assert!(!big.eq_non_null(&near));
    assert!(!near.eq_non_null(&big));
    assert_eq!(big.cypher_eq(&near), Value::Bool(false));
    // Ordering agrees: the integer sits strictly above the float.
    assert_eq!(big.compare(&near), Some(Ordering::Greater));
    assert_eq!(near.compare(&big), Some(Ordering::Less));
    // The float DOES equal/compare-equal the integer it exactly denotes.
    let exact = Value::Int(9_007_199_254_740_992);
    assert!(exact.eq_non_null(&near));
    assert_eq!(exact.compare(&near), Some(Ordering::Equal));
    // Small values keep `1 = 1.0`, `0.0 = -0.0`.
    assert!(Value::Int(1).eq_non_null(&Value::Float(1.0)));
    assert!(Value::Int(0).eq_non_null(&Value::Float(-0.0)));
    assert_eq!(
        Value::Int(1).compare(&Value::Float(1.5)),
        Some(Ordering::Less)
    );
    assert_eq!(
        Value::Int(2).compare(&Value::Float(1.5)),
        Some(Ordering::Greater)
    );
    // i64::MAX is not 2^63 (which is out of the exact-integer range).
    assert!(!Value::Int(i64::MAX).eq_non_null(&Value::Float(9_223_372_036_854_775_808.0)));
    assert_eq!(
        Value::Int(i64::MAX).compare(&Value::Float(9_223_372_036_854_775_808.0)),
        Some(Ordering::Less)
    );
}

#[test]
fn vector_eq_is_elementwise_numeric() {
    // Same rule as the List arm and as the equivalence keys: vector
    // elements compare by mathematical value, so an Int element matches
    // the Float it denotes. A derived `av == bv` (the old code) was
    // variant-strict and disagreed with DISTINCT/GROUP BY.
    let int_backed = Value::Vector {
        coord: VectorCoordKind::Float64,
        values: vec![Value::Int(1)].into(),
    };
    let float_backed = Value::Vector {
        coord: VectorCoordKind::Float64,
        values: vec![Value::Float(1.0)].into(),
    };
    assert!(int_backed.eq_non_null(&float_backed));
    assert!(float_backed.eq_non_null(&int_backed));
    // ... including the exact large-magnitude rule elementwise.
    let big_int_backed = Value::Vector {
        coord: VectorCoordKind::Float64,
        values: vec![Value::Int(9_007_199_254_740_993)].into(),
    };
    let near_float_backed = Value::Vector {
        coord: VectorCoordKind::Float64,
        values: vec![Value::Float(9_007_199_254_740_992.0)].into(),
    };
    assert!(!big_int_backed.eq_non_null(&near_float_backed));
    // Different values and different coordinate kinds stay unequal.
    let two = Value::Vector {
        coord: VectorCoordKind::Float64,
        values: vec![Value::Float(2.0)].into(),
    };
    assert!(!int_backed.eq_non_null(&two));
    let other_kind = Value::Vector {
        coord: VectorCoordKind::Int32,
        values: vec![Value::Int(1)].into(),
    };
    assert!(!int_backed.eq_non_null(&other_kind));
}

#[test]
fn logical_bytes_string_header() {
    assert_eq!(Value::String("abc".into()).logical_bytes(), 19);
    assert_eq!(Value::Int(1).logical_bytes(), 8);
}

fn logical_bytes_cases() -> Vec<(Value, u64)> {
    let node = Node {
        id: 1,
        labels: Arc::new(vec!["L".into()]),
        properties: crate::PropertyMap::share(BTreeMap::from([(
            "name".into(),
            Value::String("x".into()),
        )])),
    };
    let rel = Relationship {
        id: 2,
        start: 1,
        end: 1,
        rel_type: "R".into(),
        properties: crate::PropertyMap::share(BTreeMap::from([("w".into(), Value::Float(0.5))])),
    };
    vec![
        (Value::Null, 1),
        (Value::Bool(true), 1),
        (Value::Int(1), 8),
        (Value::Float(1.0), 8),
        (Value::String("abc".into()), 19),
        (
            Value::List(vec![Value::Int(1), Value::String("ab".into())].into()),
            16 + 8 + 18,
        ),
        (
            Value::Map(BTreeMap::from([("k".into(), Value::Int(1))]).into()),
            16 + 17 + 8,
        ),
        (Value::from_node(node.clone()), 17 + 20 + 17),
        (Value::from_relationship(rel.clone()), 17 + 17 + 8),
        (
            Value::from_path(Path {
                nodes: vec![node.clone(), node],
                rels: vec![rel],
            }),
            54 + 54 + 42,
        ),
        (Value::Uuid(7), 16),
        (
            Value::Vector {
                coord: VectorCoordKind::Float64,
                values: vec![Value::Float(1.0), Value::Float(2.0)].into(),
            },
            16 + 16,
        ),
    ]
}

#[test]
fn logical_bytes_formula_is_pinned_per_variant() {
    for (value, expected) in logical_bytes_cases() {
        assert_eq!(value.logical_bytes(), expected, "{value:?}");
    }
}

// The budgeted walk shares the formula: it reports exceeded exactly when
// the summed `logical_bytes()` is over the limit (ADR-0341).
#[test]
fn logical_bytes_within_agrees_with_logical_bytes() {
    let cases = logical_bytes_cases();
    for (value, bytes) in &cases {
        assert_eq!(
            logical_bytes_within([value], *bytes),
            Some(*bytes),
            "{value:?}"
        );
        assert_eq!(logical_bytes_within([value], bytes - 1), None, "{value:?}");
    }
    let total: u64 = cases.iter().map(|(_, bytes)| bytes).sum();
    assert_eq!(
        logical_bytes_within(cases.iter().map(|(v, _)| v), total),
        Some(total)
    );
    assert_eq!(
        logical_bytes_within(cases.iter().map(|(v, _)| v), total - 1),
        None
    );
    assert_eq!(logical_bytes_within([], 0), Some(0));
}

// ADR-0311: the within-budget walk returns the exact sum while it fits and
// `None` once the total passes the limit.
#[test]
fn logical_bytes_within_returns_the_sum_under_the_limit() {
    let cases = logical_bytes_cases();
    let total: u64 = cases.iter().map(|(_, bytes)| bytes).sum();
    let values = || cases.iter().map(|(v, _)| v);
    assert_eq!(logical_bytes_within(values(), total), Some(total));
    assert_eq!(logical_bytes_within(values(), total - 1), None);
    assert_eq!(logical_bytes_within([], 0), Some(0));
}

// 40 rounds of self-sharing give 2^40 logical leaves in 41 allocations;
// an unbounded sum would take hours, the budgeted walk stops after about
// a mebibyte of leaves.
#[test]
fn logical_bytes_within_stops_once_over_budget() {
    let mut x = Value::Int(1);
    for _ in 0..40 {
        x = Value::List(vec![x.clone(), x].into());
    }
    let started = std::time::Instant::now();
    assert_eq!(logical_bytes_within([&x], 1 << 20), None);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "budgeted walk took {:?}",
        started.elapsed()
    );
}

#[test]
fn list_map_three_valued_eq() {
    assert_eq!(
        Value::List(vec![Value::Null].into()).cypher_eq(&Value::List(vec![Value::Int(1)].into())),
        Value::Null
    );
    assert_eq!(
        Value::List(vec![Value::Int(1)].into()).cypher_eq(&Value::List(vec![Value::Int(2)].into())),
        Value::Bool(false)
    );
    let mut a = BTreeMap::new();
    a.insert("k".into(), Value::Null);
    assert_eq!(
        Value::Map(a.clone().into()).cypher_eq(&Value::Map(a.into())),
        Value::Null
    );
}

#[test]
fn tagged_roundtrip() {
    let v = Value::List(vec![Value::Int(1), Value::String("a".into())].into());
    let j = v.to_json_tagged();
    assert_eq!(Value::from_json_tagged(&j).unwrap(), v);

    let point = Value::Point(PointValue::cartesian_3d(1.0, 2.0, 3.0));
    assert_eq!(
        Value::from_json_tagged(&point.to_json_tagged()).unwrap(),
        point
    );
    let vector = Value::Vector {
        coord: VectorCoordKind::Int32,
        values: vec![Value::Int(1), Value::Int(2)].into(),
    };
    assert_eq!(
        Value::from_json_tagged(&vector.to_json_tagged()).unwrap(),
        vector
    );
}

#[test]
fn order_by_type_rank() {
    assert_eq!(
        Value::List(vec![Value::Int(1), Value::Int(2)].into()).order_by_cmp(&Value::Int(1)),
        Ordering::Less
    );
    assert_eq!(
        Value::String("a".into()).order_by_cmp(&Value::Int(1)),
        Ordering::Less
    );
    assert_eq!(
        Value::List(vec![Value::Null, Value::Int(1)].into())
            .order_by_cmp(&Value::List(vec![Value::Null, Value::Int(2)].into())),
        Ordering::Less
    );
}

#[test]
fn uuid_compare_and_order() {
    // CIP-246: distinct UUIDs must order by numeric value, not collapse
    // to equal. These are the ids this regression was reported with.
    let a = Value::Uuid(0x0000_0000_0000_0000_0000_0000_0000_000a);
    let b = Value::Uuid(0x0000_0000_0000_0000_0000_0000_0000_000b);
    // Comparison operators (`<`, `>`): three-valued, so distinct UUIDs
    // were NULL before the fix.
    assert_eq!(a.compare(&b), Some(Ordering::Less));
    assert_eq!(b.compare(&a), Some(Ordering::Greater));
    assert_eq!(a.compare(&a), Some(Ordering::Equal));
    // ORDER BY uses order_by_cmp, which must not fall back to Equal.
    assert_eq!(a.order_by_cmp(&b), Ordering::Less);
    assert_eq!(b.order_by_cmp(&a), Ordering::Greater);
    // Rank: BOOLEAN < UUID < TEMPORAL.
    assert_eq!(Value::Bool(true).order_by_cmp(&a), Ordering::Less);
    assert_eq!(
        a.order_by_cmp(&Value::Temporal(Temporal::Date(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
        ))),
        Ordering::Less
    );
    // min()/max() are `.min_by`/`.max_by` over order_by_cmp (see
    // execute aggregate); sorting is the equivalent core-level check.
    let mut xs = vec![b.clone(), a.clone()];
    xs.sort_by(Value::order_by_cmp);
    assert_eq!(xs, vec![a.clone(), b.clone()]);
    assert_eq!(xs.iter().min_by(|x, y| x.order_by_cmp(y)).unwrap(), &a);
    assert_eq!(xs.iter().max_by(|x, y| x.order_by_cmp(y)).unwrap(), &b);
}

#[test]
fn uuid_parse_is_strict_and_case_insensitive() {
    // Canonical lowercase parses, and round-trips through the renderer.
    let id = "f066ecfc-5151-4121-8a36-b35e00d72918";
    let u = uuid_parse(id).unwrap();
    assert_eq!(uuid_hyphenated(u), id);
    // Uppercase hex is accepted and equals the lowercase parse.
    assert_eq!(uuid_parse(&id.to_ascii_uppercase()), Some(u));

    // Strictness: exactly 8-4-4-4-12 hex groups, four hyphens, nothing
    // before or after -- any deviation is rejected.
    for bad in [
        "",
        "f066ecfc-5151-4121-8a36-b35e00d7291", // too short (last group)
        "f066ecfc-5151-4121-8a36-b35e00d729188", // too long (last group)
        "f066ecfc5151-4121-8a36-b35e00d72918", // missing first hyphen
        "f066ecfc-51514121-8a36-b35e00d72918", // hyphen in wrong place
        "f066ecfc-5151-4121-8a36-b35e00d7291g", // non-hex digit
        "f066ecfc-5151-4121-8a36-b35e00d72918-", // trailing hyphen
        " f066ecfc-5151-4121-8a36-b35e00d72918", // leading space
    ] {
        assert_eq!(uuid_parse(bad), None, "uuid_parse accepted {bad:?}");
    }
}

#[test]
fn cypher_literal_string() {
    assert_eq!(
        Value::String("o'reilly".into()).to_cypher_literal(),
        "'o\\'reilly'"
    );
}

#[test]
fn from_json_tagged_rejects_malformed_payloads() {
    use serde_json::json;
    // Tag without a matching `v` payload must not silently default.
    assert!(Value::from_json_tagged(&json!({"t": "bool"})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "int"})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "int", "v": "nope"})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "float"})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "float", "v": "1.5"})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "string"})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "list"})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "list", "v": {}})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "map"})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "map", "v": []})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "point"})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "point", "v": {"srid": 4326}})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "vector", "coord": "float32"})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "vector", "v": []})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "date"})).is_err());
    assert!(Value::from_json_tagged(&json!({"t": "date", "v": 7})).is_err());
    // Unknown tags are errors, not Null.
    assert!(Value::from_json_tagged(&json!({"t": "bogus", "v": 1})).is_err());
    assert!(Value::from_json_tagged(&json!({"v": 1})).is_err());
    assert!(Value::from_json_tagged(&json!(7)).is_err());
    // Well-formed payloads still decode.
    assert_eq!(
        Value::from_json_tagged(&json!({"t": "int", "v": 7})).unwrap(),
        Value::Int(7)
    );
    assert!(matches!(
        Value::from_json_tagged(&json!({"t": "float", "v": "NaN"})).unwrap(),
        Value::Float(f) if f.is_nan()
    ));
    assert_eq!(
        Value::from_json_tagged(&json!({"t": "list", "v": []})).unwrap(),
        Value::List(vec![].into())
    );
    assert_eq!(
        Value::from_json_tagged(&json!({"t": "map", "v": {}})).unwrap(),
        Value::Map(Default::default())
    );
    assert!(matches!(
        Value::from_json_tagged(&json!({"t": "point", "v": {"srid": 4326, "x": 1.0, "y": 2.0}}))
            .unwrap(),
        Value::Point(_)
    ));
}

#[test]
fn from_json_scalars_keeps_type_tags_as_map_keys() {
    use serde_json::json;
    let wire = json!({"_type": "node", "id": 5, "nested": {"_type": "point", "x": 1}});
    let Value::Map(m) = Value::from_json_scalars(&wire) else {
        panic!("scalars decoder returned a non-map");
    };
    assert_eq!(m.get("_type"), Some(&Value::String("node".into())));
    assert_eq!(m.get("id"), Some(&Value::Int(5)));
    let Value::Map(nested) = m.get("nested").unwrap() else {
        panic!("nested object was not a map");
    };
    assert_eq!(nested.get("_type"), Some(&Value::String("point".into())));
    assert!(matches!(
        Value::from_json_plain(&json!({"_type": "node", "id": 5})),
        Value::Node(_)
    ));
}

// The tagged WAL/checkpoint encoder rejects graph entities instead of
// silently writing a null tag; `to_json_tagged` maps that internal
// error to Null.
#[test]
fn tagged_encoder_rejects_graph_entities() {
    let node = Value::from_node(Node {
        id: 1,
        labels: Arc::new(vec!["L".into()]),
        properties: crate::PropertyMap::share(BTreeMap::new()),
    });
    let rel = Value::from_relationship(Relationship {
        id: 2,
        start: 1,
        end: 1,
        rel_type: "R".into(),
        properties: crate::PropertyMap::share(BTreeMap::new()),
    });
    let path = Value::from_path(Path {
        nodes: vec![],
        rels: vec![],
    });
    for v in [&node, &rel, &path] {
        let err = serde_json::to_value(v).expect_err("entity must not encode");
        assert_eq!(
            err.to_string(),
            "graph entity is not a persistence property"
        );
        assert_eq!(v.to_json_tagged(), serde_json::Value::Null);
    }
    // Nested inside a property container, the error propagates.
    let nested = Value::List(vec![node].into());
    assert!(serde_json::to_value(&nested).is_err());
}

// The Path arm renders each element through the node/relationship
// helpers directly; the output shape is unchanged.
#[test]
fn to_json_plain_path_renders_elements_directly() {
    let node = Node {
        id: 1,
        labels: Arc::new(vec!["L".into()]),
        properties: crate::PropertyMap::share(BTreeMap::from([(
            "name".into(),
            Value::String("x".into()),
        )])),
    };
    let rel = Relationship {
        id: 2,
        start: 1,
        end: 1,
        rel_type: "R".into(),
        properties: crate::PropertyMap::share(BTreeMap::new()),
    };
    let path = Value::from_path(Path {
        nodes: vec![node],
        rels: vec![rel],
    });
    assert_eq!(
        path.to_json_plain(),
        serde_json::json!({
            "_type": "path",
            "nodes": [
                {"_type": "node", "id": 1, "labels": ["L"], "properties": {"name": "x"}}
            ],
            "relationships": [
                {"_type": "relationship", "id": 2, "start": 1, "end": 1, "type": "R",
                 "properties": {}}
            ],
        })
    );
}

// The single `_type` probe: unknown tags and a "vector" with an
// invalid coordinate type fall through to a plain map.
#[test]
fn from_json_plain_falls_through_to_map() {
    use serde_json::json;
    let Value::Map(m) = Value::from_json_plain(&json!({"_type": "bogus", "id": 1})) else {
        panic!("unknown _type did not fall through to a map");
    };
    assert_eq!(m.get("id"), Some(&Value::Int(1)));
    assert_eq!(m.get("_type"), Some(&Value::String("bogus".into())));
    let Value::Map(m) =
        Value::from_json_plain(&json!({"_type": "vector", "coordinateType": "bogus"}))
    else {
        panic!("invalid vector did not fall through to a map");
    };
    assert_eq!(m.get("_type"), Some(&Value::String("vector".into())));
}

// ADR-0327: the statement-scoped byte array. Byte-wise equality and
// ordering, its own ORDER BY rank right after LIST, the String logical-size
// formula, BINARY as the column type, and the LIST<INTEGER> crossing
// shared by the literal, plain-JSON and `value_to_cypher` renderings.
#[test]
fn bytes_value_semantics() {
    let bytes = |b: &[u8]| Value::Bytes(b.into());
    let ints = |b: &[i64]| Value::List(b.iter().map(|&x| Value::Int(x)).collect::<Vec<_>>().into());

    assert_eq!(
        bytes(&[0, 1, 255]).cypher_eq(&bytes(&[0, 1, 255])),
        Value::Bool(true)
    );
    assert_eq!(
        bytes(&[0, 1]).cypher_eq(&bytes(&[0, 2])),
        Value::Bool(false)
    );
    assert_eq!(bytes(&[0, 1]).cypher_eq(&ints(&[0, 1])), Value::Bool(false));
    assert_eq!(bytes(&[1]).cypher_eq(&Value::Null), Value::Null);

    assert_eq!(
        bytes(&[1, 2]).compare(&bytes(&[1, 3])),
        Some(Ordering::Less)
    );
    assert_eq!(
        bytes(&[1, 2]).compare(&bytes(&[1])),
        Some(Ordering::Greater)
    );
    assert_eq!(bytes(&[1]).compare(&ints(&[1])), None);
    // LIST < BYTES < PATH-ranked values < STRING.
    assert_eq!(ints(&[9]).order_by_cmp(&bytes(&[0])), Ordering::Less);
    assert_eq!(
        bytes(&[255]).order_by_cmp(&Value::String("a".into())),
        Ordering::Less
    );

    assert_eq!(bytes(&[1, 2, 3]).logical_bytes(), 16 + 3);
    assert_eq!(crate::column_type_name(&bytes(&[1])), "BINARY");
    assert_eq!(bytes(&[0, 1, 255]).to_cypher_literal(), "[0, 1, 255]");
    assert_eq!(bytes(&[]).to_display_string(), "[]");
    assert_eq!(
        bytes(&[0, 255]).to_json_plain(),
        serde_json::json!([0, 255])
    );
    assert_eq!(
        crate::value_to_cypher(&bytes(&[0, 255])),
        cypherlite_model::CypherValue::List(vec![
            cypherlite_model::CypherValue::Int(0),
            cypherlite_model::CypherValue::Int(255),
        ])
    );
}

// ADR-0327: the tagged WAL/checkpoint encoder has no byte-array tag; it
// rejects a byte array, also nested, instead of writing a null tag.
#[test]
fn tagged_encoder_rejects_byte_arrays() {
    let value = Value::Bytes(vec![1].into());
    let err = serde_json::to_value(&value).expect_err("bytes must not encode");
    assert_eq!(err.to_string(), "byte array is not a persistence property");
    assert!(serde_json::to_value(Value::List(vec![value].into())).is_err());
}
