use super::envelope::{compact_integer, page_header, Compact};
use super::fixtures::{commit_ops, load_part, write_bare_table, write_rel_table, write_table};
use super::*;
use arrow::array::{RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use bytes::Bytes;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use std::sync::Arc;

fn one_rel_parquet() -> (Graph, Vec<u8>, i64) {
    let g = Graph::default();
    g.begin_write().unwrap();
    g.create_node(vec!["A".into()], Default::default()).unwrap();
    g.create_node(vec!["B".into()], Default::default()).unwrap();
    let rid = g
        .create_rel(
            0,
            1,
            "KNOWS".into(),
            std::collections::BTreeMap::from([("since".into(), Value::Int(1843))]),
        )
        .unwrap();
    let ts = commit_ops(&g);
    let bytes = rels_parquet_ids(&g, ts, &[rid]).unwrap();
    (g, bytes, rid)
}

#[test]
fn load_rels_roundtrip() {
    let (g, bytes, rid) = one_rel_parquet();
    let loaded = Graph::default();
    load_part(load_rels, &loaded, &bytes).unwrap();
    let rel = loaded.rel_at(rid, 1).unwrap();
    assert_eq!(rel.rel_type, "KNOWS");
    assert_eq!(rel.properties["since"], Value::Int(1843));
    drop(g);
}

#[test]
fn node_and_relationship_shards_roundtrip() {
    let (g, rel_bytes, rid) = one_rel_parquet();
    let node_bytes = nodes_parquet_ids(&g, 1, &[0, 1]).unwrap();
    let loaded = Graph::default();
    load_part(load_nodes, &loaded, &node_bytes).unwrap();
    load_part(load_rels, &loaded, &rel_bytes).unwrap();
    assert_eq!(loaded.node_at(0, 1).unwrap().labels.as_slice(), ["A"]);
    assert_eq!(loaded.node_at(1, 1).unwrap().labels.as_slice(), ["B"]);
    assert_eq!(loaded.rel_at(rid, 1).unwrap().rel_type, "KNOWS");
}

#[test]
fn large_property_shard_omits_page_statistics() {
    let g = Graph::default();
    g.begin_write().unwrap();
    let content = "x".repeat(80 * 1024);
    g.create_node(
        vec!["File".into()],
        std::collections::BTreeMap::from([(
            "content".into(),
            Value::String(content.as_str().into()),
        )]),
    )
    .unwrap();
    let ts = commit_ops(&g);
    let bytes = nodes_parquet_ids(&g, ts, &[0]).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::copy_from_slice(&bytes)).unwrap();
    assert!(builder
        .metadata()
        .row_group(0)
        .column(2)
        .statistics()
        .is_none());
    let loaded = Graph::default();
    load_part(load_nodes, &loaded, &bytes).unwrap();
    assert_eq!(
        loaded.node_at(0, ts).unwrap().properties["content"],
        Value::String(content.into())
    );
}

#[test]
fn max_node_relationship_and_endpoint_ids_fail_before_graph_restore() {
    let node = |id| write_table(vec![id], vec![Some(vec![])], vec![Some("{}".into())]).unwrap();
    let rel = |id, start, end| {
        write_rel_table(
            vec![id],
            vec![start],
            vec![end],
            vec![Some("R".into())],
            vec![Some("{}".into())],
        )
        .unwrap()
    };
    // A part's range rule meets a row id of i64::MAX first: no exclusive
    // range end admits it. A monolith declares no range, so the persisted-id
    // check meets it; an endpoint is held to that check either way.
    let outside_parts = format!("id {} is outside the part range", i64::MAX);
    for (bytes, loader, in_part) in [
        (node(i64::MAX), load_nodes as Loader, outside_parts.as_str()),
        (rel(i64::MAX, 1, 2), load_rels as Loader, &outside_parts),
        (rel(1, i64::MAX, 2), load_rels as Loader, "i64::MAX"),
        (rel(1, 1, i64::MAX), load_rels as Loader, "i64::MAX"),
    ] {
        for (origin, expected) in [
            (Origin::Part(0..i64::MAX), in_part),
            (Origin::Monolith, "i64::MAX"),
        ] {
            let graph = Graph::default();
            let err = loader(&graph, bytes.clone(), origin).unwrap_err();
            assert!(matches!(err, Error::Storage(_)), "{err}");
            assert!(err.to_string().contains(expected), "{err}");
            assert_eq!(graph.next_node_id(), 0);
            assert_eq!(graph.next_rel_id(), 0);
        }
    }
    let graph = Graph::default();
    load_part(load_nodes, &graph, &node(i64::MAX - 1)).unwrap();
    assert_eq!(graph.next_node_id(), i64::MAX);
    load_part(
        load_rels,
        &graph,
        &rel(i64::MAX - 1, i64::MAX - 1, i64::MAX - 1),
    )
    .unwrap();
    assert_eq!(graph.next_rel_id(), i64::MAX);
}

