//! ADR-0120 §5/§8 at the Parquet codec: a checkpoint that carries an
//! ontology document publishes it as an immutable
//! `ontology/<128-bit-hex>.json` object under the ADR-0092/ADR-0089 rules
//! (exclusive create, sync, durable name before the manifest; the
//! superseded object retired only after the new manifest publishes) and
//! writes `format_version` 4; a checkpoint without one stays at 3. Readers
//! accept 1..=4, and a crash between the object write and the manifest
//! publish leaves an orphan the next open sweeps.

use super::super::super::*;
use super::super::*;
use super::harness::*;
use crate::{memory_store, open_store, StoreTarget};
use cypherlite_executor::Graph;

const DOCUMENT: &str = r#"{"version":1,"classes":[{"name":"Person"}]}"#;

fn committed_graph() -> (Graph, crate::Wal) {
    let graph = Graph::default();
    let mut wal = crate::Wal::memory();
    commit_two_nodes(&graph, &mut wal);
    (graph, wal)
}

fn checkpoint(store: &StoreTarget, graph: &Graph, lsn: u64, ontology: Option<&str>) {
    checkpoint_at_with_bytes(store, graph, lsn, graph.commit_ts(), MAX_FILE, ontology).unwrap();
}

fn load_ontology(store: &StoreTarget) -> Option<String> {
    load_with_checkpoint_lsn(store).unwrap().ontology
}

