use super::super::*;
use super::harness::checkpoint_at;
use super::harness::*;
use crate::codec::parquet::with_test_file_ceiling;
use crate::{memory_store, open_store, Wal};
use cypherlite_executor::Graph;

#[test]
fn writer_splits_on_file_envelope_below_target_and_rejects_unsplittable_row() {
    let store = memory_store();
    let graph = Graph::default();
    graph.begin_write().unwrap();
    for i in 0..12 {
        graph
            .create_node(vec![format!("Label{i}")], Default::default())
            .unwrap();
    }
    let ts = apply_commit(&graph);
    let all =
        crate::codec::parquet::nodes_parquet_ids(&graph, ts, &graph.all_node_ids_at(ts)).unwrap();
    let one = crate::codec::parquet::nodes_parquet_ids(&graph, ts, &[0]).unwrap();
    assert!(all.len() > one.len());
    let ceiling = (all.len() + one.len()) as u64 / 2;
    with_test_file_ceiling(ceiling, || {
        let err = nodes_parquet_ids(&graph, ts, &graph.all_node_ids_at(ts)).unwrap_err();
        assert!(err.to_string().contains("file envelope"), "{err}");
        checkpoint_at(&store, &graph, 0, ts, MAX_FILE)
    })
    .unwrap();
    let manifest = manifest(&store);
    assert!(manifest.node_parts.len() > 1);
    assert!(manifest.node_parts.iter().all(|p| p.bytes <= ceiling));
    assert_eq!(load(&store).unwrap().live_node_count(), 12);

    let store = memory_store();
    let result = with_test_file_ceiling(one.len() as u64 - 1, || {
        checkpoint_at(&store, &graph, 0, ts, MAX_FILE)
    });
    assert!(result.unwrap_err().to_string().contains("file envelope"));
    assert!(!object_exists(&store, MANIFEST_JSON).unwrap());
}

#[test]
fn real_page_and_decoded_envelopes_split_into_reopenable_parts() {
    use crate::codec::parquet::with_test_envelope;

    // Encode the pair, which must miss `bound`, then split into two parts a
    // reopened store can read back. The envelope is the real preflight, not a
    // synthetic manifest or the test-only file-size ceiling.
    let split = |page, decoded, bound: &str, labels: [Vec<String>; 2]| {
        with_test_envelope(page, 10_000, decoded, || {
            let store = memory_store();
            let (vfs, prefix) = store.wal_vfs();
            let mut wal = Wal::open(vfs, prefix, 0).unwrap();
            let graph = Graph::default();
            graph.begin_write().unwrap();
            for label in &labels {
                graph
                    .create_node(label.clone(), Default::default())
                    .unwrap();
            }
            commit_open_write(&graph, &mut wal);
            let err = nodes_parquet_ids(&graph, graph.commit_ts(), &[0, 1]).unwrap_err();
            assert!(err.to_string().contains(bound), "{err}");
            checkpoint_at(&store, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
            let parts = manifest(&store).node_parts;
            assert_eq!(parts.len(), 2, "page={page} decoded={decoded}");
            let cut = wal.rotate().unwrap();
            wal.recycle(cut).unwrap();
            let (reopened, _) = open_store(&store).unwrap();
            assert_eq!(reopened.node(0).unwrap().labels.as_ref(), &labels[0]);
            assert_eq!(reopened.node(1).unwrap().labels.as_ref(), &labels[1]);
        });
    };

    // One 2_800-character label fits a 4 KiB page; the pair does not.
    split(
        4096,
        100_000,
        "page uncompressed size",
        ["a".repeat(2800), "b".repeat(2800)].map(|s| vec![s]),
    );
    // JSON preflight counts label characters (~3_200 for the pair). List
    // offsets make the row group ~7_500, and each node alone stays ~3_800,
    // so 5_500 rejects the pair and accepts each half. Page statistics used
    // to open this gap by copying min/max into the column chunk.
    let many = |prefix: char| (0..400).map(|i| format!("{prefix}{i:03}")).collect();
    split(128 << 20, 5_500, "row group bytes", [many('l'), many('m')]);
}

#[test]
fn late_numeric_collections_preflight_before_encoding_and_split_reopen() {
    use crate::codec::parquet::{take_encoded_rows, with_test_envelope};
    use cypherlite_core::Value;

    let rich = std::collections::BTreeMap::from([(
        "nested\"key".to_owned(),
        Value::Map(
            std::collections::BTreeMap::from([(
                "numbers".to_owned(),
                Value::List(
                    (0..45)
                        .map(|i| Value::Int(i64::MAX - i))
                        .collect::<Vec<_>>()
                        .into(),
                ),
            )])
            .into(),
        ),
    )]);
    let store = memory_store();
    let (vfs, prefix) = store.wal_vfs();
    let mut wal = Wal::open(vfs, prefix, 0).unwrap();
    let graph = Graph::default();
    graph.begin_write().unwrap();
    // The invariant under test: the first 64 light entries fit the default
    // budget and entry 65 onward carries the rich payload.
    let payload = |i: i64| {
        if i >= 65 {
            rich.clone()
        } else {
            Default::default()
        }
    };
    for i in 0..73 {
        graph.create_node(vec!["N".into()], payload(i)).unwrap();
    }
    for i in 0..72 {
        graph
            .create_rel(i, i + 1, "EDGE".into(), payload(i))
            .unwrap();
    }
    commit_open_write(&graph, &mut wal);
    let ts = graph.commit_ts();

    let nodes = graph.all_node_ids_at(ts);
    let rels = graph.all_rel_ids_at(ts);
    assert_eq!(nodes.len(), 73);
    assert_eq!(rels.len(), 72);
    with_test_envelope(128 << 20, 500, 3000, || {
        take_encoded_rows();
        for (ids, encode) in [
            (
                &nodes,
                nodes_parquet_ids as fn(&Graph, u64, &[i64]) -> Result<Vec<u8>>,
            ),
            (
                &rels,
                rels_parquet_ids as fn(&Graph, u64, &[i64]) -> Result<Vec<u8>>,
            ),
        ] {
            let err = encode(&graph, ts, ids).unwrap_err();
            assert!(err.to_string().contains("decoded row"), "{err}");
            assert_eq!(take_encoded_rows(), 0, "JSON/Arrow encoding started");
        }
    });

    // Each late row fits individually; the summed decoded size does not, so
    // the checkpoint cuts the window on the compact byte count.
    with_test_envelope(128 << 20, 4000, 8000, || {
        checkpoint_at(&store, &graph, wal.lsn, ts, MAX_FILE).unwrap();
        let manifest = manifest(&store);
        assert!(manifest.node_parts.len() > 1);
        assert!(manifest.rel_parts.len() > 1);
        let (reopened, _) = open_store(&store).unwrap();
        assert_eq!(reopened.node(70).unwrap().properties.as_ref(), &rich);
        assert_eq!(reopened.rel_at(70, ts).unwrap().properties.as_ref(), &rich);
    });
}

#[test]
fn shard_row_groups_stay_within_cap() {
    use crate::codec::parquet::ROW_GROUP_CAP;
    let store = memory_store();
    let graph = Graph::default();
    graph.begin_write().unwrap();
    for i in 0..130 {
        graph
            .create_node(vec![format!("N{i}")], Default::default())
            .unwrap();
    }
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, MAX_FILE).unwrap();
    let manifest = manifest(&store);
    assert_eq!(manifest.node_parts.len(), 1);
    let groups = row_group_rows(&store, &manifest.node_parts[0].path);
    assert!(groups.len() > 1);
    for rows in groups {
        assert!(rows > 0);
        assert!(rows <= ROW_GROUP_CAP as i64);
    }
    assert_eq!(open_store(&store).unwrap().0.live_node_count(), 130);
}