#[test]
fn max_id_shards_fail_checkpoint_recovery_before_graph_install() {
    let node = write_table(vec![i64::MAX], vec![Some(vec![])], vec![Some("{}".into())]).unwrap();
    let rel = write_rel_table(
        vec![i64::MAX],
        vec![0],
        vec![0],
        vec![Some("R".into())],
        vec![Some("{}".into())],
    )
    .unwrap();
    for (dir, bytes, parts, other_parts) in [
        ("nodes", node, "node_parts", "rel_parts"),
        ("relationships", rel, "rel_parts", "node_parts"),
    ] {
        let target = crate::memory_store();
        let path = format!("{dir}/0-4096.parquet");
        target.vfs.create_dir_all(dir).unwrap();
        crate::snapshot::put_object(&target, &path, &bytes).unwrap();
        let part =
            serde_json::json!({"path":path,"id_lo":0,"id_hi":4096,"rows":1,"bytes":bytes.len()});
        let manifest = serde_json::json!({
            "format_version":3,"next_node_id":0,"next_rel_id":0,
            parts:[part],other_parts:[]
        });
        crate::snapshot::put_object(
            &target,
            "manifest.json",
            &serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let err = crate::open_store(&target).err().unwrap();
        assert!(matches!(err, Error::Storage(_)), "{err}");
        // The part's range rule meets the row before the persisted-id check.
        let expected = format!(
            "manifest part {path}: id {} is outside the part range [0, 4096) (ADR-0183)",
            i64::MAX
        );
        assert!(err.to_string().contains(&expected), "{err}");
        assert_eq!(crate::snapshot::get_object(&target, &path).unwrap(), bytes);
        assert_eq!(
            crate::snapshot::get_object(&target, "manifest.json").unwrap(),
            serde_json::to_vec(&manifest).unwrap()
        );
    }
}

fn split_into_two_row_groups(original: Vec<u8>) -> Vec<u8> {
    let mut reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(original))
        .unwrap()
        .build()
        .unwrap();
    let batch = reader.next().unwrap().unwrap();
    assert!((1..=2).contains(&batch.num_rows()));
    let mut bytes = Vec::new();
    {
        let props = WriterProperties::builder()
            .set_max_row_group_size(1)
            .build();
        let mut writer = ArrowWriter::try_new(&mut bytes, batch.schema(), Some(props)).unwrap();
        writer.write(&batch).unwrap();
        if batch.num_rows() == 1 {
            // A copy of the row under the next id keeps the ids ascending.
            let id = batch.column(0).as_any().downcast_ref::<Int64Array>();
            let mut columns = batch.columns().to_vec();
            columns[0] = Arc::new(Int64Array::from(vec![id.unwrap().value(0) + 1]));
            writer
                .write(&RecordBatch::try_new(batch.schema(), columns).unwrap())
                .unwrap();
        }
        writer.close().unwrap();
    }
    let meta = ParquetRecordBatchReaderBuilder::try_new(Bytes::copy_from_slice(&bytes)).unwrap();
    assert_eq!(meta.metadata().num_row_groups(), 2);
    bytes
}