fn ontology_objects(store: &StoreTarget) -> Vec<String> {
    let mut names = store
        .vfs
        .list_dir(ONTOLOGY_DIR)
        .unwrap_or_default()
        .into_iter()
        .map(|name| format!("{ONTOLOGY_DIR}/{name}"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn checkpoint_with_ontology_publishes_the_object_before_a_v4_manifest() {
    let store = memory_store();
    let (graph, wal) = committed_graph();
    checkpoint(&store, &graph, wal.lsn, Some(DOCUMENT));

    let manifest = manifest(&store);
    assert_eq!(manifest.format_version, 4);
    let path = manifest.ontology_path.expect("v4 selects the object");
    assert!(is_ontology_object_name(&path), "{path}");
    assert_eq!(get_object(&store, &path).unwrap(), DOCUMENT.as_bytes());
    assert_eq!(ontology_objects(&store), vec![path]);

    let LoadedSnapshot {
        graph: loaded,
        ontology,
        marker,
        ..
    } = load_with_checkpoint_lsn(&store).unwrap();
    assert_eq!(ontology.as_deref(), Some(DOCUMENT));
    assert_eq!(marker, crate::version::SnapshotMarker::Current);
    assert_eq!(graph_state(&loaded), graph_state(&graph));
    // The object counts as live snapshot bytes.
    let with_doc = snapshot_data_bytes(&store).unwrap();
    let bare = bytes::Bytes::from(get_object(&store, MANIFEST_JSON).unwrap());
    assert!(with_doc > bare.len() as u64 + DOCUMENT.len() as u64);
}

#[test]
fn checkpoint_without_ontology_stays_v3_and_writes_no_field() {
    let store = memory_store();
    let (graph, wal) = committed_graph();
    checkpoint(&store, &graph, wal.lsn, None);

    let manifest = manifest(&store);
    assert_eq!(manifest.format_version, 3);
    assert_eq!(manifest.ontology_path, None);
    let bytes = get_object(&store, MANIFEST_JSON).unwrap();
    assert!(!String::from_utf8(bytes).unwrap().contains("ontology_path"));
    assert!(!store.vfs.exists(ONTOLOGY_DIR).unwrap());
    assert_eq!(load_ontology(&store), None);
}

#[test]
fn unchanged_ontology_keeps_the_same_object_across_checkpoints() {
    let store = memory_store();
    let (graph, mut wal) = committed_graph();
    checkpoint(&store, &graph, wal.lsn, Some(DOCUMENT));
    let first = manifest(&store).ontology_path.unwrap();

    modify_two_nodes(&graph, &mut wal);
    checkpoint(&store, &graph, wal.lsn, Some(DOCUMENT));
    let manifest = manifest(&store);
    assert_eq!(manifest.format_version, 4);
    assert_eq!(manifest.ontology_path.as_deref(), Some(first.as_str()));
    assert_eq!(ontology_objects(&store), vec![first]);
}

#[test]
fn changed_ontology_publishes_a_new_object_and_retires_the_old_one() {
    let store = memory_store();
    let (graph, mut wal) = committed_graph();
    checkpoint(&store, &graph, wal.lsn, Some(DOCUMENT));
    let old = manifest(&store).ontology_path.unwrap();

    let updated = r#"{"version":2,"classes":[]}"#;
    modify_two_nodes(&graph, &mut wal);
    checkpoint(&store, &graph, wal.lsn, Some(updated));

    let manifest = manifest(&store);
    assert_eq!(manifest.format_version, 4);
    let new = manifest.ontology_path.unwrap();
    assert_ne!(new, old);
    assert_eq!(get_object(&store, &new).unwrap(), updated.as_bytes());
    // The old object is retired only after the new manifest published.
    assert_eq!(ontology_objects(&store), vec![new]);
    assert_eq!(load_ontology(&store).as_deref(), Some(updated));
}

#[test]
fn dropped_ontology_returns_the_manifest_to_v3_and_retires_the_object() {
    let store = memory_store();
    let (graph, mut wal) = committed_graph();
    checkpoint(&store, &graph, wal.lsn, Some(DOCUMENT));
    assert!(manifest(&store).ontology_path.is_some());

    modify_two_nodes(&graph, &mut wal);
    checkpoint(&store, &graph, wal.lsn, None);

    let manifest = manifest(&store);
    assert_eq!(manifest.format_version, 3);
    assert_eq!(manifest.ontology_path, None);
    assert_eq!(ontology_objects(&store), Vec::<String>::new());
    assert_eq!(load_ontology(&store), None);
}

#[test]
fn oversized_ontology_document_fails_before_any_object_is_written() {
    let store = memory_store();
    let (graph, wal) = committed_graph();
    let huge = "x".repeat(MAX_JSON_OBJECT as usize + 1);
    let before = get_object(&store, MANIFEST_JSON).unwrap_or_default();
    let err = checkpoint_at_with_bytes(
        &store,
        &graph,
        wal.lsn,
        graph.commit_ts(),
        MAX_FILE,
        Some(&huge),
    )
    .unwrap_err();
    assert!(err.to_string().contains("ontology document limit"), "{err}");
    assert!(!store.vfs.exists(ONTOLOGY_DIR).unwrap());
    // A failed publish leaves the manifest alone.
    assert_eq!(
        get_object(&store, MANIFEST_JSON).unwrap_or_default(),
        before
    );
}

#[test]
fn manifest_with_invalid_ontology_path_fails_open() {
    let store = memory_store();
    let (graph, wal) = committed_graph();
    checkpoint(&store, &graph, wal.lsn, Some(DOCUMENT));
    let base = manifest(&store);
    for bad in [
        "ontology/not-hex.json",
        "ontology/0123456789abcdef0123456789abcdef.JSON",
        "ontology/0123456789abcdef0123456789abcdef.json/extra",
        "nodes/0-1.parquet",
        "ontology/.json",
    ] {
        let mut forged = base.clone();
        forged.ontology_path = Some(bad.to_owned());
        put_manifest(&store, &forged);
        let err = open_store(&store).err().expect("invalid path must fail");
        assert!(
            err.to_string().contains("invalid manifest ontology path"),
            "{bad}: {err}"
        );
    }
}

#[test]
fn manifest_selecting_a_missing_ontology_object_fails_open() {
    let store = memory_store();
    let (graph, wal) = committed_graph();
    checkpoint(&store, &graph, wal.lsn, Some(DOCUMENT));
    let mut forged = manifest(&store);
    let missing = format!("{ONTOLOGY_DIR}/{}.json", "00".repeat(16));
    forged.ontology_path = Some(missing.clone());
    put_manifest(&store, &forged);
    let err = open_store(&store).err().expect("missing object must fail");
    assert!(err.to_string().contains("missing ontology/"), "{err}");
}

#[test]
fn crash_between_ontology_object_and_manifest_publish_is_swept_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let native = crate::parse_uri(dir.path().join("db").to_str().unwrap()).unwrap();
    let memory = memory_store();
    for store in [native, memory] {
        let (graph, wal) = committed_graph();
        checkpoint(&store, &graph, wal.lsn, None);
        let manifest_before = get_object(&store, MANIFEST_JSON).unwrap();

        // The crash window (ADR-0120 §5): the object is durable, the
        // manifest that would select it never published.
        let orphan = format!(
            "{ONTOLOGY_DIR}/{}.json",
            crate::vfs::random_hex_16().unwrap()
        );
        store.vfs.create_dir_all_for_atomic(ONTOLOGY_DIR).unwrap();
        put_object(&store, &orphan, DOCUMENT.as_bytes()).unwrap();

        // The store opens at the previous manifest and the sweep collects
        // the orphan. The native leg defers the sweep assertion on Windows:
        // `sweep_orphans` (like `retire_superseded`) returns early on a
        // Windows native directory (ADR-0089 §4 — Windows has no proven
        // power-loss namespace barrier for the renamed manifest,
        // ADR-0098, so a later power loss can restore an older manifest
        // and the unreferenced object is kept as its fallback; housekeeping
        // waits for a proven maintenance path). The memory leg has no
        // native root, so it asserts the sweep on every platform — it is
        // the Windows coverage for the crash-window semantics.
        let (opened, _wal) = open_store(&store).unwrap();
        assert_eq!(graph_state(&opened), graph_state(&graph));
        assert_eq!(
            get_object(&store, MANIFEST_JSON).unwrap(),
            manifest_before,
            "open stays at the previous manifest"
        );
        if !deferred_retire(&store) {
            assert!(!object_exists(&store, &orphan).unwrap(), "orphan swept");
        }
    }
}

/// ADR-0120 §7: a `.cl` bundle keeps the ontology object at a virtual
/// path; the container is unchanged and the same publication rules apply.
#[test]
fn bundle_store_roundtrips_and_retires_the_ontology_object() {
    let dir = tempfile::tempdir().unwrap();
    let store = crate::parse_uri(dir.path().join("graph.cl").to_str().unwrap()).unwrap();
    let (graph, mut wal) = committed_graph();
    checkpoint(&store, &graph, wal.lsn, Some(DOCUMENT));
    let first = manifest(&store).ontology_path.unwrap();

    let updated = r#"{"version":2,"classes":[]}"#;
    modify_two_nodes(&graph, &mut wal);
    checkpoint(&store, &graph, wal.lsn, Some(updated));

    let manifest = manifest(&store);
    assert_eq!(manifest.format_version, 4);
    assert_ne!(manifest.ontology_path.as_deref(), Some(first.as_str()));
    assert_eq!(load_ontology(&store).as_deref(), Some(updated));
    assert_eq!(
        ontology_objects(&store),
        vec![manifest.ontology_path.clone().unwrap()],
        "the retired object leaves the bundle catalog"
    );

    // And the bundle reopens onto the same document (drop the first handle
    // first: a bundle holds the host file's exclusive lock).
    drop(store);
    let reopened = crate::parse_uri(dir.path().join("graph.cl").to_str().unwrap()).unwrap();
    assert_eq!(load_ontology(&reopened).as_deref(), Some(updated));
}

#[test]
fn sweep_keeps_the_ontology_object_the_manifest_selects() {
    let store = memory_store();
    let (graph, wal) = committed_graph();
    checkpoint(&store, &graph, wal.lsn, Some(DOCUMENT));
    let path = manifest(&store).ontology_path.unwrap();
    sweep_orphans(&store).unwrap();
    assert!(object_exists(&store, &path).unwrap());
    // An unreferenced sibling goes, the selected object stays.
    let orphan = format!(
        "{ONTOLOGY_DIR}/{}.json",
        crate::vfs::random_hex_16().unwrap()
    );
    put_object(&store, &orphan, DOCUMENT.as_bytes()).unwrap();
    sweep_orphans(&store).unwrap();
    assert!(object_exists(&store, &path).unwrap());
    assert!(!object_exists(&store, &orphan).unwrap());
}