#[test]
fn single_row_over_target_reopens_and_hard_page_or_row_failure_keeps_wal() {
    use crate::codec::parquet::with_test_envelope;
    let check = |page: u64, row: u64, payload_len: usize, expected: &str| {
        with_test_envelope(page, row, 100_000, || {
            let store = memory_store();
            let (vfs, prefix) = store.wal_vfs();
            let mut wal = Wal::open(vfs, prefix, 0).unwrap();
            let graph = Graph::default();
            committed_nodes(&graph, &mut wal, ["A", "B"]);
            checkpoint_at(&store, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
            let old = get_object(&store, MANIFEST_JSON).unwrap();

            graph.begin_write().unwrap();
            graph.set_property_node(
                0,
                "big",
                cypherlite_core::Value::String("x".repeat(payload_len).into()),
            );
            commit_open_write(&graph, &mut wal);
            let lsn = wal.lsn;
            let err = checkpoint_at(&store, &graph, lsn, graph.commit_ts(), MAX_FILE).unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
            assert_eq!(get_object(&store, MANIFEST_JSON).unwrap(), old);
            assert_eq!(wal.lsn, lsn);
            assert_eq!(
                open_store(&store).unwrap().0.node(0).unwrap().properties["big"],
                cypherlite_core::Value::String("x".repeat(payload_len).into())
            );
        });
    };
    check(4096, 10_000, 5000, "page uncompressed size");
    check(
        128 << 20,
        1000,
        1100,
        "node 0: invalid or out-of-bounds 384 MiB decoded row",
    );

    let store = memory_store();
    let graph = Graph::default();
    graph.begin_write().unwrap();
    graph
        .create_node(vec!["A".into()], Default::default())
        .unwrap();
    let ts = apply_commit(&graph);
    checkpoint_at(&store, &graph, 0, ts, 1).unwrap();
    let part = &manifest(&store).node_parts[0];
    assert!(part.bytes > 1);
    assert_eq!(
        load(&store).unwrap().node(0).unwrap().labels.as_slice(),
        ["A"]
    );
}
