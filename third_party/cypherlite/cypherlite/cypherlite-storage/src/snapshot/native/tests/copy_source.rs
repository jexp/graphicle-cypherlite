//! A bad copy source fails closed (ADR-0183). A part that breaks the shard
//! rules fails the open, naming the part. The copy cursor holds its source
//! to the same rules, and a checkpoint that meets a bad source, such as a
//! valid part missing a clean row, fails after one read of the shard, is not
//! halved, and leaves the published manifest.

use super::super::*;
use super::harness::checkpoint_at;
use super::harness::*;
use crate::codec::parquet::{write_bare_table, write_int_label_table, ROW_GROUP_CAP};
use crate::{memory_store, StoreTarget};
use cypherlite_executor::Graph;
use std::sync::atomic::Ordering;

/// Commit unlabeled nodes `ids` with no properties and checkpoint them.
fn checkpointed_nodes(store: &StoreTarget, ids: &[i64]) -> (Graph, u64) {
    let graph = Graph::default();
    graph.begin_write().unwrap();
    for &id in ids {
        graph.set_next_node_id(id);
        graph.create_node(Vec::new(), Default::default()).unwrap();
    }
    let ts = apply_commit(&graph);
    checkpoint_at(store, &graph, 0, ts, MAX_FILE).unwrap();
    (graph, ts)
}

/// Checkpoint `graph` with opens of `path` counted; the checkpoint must fail
/// with `expected`, naming `path`, read `path` once, and leave `manifest`
/// published.
fn assert_fails_once(
    store: &StoreTarget,
    graph: &Graph,
    ts: u64,
    path: &str,
    manifest: &[u8],
    expected: &str,
) {
    let (watched, opens) = watch_opens(store, path);
    let err = checkpoint_at(&watched, graph, 0, ts, MAX_FILE).unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err}");
    let named = format!("previous Parquet shard {path}: ");
    assert!(err.to_string().contains(&named), "{err}");
    assert!(err.to_string().contains(expected), "{err}");
    assert_eq!(opens.load(Ordering::SeqCst), 1, "the source was retried");
    assert_eq!(get_object(store, MANIFEST_JSON).unwrap(), manifest);
}

#[test]
fn missing_clean_row_fails_without_halving() {
    let store = memory_store();
    let ids: Vec<i64> = (0..10).chain(2_000_000..2_000_010).collect();
    let (graph, ts) = checkpointed_nodes(&store, &ids);
    let held: Vec<i64> = (2_000_000..2_000_010)
        .filter(|id| *id != 2_000_005)
        .collect();
    let bytes = nodes_parquet_ids(&graph, ts, &held).unwrap();
    let (path, published) = forge_part(&store, &mut manifest(&store), Kind::Node, 1, &bytes, 9);
    let files = || {
        let mut files = store.vfs.list_dir("nodes").unwrap();
        files.sort();
        files
    };
    let before = files();

    // The first unit writes its file before the second meets the missing
    // clean row beside its dirty neighbor; the failure removes that file.
    stage_dirty(&graph, 1, |id| id == 1 || id == 2_000_004);
    let ts = apply_commit(&graph);
    assert_fails_once(&store, &graph, ts, &path, &published, "row 2000005 missing");
    assert_eq!(files(), before);
}

#[test]
fn duplicate_or_descending_source_ids_fail_open() {
    for held in [vec![0, 0, 1], vec![1, 0]] {
        let store = memory_store();
        let (graph, _) = checkpointed_nodes(&store, &[0, 1]);
        let rows = held.len() as u64;
        let bytes = write_bare_table(held).unwrap();
        let (path, published) =
            forge_part(&store, &mut manifest(&store), Kind::Node, 0, &bytes, rows);
        assert_fails_open(&store, &path, "ids are not strictly ascending");

        // A checkpoint that still meets the part, from a graph that did not
        // open it, fails by the same rule.
        stage_dirty(&graph, 1, |id| id == 1);
        let ts = apply_commit(&graph);
        assert_fails_once(
            &store,
            &graph,
            ts,
            &path,
            &published,
            "ids are not strictly ascending",
        );
    }
}

#[test]
fn descending_ids_across_batch_boundary_fail() {
    // The cursor reads the part in `ROW_GROUP_CAP`-row batches: its first
    // batch is ascending, and its second starts below the first's last id.
    let cap = ROW_GROUP_CAP as i64;
    let store = memory_store();
    let ids: Vec<i64> = (0..cap + 6).collect();
    let (graph, _) = checkpointed_nodes(&store, &ids);
    let held: Vec<i64> = (0..cap).chain([cap - 2]).chain(cap..cap + 6).collect();
    let rows = held.len() as u64;
    let bytes = write_bare_table(held).unwrap();
    let (path, published) = forge_part(&store, &mut manifest(&store), Kind::Node, 0, &bytes, rows);
    assert_fails_open(&store, &path, "ids are not strictly ascending");

    // The last id is dirty, so the copy of every clean id before it
    // reads past the first batch.
    stage_dirty(&graph, 1, |id| id == cap + 5);
    let ts = apply_commit(&graph);
    assert_fails_once(
        &store,
        &graph,
        ts,
        &path,
        &published,
        "ids are not strictly ascending",
    );
}

#[test]
fn source_schema_mismatch_fails_open() {
    let store = memory_store();
    let (graph, _) = checkpointed_nodes(&store, &[0, 1]);
    let bytes = write_int_label_table(vec![0, 1]).unwrap();
    let (path, published) = forge_part(&store, &mut manifest(&store), Kind::Node, 0, &bytes, 2);
    assert_fails_open(&store, &path, "schema differs from the shard schema");

    stage_dirty(&graph, 1, |id| id == 1);
    let ts = apply_commit(&graph);
    assert_fails_once(
        &store,
        &graph,
        ts,
        &path,
        &published,
        "schema differs from the shard schema",
    );
}
