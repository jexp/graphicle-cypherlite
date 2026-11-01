//! The vendored Odyssey vault e2e (ADR-0406 §7): 27 files transferred
//! from the lpgfs exploration as owned test data — `title:` keys
//! matching filenames 1:1 (the inference path), `name:` keys on
//! Character, full-form wikilinks under per-rel-type keys, bodies, a
//! root-level README (`:Vault` rule), and `index.md`/`log.md` at root
//! *and* inside label directories (reserved at any hierarchy level).
//! The test asserts load → checkpoint → reload stability and an
//! idempotent re-checkpoint.

mod vault_support;

use cypherlite_storage::{memory_store_with_codec, parse_uri_with_options, SnapshotCodecKind};
use vault_support::{copy_dir, copy_dir_into_vfs, fixture_dir, vault_signature};

#[test]
fn odyssey_vault_loads_checkpoints_and_reloads_stably() {
    // Copy the fixture into a temp dir so the checkpoint can write.
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    copy_dir(&fixture_dir(), &vault);

    let uri = vault.to_str().unwrap();
    let store = parse_uri_with_options(uri, None, Some(SnapshotCodecKind::Markdown)).unwrap();
    let (graph, _wal) = cypherlite_storage::open_store(&store).unwrap();

    // 20 concept files + the root README, all loaded as nodes.
    let node_count = graph.all_node_ids_at(u64::MAX).len();
    assert_eq!(node_count, 21, "20 concepts + the :Vault README");

    // Every rel-type link key became relationships; spot-check the hub.
    let rel_count = graph.all_rel_ids_at(u64::MAX).len();
    assert_eq!(rel_count, 30, "Odyssey topology loaded");

    // Checkpoint, reopen: same graph shape, plus the engine sidecar.
    cypherlite_storage::checkpoint(&store, &graph, None).unwrap();
    let manifest = SnapshotCodecKind::Markdown.checkpoint_image();
    assert!(store.vfs().exists(manifest).unwrap());
    let (reloaded, _wal) = cypherlite_storage::open_store(&store).unwrap();
    assert_eq!(
        reloaded.all_node_ids_at(u64::MAX).len(),
        node_count,
        "the stub-free stable vault round-trips"
    );
    assert_eq!(
        reloaded.all_rel_ids_at(u64::MAX).len(),
        rel_count,
        "relationship topology survives the checkpoint"
    );

    // Idempotent re-checkpoint: no guarded file changes.
    let before = vault_signature(&store);
    cypherlite_storage::checkpoint(&store, &reloaded, None).unwrap();
    assert_eq!(before, vault_signature(&store));
}

#[test]
fn odyssey_vault_roundtrip_on_memory_vfs_matches_native() {
    // The same fixture through a MemoryVfs store: the graph built from
    // the files must equal the native path's graph.
    let native = native_odyssey_counts();
    let memory = memory_store_with_codec(SnapshotCodecKind::Markdown).unwrap();
    let vfs = memory.vfs();
    copy_dir_into_vfs(&fixture_dir(), &*vfs);
    let (memory_graph, _) = cypherlite_storage::open_store(&memory).unwrap();
    assert_eq!(
        memory_graph.all_node_ids_at(u64::MAX).len(),
        native.nodes,
        "the MemoryVfs graph matches the native store's node count"
    );
    assert_eq!(
        memory_graph.all_rel_ids_at(u64::MAX).len(),
        native.rels,
        "the MemoryVfs graph matches the native store's relationship count"
    );
}

/// The native path's Odyssey graph shape, opened from a fresh fixture
/// copy: the reference the MemoryVfs roundtrip is compared against.
fn native_odyssey_counts() -> OdysseyCounts {
    let dir = tempfile::tempdir().unwrap();
    let vault = dir.path().join("vault");
    copy_dir(&fixture_dir(), &vault);
    let store = parse_uri_with_options(
        vault.to_str().unwrap(),
        None,
        Some(SnapshotCodecKind::Markdown),
    )
    .unwrap();
    let (graph, _) = cypherlite_storage::open_store(&store).unwrap();
    OdysseyCounts {
        nodes: graph.all_node_ids_at(u64::MAX).len(),
        rels: graph.all_rel_ids_at(u64::MAX).len(),
    }
}

struct OdysseyCounts {
    nodes: usize,
    rels: usize,
}
