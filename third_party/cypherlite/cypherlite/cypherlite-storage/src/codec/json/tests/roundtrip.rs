use super::fixtures::graph_with_typed_and_user_maps;
use super::*;

fn assert_json_snapshot_roundtrip(store: &StoreTarget, reopened: &StoreTarget) {
    let graph = graph_with_typed_and_user_maps();
    crate::checkpoint(store, &graph, None).unwrap();
    let raw = crate::snapshot::get_object(store, GRAPH_JSON).unwrap();
    let data: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(data["value_encoding"], 2);
    assert_eq!(data["nodes"][0]["properties"]["point"]["__type"], "point");
    assert_eq!(data["nodes"][0]["properties"]["vector"]["__type"], "vector");
    assert_eq!(data["nodes"][0]["properties"]["vector"]["coord"], "FLOAT32");
    assert_eq!(
        data["nodes"][0]["properties"]["vector"]["values"],
        serde_json::json!([1.5, -0.0])
    );
    assert_eq!(
        data["nodes"][1]["properties"]["payload"][0]["__type"],
        "map"
    );
    assert_eq!(
        data["nodes"][1]["properties"]["payload"][0]["value"]["__type"],
        "point"
    );
    assert_eq!(
        data["relationships"][0]["properties"]["payload"]["__type"],
        "map"
    );
    assert_eq!(
        data["relationships"][0]["properties"]["payload"]["value"]["duration"]["__type"],
        "duration"
    );

    let (loaded, _wal) = crate::open_store(reopened).unwrap();
    for id in [1, 2] {
        assert_eq!(
            loaded.node(id).unwrap().properties,
            graph.node(id).unwrap().properties
        );
    }
    assert_eq!(
        loaded.rel(3).unwrap().properties,
        graph.rel(3).unwrap().properties
    );
}

#[test]
fn json_snapshot_roundtrip_native_vfs() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let store = crate::parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Json)).unwrap();
    let reopened = store.clone();
    assert_json_snapshot_roundtrip(&store, &reopened);
}

#[test]
fn json_snapshot_roundtrip_memory_vfs() {
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    assert_json_snapshot_roundtrip(&store, &store.clone());
}

fn snapshot(property: serde_json::Value, encoding: Option<u64>) -> GraphData {
    let mut json = serde_json::json!({
        "nodes": [{"id": 1, "labels": ["N"], "properties": {"p": property}}],
        "relationships": [], "next_node_id": 2, "next_rel_id": 1
    });
    if let Some(version) = encoding {
        json["value_encoding"] = version.into();
    }
    serde_json::from_value(json).unwrap()
}

#[test]
fn snapshot_header_selects_legacy_or_escaped_map_decoding() {
    let bytes = serde_json::json!({"__type":"map","value":{"x":1}});
    let legacy = graph_from_data(snapshot(bytes.clone(), None), 1, None)
        .unwrap()
        .0;
    let node = legacy.node_at(1, 1).unwrap();
    let Value::Map(map) = &node.properties["p"] else {
        panic!("expected map")
    };
    assert_eq!(map["__type"], Value::String("map".into()));
    assert_eq!(
        map["value"],
        Value::Map(std::collections::BTreeMap::from([("x".into(), Value::Int(1))]).into())
    );

    let v2 = graph_from_data(snapshot(bytes, Some(2)), 2, None)
        .unwrap()
        .0;
    let node = v2.node_at(1, 1).unwrap();
    let Value::Map(map) = &node.properties["p"] else {
        panic!("expected map")
    };
    assert_eq!(map.len(), 1);
    assert_eq!(map["x"], Value::Int(1));
    let empty = Graph::default();
    assert!(
        serde_json::to_value(graph_to_data(&empty, 0, empty.checkpoint_theater(), None).unwrap())
            .unwrap()["value_encoding"]
            == 2
    );
}

#[test]
fn snapshot_rejects_malformed_nested_tags_and_unknown_versions() {
    let bad = serde_json::json!({"nested":[{"__type":"point","srid":7203,"x":1}]});
    let err = graph_from_data(snapshot(bad.clone(), None), 1, None)
        .err()
        .unwrap()
        .to_string();
    assert!(
        err.contains("node 1: invalid graph property $.p.nested[0].y"),
        "{err}"
    );
    assert!(graph_from_data(snapshot(bad, Some(2)), 2, None).is_err());
    // The registry classification is the one value_encoding gate; an
    // unknown marker is unreadable and the load fails naming it.
    assert_eq!(
        crate::version::value_encoding::classify(Some(4), false),
        crate::version::Classification::Unreadable(4)
    );
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    crate::snapshot::put_object(
        &store,
        GRAPH_JSON,
        br#"{"value_encoding":4,"nodes":[],"relationships":[],"next_node_id":0,"next_rel_id":0}"#,
    )
    .unwrap();
    let err = load_with_checkpoint_lsn(&store).err().unwrap().to_string();
    assert!(err.contains("unsupported graph value encoding: 4"), "{err}");
    let mut invalid_header =
        serde_json::json!({"nodes":[],"relationships":[],"next_node_id":1,"next_rel_id":1});
    invalid_header["value_encoding"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<GraphData>(invalid_header).is_err());
}