fn forge_cumulative_row_count(mut bytes: Vec<u8>) -> Vec<u8> {
    // Keep the real pages, but forge each row group's compact-Thrift
    // count. Neither group exceeds the cap alone; their sum does.
    let footer = bytes.len() - 8;
    let len = u32::from_le_bytes(bytes[footer..footer + 4].try_into().unwrap()) as usize;
    let start = footer - len;
    let mut thrift = Compact::page(&bytes[start..footer]);
    let mut locations = Vec::new();
    thrift
        .fields(0, |c, id, ty, depth| {
            if id == 4 && ty == 9 {
                let head = c.byte()?;
                assert_eq!(head, 0x2c); // two row-group structs
                for _ in 0..2 {
                    c.fields(depth + 1, |c, id, ty, depth| {
                        if id == 3 && ty == 6 {
                            let begin = c.pos;
                            assert_eq!(c.integer()?, 1);
                            locations.push((begin, c.pos));
                        } else {
                            c.skip(ty, depth)?;
                        }
                        Ok(())
                    })?;
                }
            } else {
                c.skip(ty, depth)?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(locations.len(), 2);
    for ((begin, end), count) in locations.into_iter().zip([600_000, 400_001]).rev() {
        bytes.splice(start + begin..start + end, compact_integer(count));
    }
    let footer = bytes.len() - 8;
    bytes[footer..footer + 4].copy_from_slice(&((footer - start) as u32).to_le_bytes());
    bytes
}

#[test]
fn multiple_row_groups_share_one_recovery_budget() {
    let (g, rel_bytes, rid) = one_rel_parquet();
    let node_bytes = nodes_parquet_ids(&g, 1, &[0, 1]).unwrap();
    for (bytes, loader, is_node) in [
        (node_bytes, load_nodes as Loader, true),
        (rel_bytes, load_rels as Loader, false),
    ] {
        let bytes = split_into_two_row_groups(bytes);
        let loaded = Graph::default();
        load_part(loader, &loaded, &bytes).unwrap();
        if is_node {
            assert!(loaded.node_at(0, 1).is_some());
            assert!(loaded.node_at(1, 1).is_some());
        } else {
            assert!(loaded.rel_at(rid, 1).is_some());
        }

        let err = load_part(
            loader,
            &Graph::default(),
            &forge_cumulative_row_count(bytes),
        )
        .unwrap_err();
        assert!(err.to_string().contains("row count"), "{err}");
    }
}

#[test]
fn crafted_high_ratio_snappy_page_is_rejected_before_decode() {
    // Write one oversized value with a large page limit so ArrowWriter
    // emits an actual, internally consistent Snappy-compressed page.
    let schema = Arc::new(Schema::new(vec![Field::new(
        "properties",
        DataType::Utf8,
        false,
    )]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(StringArray::from(vec![
            "a".repeat(MAX_PAGE as usize)
        ]))],
    )
    .unwrap();
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .set_dictionary_enabled(false)
        .set_statistics_enabled(EnabledStatistics::None)
        .set_data_page_size_limit(MAX_PAGE as usize * 2)
        .build();
    let mut bytes = Vec::new();
    {
        let mut writer = ArrowWriter::try_new(&mut bytes, schema, Some(props)).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }
    drop(batch);
    assert!(
        bytes.len() < MAX_PAGE as usize / 16,
        "compressed shard is {} bytes",
        bytes.len()
    );
    let builder = ParquetRecordBatchReaderBuilder::try_new(Bytes::copy_from_slice(&bytes)).unwrap();
    let col = builder.metadata().row_group(0).column(0);
    assert_eq!(col.compression(), Compression::SNAPPY);
    let start = col.data_page_offset() as usize;
    let mut c = Compact::page(&bytes[start..start + MAX_HEADER]);
    let (mut raw, mut packed) = (None, None);
    c.fields(0, |c, id, ty, depth| {
        match (id, ty) {
            (2, 5) => raw = Some(c.integer()?),
            (3, 5) => packed = Some(c.integer()?),
            _ => c.skip(ty, depth)?,
        }
        Ok(())
    })
    .unwrap();
    let (raw, packed) = (raw.unwrap() as u64, packed.unwrap() as u64);
    assert!(raw > MAX_PAGE && packed < MAX_PAGE / 16);
    assert!(col.uncompressed_size() as u64 >= raw);
    assert!(start + c.pos + packed as usize <= bytes.len());
    for loader in [load_nodes as Loader, load_rels as Loader] {
        let err = load_part(loader, &Graph::default(), &bytes).unwrap_err();
        assert!(err.to_string().contains("page uncompressed size"), "{err}");
    }
}

#[test]
fn footer_metadata_and_actual_rows_have_independent_limits() {
    let (_, mut bytes, _) = one_rel_parquet();
    let end = bytes.len();
    bytes[end - 8..end - 4].copy_from_slice(&(MAX_METADATA as u32 + 1).to_le_bytes());
    let err = load_part(load_rels, &Graph::default(), &bytes).unwrap_err();
    assert!(err.to_string().contains("footer metadata"), "{err}");

    let mut decoded = Decoded::default();
    decoded.batch(600_000).unwrap();
    decoded.row(100).unwrap();
    decoded.batch(400_000).unwrap();
    assert!(decoded
        .batch(1)
        .unwrap_err()
        .to_string()
        .contains("actual decoded rows"));
    for _ in 0..5 {
        decoded.row(MAX_ROW_BYTES).unwrap();
    }
    decoded.row(MAX_DECODED - 100 - 5 * MAX_ROW_BYTES).unwrap();
    assert!(decoded
        .row(1)
        .unwrap_err()
        .to_string()
        .contains("actual decoded bytes"));
    assert!(Decoded::default().row(MAX_ROW_BYTES + 1).is_err());
}

#[test]
fn footer_thrift_list_bomb_is_rejected_before_parquet_allocates() {
    // Replace the schema list's count with u32::MAX while leaving the
    // actual footer tiny. Parquet's generated reader otherwise calls
    // Vec::with_capacity(count) before reading any SchemaElement.
    let (g, rel_bytes, _) = one_rel_parquet();
    let node_bytes = nodes_parquet_ids(&g, 1, &[0, 1]).unwrap();
    for (mut bytes, loader) in [
        (node_bytes, load_nodes as Loader),
        (rel_bytes, load_rels as Loader),
    ] {
        let footer = bytes.len() - 8;
        let len = u32::from_le_bytes(bytes[footer..footer + 4].try_into().unwrap()) as usize;
        let start = footer - len;
        let mut thrift = Compact::page(&bytes[start..footer]);
        let mut list = None;
        thrift
            .fields(0, |c, id, ty, depth| {
                if id == 2 && ty == 9 {
                    list = Some(c.pos);
                }
                c.skip(ty, depth)
            })
            .unwrap();
        let at = start + list.unwrap();
        assert_eq!(bytes[at] & 15, 12); // schema is a list of structs
        bytes.splice(at..at + 1, [0xfc, 0xff, 0xff, 0xff, 0xff, 0x0f]);
        let footer = bytes.len() - 8;
        bytes[footer..footer + 4].copy_from_slice(&((len + 5) as u32).to_le_bytes());
        assert!(bytes.len() < 4096);
        let err = load_part(loader, &Graph::default(), &bytes).unwrap_err();
        assert!(matches!(err, Error::Storage(_)), "{err}");
        assert!(
            err.to_string().contains("footer Thrift collection count"),
            "{err}"
        );
    }
}

#[test]
fn metadata_row_bomb_is_rejected_before_arrow_build() {
    let (_, mut bytes, _) = one_rel_parquet();
    let footer = bytes.len() - 8;
    let len = u32::from_le_bytes(bytes[footer..footer + 4].try_into().unwrap()) as usize;
    let start = footer - len;
    let mut thrift = Compact::page(&bytes[start..footer]);
    let mut row_pos = None;
    thrift
        .fields(0, |c, id, ty, depth| {
            if id == 3 && ty == 6 {
                row_pos = Some(c.pos);
                assert_eq!(c.integer()?, 1);
            } else {
                c.skip(ty, depth)?;
            }
            Ok(())
        })
        .unwrap();
    // Zigzag compact i64 1,000,001, replacing the single-byte row count.
    let at = start + row_pos.unwrap();
    bytes.splice(at..at + 1, [0x82, 0x89, 0x7a]);
    let footer = bytes.len() - 8;
    bytes[footer..footer + 4].copy_from_slice(&((len + 2) as u32).to_le_bytes());
    let err = load_part(load_rels, &Graph::default(), &bytes).unwrap_err();
    assert!(err.to_string().contains("file rows"), "{err}");
}

#[test]
fn isolated_page_and_row_limits_are_inclusive() {
    // A single page/row at its own cap is legal; one byte above is not,
    // independently of totals across columns and batches.
    let mut header = vec![0x15, 0, 0x15]; // DATA_PAGE; raw size follows
    let raw_offset = header.len();
    header.extend(compact_integer(MAX_PAGE as i64));
    header.extend([0x15, 0, 0x2c, 0x15, 0, 0, 0]); // packed=0, values=0
    assert_eq!(page_header(&header).unwrap().2, MAX_PAGE);
    header.splice(
        raw_offset..raw_offset + compact_integer(MAX_PAGE as i64).len(),
        compact_integer(MAX_PAGE as i64 + 1),
    );
    assert!(page_header(&header)
        .unwrap_err()
        .to_string()
        .contains("page uncompressed size"));

    let mut decoded = Decoded::default();
    decoded.row(MAX_ROW_BYTES).unwrap();
    assert!(Decoded::default()
        .row(MAX_ROW_BYTES + 1)
        .unwrap_err()
        .to_string()
        .contains("decoded row"));
    assert!(row_fits(MAX_ROW_BYTES) && !row_fits(MAX_ROW_BYTES + 1));
    // Under a decoded envelope smaller than the row envelope, a row over the
    // decoded envelope by itself misses that one.
    with_test_envelope(MAX_PAGE, 100, 50, || {
        assert!(row_fits(50) && !row_fits(51));
        let err = check_row(51).unwrap_err();
        assert!(err.to_string().contains("actual decoded bytes"), "{err}");
    });
}

#[test]
fn corrupt_rel_shard_is_an_error_not_a_panic() {
    let (_g, mut bytes, _rid) = one_rel_parquet();
    // Truncate mid-file: reader construction must fail with Err, not panic.
    bytes.truncate(bytes.len() / 2);
    let loaded = Graph::default();
    assert!(load_part(load_rels, &loaded, &bytes).is_err());
    // Garbage bytes also error.
    assert!(load_part(load_rels, &loaded, b"not parquet at all").is_err());
    // A node shard fed to the rel loader has the wrong columns: Err, not panic.
    let g2 = Graph::default();
    g2.begin_write().unwrap();
    g2.create_node(vec!["A".into()], Default::default())
        .unwrap();
    let ts = commit_ops(&g2);
    let node_bytes = nodes_parquet_ids(&g2, ts, &[0]).unwrap();
    assert!(load_part(load_rels, &loaded, &node_bytes).is_err());
}

#[test]
fn nested_test_overrides_restore_the_outer_value() {
    assert_eq!(file_ceiling(), MAX_FILE);
    with_test_file_ceiling(10, || {
        with_test_file_ceiling(20, || assert_eq!(file_ceiling(), 20));
        assert_eq!(file_ceiling(), 10);
        let inner = std::panic::catch_unwind(|| with_test_file_ceiling(30, || panic!("inner")));
        assert!(inner.is_err());
        assert_eq!(file_ceiling(), 10, "a panic restores the outer value too");
    });
    assert_eq!(file_ceiling(), MAX_FILE);
}

#[test]
fn page_envelope_rows_fill_max_pages_at_the_row_group_cap() {
    assert_eq!(Kind::Node.page_envelope_rows(), 699_008);
    assert_eq!(Kind::Rel.page_envelope_rows(), 419_392);
}

#[test]
fn small_rows_write_a_dictionary_and_a_data_page_per_leaf_per_group() {
    use parquet::file::reader::{FileReader, SerializedFileReader};

    let n = 2 * ROW_GROUP_CAP;
    let ids: Vec<i64> = (0..n as i64).collect();
    let text = |s: &str| vec![Some(s.to_owned()); n];
    for (kind, bytes) in [
        (
            Kind::Node,
            write_table(
                ids.clone(),
                vec![Some(vec![Some("L".into())]); n],
                text("{}"),
            )
            .unwrap(),
        ),
        (Kind::Node, write_bare_table(ids.clone()).unwrap()),
        (
            Kind::Rel,
            write_rel_table(ids.clone(), ids.clone(), ids.clone(), text("R"), text("{}")).unwrap(),
        ),
    ] {
        let reader = SerializedFileReader::new(Bytes::from(bytes)).unwrap();
        assert_eq!(reader.num_row_groups(), 2);
        for group in 0..reader.num_row_groups() {
            let group = reader.get_row_group(group).unwrap();
            assert_eq!(group.num_columns() as u64, kind.leaf_columns());
            for column in 0..group.num_columns() {
                let pages = group.get_column_page_reader(column).unwrap().count();
                assert_eq!(pages as u64, PAGES_PER_LEAF, "{kind:?} column {column}");
            }
        }
    }
}
