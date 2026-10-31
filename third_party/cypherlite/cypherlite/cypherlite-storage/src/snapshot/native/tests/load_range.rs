//! A manifest part's rows lie in its `[id_lo, id_hi)`. Checkpoint coalescing
//! retires a part by that range (ADR-0183), so a row outside it fails the
//! open instead of loading. A row's properties cell holds a JSON object or
//! is null; other JSON fails the open instead of loading as no properties.

use super::super::*;
use super::harness::checkpoint_at;
use super::harness::*;
use crate::codec::parquet::{write_rel_table, write_table};
use crate::{memory_store, parse_uri, StoreTarget, Wal};
use cypherlite_executor::Graph;

/// Commit [`commit_two_nodes`] and [`commit_spread_rels`] to `store` and
/// checkpoint them, so each kind has one part holding rows 0 and 300_000.
/// Returns the graph and the WAL, which the caller keeps open.
fn checkpointed(store: &StoreTarget) -> (Graph, Wal) {
    let (vfs, prefix) = store.wal_vfs();
    let mut wal = Wal::open(vfs, prefix, 0).unwrap();
    let graph = Graph::default();
    commit_two_nodes(&graph, &mut wal);
    commit_spread_rels(&graph, &mut wal);
    checkpoint_at(store, &graph, wal.lsn, graph.commit_ts(), MAX_FILE).unwrap();
    (graph, wal)
}

#[test]
fn part_row_outside_its_range_fails_open() {
    let dir = tempfile::tempdir().unwrap();
    let native = dir.path().join("db");
    let bundle = dir.path().join("graph.cl");
    for store in [
        memory_store(),
        parse_uri(native.to_str().unwrap()).unwrap(),
        parse_uri(bundle.to_str().unwrap()).unwrap(),
    ] {
        let (graph, wal) = checkpointed(&store);
        let original = manifest(&store);
        for kind in Kind::ALL {
            let [part] = kind.parts(&original) else {
                panic!("expected one {kind:?} part");
            };
            assert_eq!((part.id_lo, part.rows), (0, 2), "{kind:?}");
            let bytes = get_object(&store, &part.path).unwrap();
            // Row 0 below the range, then row 300_000 at its exclusive end.
            // Both ranges still hold the part's two rows, so the forged
            // manifest passes read_manifest.
            for (lo, hi, outside) in [(1, part.id_hi + 1, 0), (0, 300_000, 300_000)] {
                let mut forged = original.clone();
                let moved = &mut kind.parts_mut(&mut forged)[0];
                (moved.id_lo, moved.id_hi) = (lo, hi);
                let (path, _) = forge_part(&store, &mut forged, kind, 0, &bytes, part.rows);
                read_manifest(&store).unwrap();
                let rule = format!("id {outside} is outside the part range [{lo}, {hi})");
                assert_fails_open(&store, &path, &rule);
            }
        }
        put_manifest(&store, &original);
        assert_reopens_as(&store, &graph);
        drop(wal);
    }
}

#[test]
fn non_object_properties_cell_fails_open() {
    let store = memory_store();
    let (graph, wal) = checkpointed(&store);
    let original = manifest(&store);
    let ids = vec![0, 300_000];
    // Each kind's part as the checkpoint wrote it, but for the properties
    // cells; none of the graph's rows has a property.
    let shards = |props: Vec<Option<String>>| {
        let labels = ["A", "B"].map(|l| Some(vec![Some(l.to_owned())])).to_vec();
        let types = vec![Some("LINK".to_owned()); 2];
        let ends = vec![300_000, 0];
        [
            (Kind::Node, write_table(ids.clone(), labels, props.clone())),
            (
                Kind::Rel,
                write_rel_table(ids.clone(), ids.clone(), ends, types, props),
            ),
        ]
    };
    for cell in ["[1, 2]", "7", "\"text\"", "null"] {
        for (kind, bytes) in shards(vec![Some(cell.to_owned()); 2]) {
            let mut forged = original.clone();
            let (path, _) = forge_part(&store, &mut forged, kind, 0, &bytes.unwrap(), 2);
            assert_fails_open(&store, &path, "property JSON must be an object");
        }
    }
    // A null cell loads as no properties. A successful open sweeps every file
    // the manifest does not list, so both kinds are forged into one manifest.
    let mut forged = original;
    for (kind, bytes) in shards(vec![None; 2]) {
        forge_part(&store, &mut forged, kind, 0, &bytes.unwrap(), 2);
        assert_reopens_as(&store, &graph);
    }
    drop(wal);
}