const ONTOLOGY_DOC: &str = r#"{"version":1,"classes":[{"name":"Person"}]}"#;

#[test]
fn ontology_document_roundtrips_inline_at_value_encoding_3() {
    let store = crate::memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    let graph = graph_with_typed_and_user_maps();
    crate::checkpoint(&store, &graph, Some(ONTOLOGY_DOC)).unwrap();

    let raw = crate::snapshot::get_object(&store, GRAPH_JSON).unwrap();
    let data: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    // The format follows the content (ADR-0120 §8): the inline ontology
    // writes encoding 3.
    assert_eq!(data["value_encoding"], 3);
    assert_eq!(data["ontology"], ONTOLOGY_DOC);
    // The manifest.json envelope is unchanged (ADR-0120 §6).
    let envelope: serde_json::Value =
        serde_json::from_slice(&crate::snapshot::get_object(&store, "manifest.json").unwrap())
            .unwrap();
    assert!(envelope.get("ontology").is_none());
    assert_eq!(envelope["codec"], "json");

    let LoadedSnapshot {
        graph: loaded,
        ontology,
        marker,
        ..
    } = load_with_checkpoint_lsn(&store).unwrap();
    assert_eq!(ontology.as_deref(), Some(ONTOLOGY_DOC));
    assert_eq!(marker, crate::version::SnapshotMarker::Current);
    assert_eq!(loaded.live_node_count(), graph.live_node_count());

    // Without an ontology the same graph stays at encoding 2 with no field.
    let bare = crate::memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    crate::checkpoint(&bare, &graph, None).unwrap();
    let raw = crate::snapshot::get_object(&bare, GRAPH_JSON).unwrap();
    let data: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(data["value_encoding"], 2);
    assert!(data.get("ontology").is_none());
    let LoadedSnapshot {
        ontology, marker, ..
    } = load_with_checkpoint_lsn(&bare).unwrap();
    assert_eq!(ontology, None);
    assert_eq!(marker, crate::version::SnapshotMarker::Current);
}

/// Content-mismatched markers (ADR-0120 §8) are readable; migrate-on-open
/// on a durable store normalizes the marker to the content's version
/// without losing the document.
#[test]
fn content_mismatched_value_encoding_migrates_to_the_contents_version() {
    let rewrite = |target: &StoreTarget, edit: &mut dyn FnMut(&mut serde_json::Value)| {
        let mut value: serde_json::Value =
            serde_json::from_slice(&crate::snapshot::get_object(target, GRAPH_JSON).unwrap())
                .unwrap();
        edit(&mut value);
        crate::snapshot::put_object(target, GRAPH_JSON, &serde_json::to_vec(&value).unwrap())
            .unwrap();
    };
    let dir = tempfile::tempdir().unwrap();

    // Encoding 2 with an ontology document migrates to 3.
    let uri = dir.path().join("two-with-ontology");
    let target =
        crate::parse_uri_with_options(uri.to_str().unwrap(), None, Some(SnapshotCodecKind::Json))
            .unwrap();
    crate::checkpoint(&target, &Graph::default(), None).unwrap();
    rewrite(&target, &mut |v| {
        v["ontology"] = serde_json::Value::from(ONTOLOGY_DOC);
    });
    let (_, _, _, report) = crate::open_store_with_report(&target).unwrap();
    assert_eq!(
        report.migrations(),
        &[(crate::version::Subsystem::ValueEncoding, 2)]
    );
    let ontology = load_with_checkpoint_lsn(&target).unwrap().ontology;
    assert_eq!(ontology.as_deref(), Some(ONTOLOGY_DOC));
    let raw = crate::snapshot::get_object(&target, GRAPH_JSON).unwrap();
    let data: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(data["value_encoding"], 3);
    assert_eq!(data["ontology"], ONTOLOGY_DOC);

    // Encoding 3 without the document migrates to 2.
    let uri = dir.path().join("three-without-ontology");
    let target =
        crate::parse_uri_with_options(uri.to_str().unwrap(), None, Some(SnapshotCodecKind::Json))
            .unwrap();
    crate::checkpoint(&target, &Graph::default(), Some(ONTOLOGY_DOC)).unwrap();
    rewrite(&target, &mut |v| {
        v.as_object_mut().unwrap().remove("ontology");
    });
    let (_, _, _, report) = crate::open_store_with_report(&target).unwrap();
    assert_eq!(
        report.migrations(),
        &[(crate::version::Subsystem::ValueEncoding, 3)]
    );
    let ontology = load_with_checkpoint_lsn(&target).unwrap().ontology;
    assert_eq!(ontology, None);
    let raw = crate::snapshot::get_object(&target, GRAPH_JSON).unwrap();
    let data: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(data["value_encoding"], 2);
    assert!(data.get("ontology").is_none());
}
