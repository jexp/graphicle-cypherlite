use super::*;

#[test]
fn sharded_checkpoint_roundtrips() {
    let store = memory_store();
    let (g, mut wal) = person_ada();
    checkpoint_with_lsn(&store, &g, wal.lsn, None).unwrap();
    let cut = wal.rotate().unwrap();
    wal.recycle(cut).unwrap();
    let loaded = load_with_wal(&store, &wal).unwrap();
    assert_eq!(loaded.live_node_count(), 1);
    let m = snapshot::native::read_manifest_for_test(&store)
        .unwrap()
        .unwrap();
    assert_eq!(m.format_version, 3);
    assert!(!m.node_parts.is_empty());
    assert!(m.node_parts[0].path.starts_with("nodes/"));
    assert!(snapshot_data_bytes(&store).unwrap() > 0);
}

#[test]
fn dirty_window_skips_unchanged_shard() {
    let store = memory_store();
    let g = Graph::default();
    g.begin_write().unwrap();
    g.set_next_node_id(0);
    g.create_node(vec!["A".into()], Default::default()).unwrap();
    g.set_next_node_id(300_000);
    g.create_node(vec!["B".into()], Default::default()).unwrap();
    let ops = g.take_redo();
    let ts = g.pending_commit_ts();
    g.abort_write();
    let mut wal = Wal::memory();
    wal.append_commit(ts, &ops).unwrap();
    g.apply_redo(ops.clone(), ts).unwrap();
    // At target 1 no two rows fit one rewrite unit (ADR-0183), so the rows
    // stay in separate parts and node 0's rewrite cannot absorb the other.
    snapshot::native::checkpoint_at_with_bytes(&store, &g, wal.lsn, ts, 1, None).unwrap();
    let m1 = snapshot::native::read_manifest_for_test(&store)
        .unwrap()
        .unwrap();
    assert_eq!(m1.node_parts.len(), 2);
    let high = m1
        .node_parts
        .iter()
        .find(|p| p.id_lo >= 262_144)
        .unwrap()
        .clone();
    let high_bytes = snapshot::get_object(&store, &high.path).unwrap();

    g.begin_write().unwrap();
    g.set_property_node(0, "x", cypherlite_core::Value::Int(1));
    let ops = g.take_redo();
    let ts = g.pending_commit_ts();
    g.abort_write();
    wal.append_commit(ts, &ops).unwrap();
    g.apply_redo(ops.clone(), ts).unwrap();
    snapshot::native::checkpoint_at_with_bytes(&store, &g, wal.lsn, ts, 1, None).unwrap();
    let high_bytes2 = snapshot::get_object(&store, &high.path).unwrap();
    assert_eq!(high_bytes, high_bytes2);
    let loaded = load(&store).unwrap();
    assert_eq!(
        loaded.node(0).unwrap().properties["x"],
        cypherlite_core::Value::Int(1)
    );
    assert_eq!(loaded.node(300_000).unwrap().labels.as_slice(), ["B"]);
}

#[test]
fn fat_rows_split_a_window() {
    let store = memory_store();
    let g = Graph::default();
    g.begin_write().unwrap();
    // Incompressible: a repeated character collapses under Snappy, and with
    // page statistics off (ADR-0284) that file no longer crosses the window.
    for i in 0..12 {
        let mut blob = String::new();
        let mut x = 0x9E3779B97F4A7C15u64.wrapping_add(i as u64);
        while blob.len() < 400 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
            blob.push((b'a' + (x % 26) as u8) as char);
        }
        g.create_node(
            vec!["Fat".into()],
            std::collections::BTreeMap::from([(
                "blob".into(),
                cypherlite_core::Value::String(blob.into()),
            )]),
        )
        .unwrap();
    }
    let ops = g.take_redo();
    let ts = g.pending_commit_ts();
    g.abort_write();
    let mut wal = Wal::memory();
    wal.append_commit(ts, &ops).unwrap();
    g.apply_redo(ops.clone(), ts).unwrap();
    snapshot::native::checkpoint_at_with_bytes(&store, &g, wal.lsn, ts, 1500, None).unwrap();
    let m = snapshot::native::read_manifest_for_test(&store)
        .unwrap()
        .unwrap();
    assert!(
        m.node_parts.len() >= 2,
        "expected a split, got {} parts",
        m.node_parts.len()
    );
    let loaded = load(&store).unwrap();
    assert_eq!(loaded.live_node_count(), 12);
}

#[test]
fn codec_mismatch_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().to_str().unwrap();
    let (g, mut wal) = person_ada();
    let target = parse_uri(uri).unwrap();
    checkpoint_with_lsn(&target, &g, wal.lsn, None).unwrap();
    wal.recycle(wal.lsn).unwrap();
    drop(target);
    let err = parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Json))
        .err()
        .expect("json flag vs parquet dir");
    assert!(err.to_string().contains("mismatch"), "{err}");
}

#[test]
fn old_manifest_without_codec_is_parquet() {
    let store = memory_store();
    let (g, wal) = person_ada();
    checkpoint_with_lsn(&store, &g, wal.lsn, None).unwrap();
    let m = snapshot::native::read_manifest_for_test(&store)
        .unwrap()
        .unwrap();
    assert_eq!(m.codec, SnapshotCodecKind::Parquet);
    assert_eq!(m.format_version, 3);
}
