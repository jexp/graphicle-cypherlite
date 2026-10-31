//! ADR-0127 migrate-on-open: `open_store` upgrades a supported-older store
//! before serving it (legacy-reader load, WAL replay, checkpoint at the
//! current format, WAL rotation — all on the one open flow); a second open
//! finds the current format and migrates nothing. `:memory:` never
//! migrates, and a corrupt or partial old store fails naming its detected
//! version.

use super::*;
use crate::version::Subsystem;

#[cfg(parquet_codec)]
fn commit_node(graph: &Graph, wal: &mut Wal, label: &str) -> u64 {
    graph.begin_write().unwrap();
    graph
        .create_node(vec![label.into()], Default::default())
        .unwrap();
    let ops = graph.take_redo();
    let ts = graph.pending_commit_ts();
    graph.abort_write();
    wal.append_commit(ts, &ops).unwrap();
    graph.apply_redo(ops, ts).unwrap();
    ts
}

/// A synthetic v1/v2-manifest store (the `snapshot::native::tests`
/// pattern): monolithic `nodes.parquet`/`relationships.parquet` holding
/// the first commit, a manifest at `version` checkpointed at LSN 1, and a
/// second commit in the WAL tail so migration must replay before it
/// checkpoints.
#[cfg(parquet_codec)]
fn legacy_monolith_store(target: &StoreTarget, version: u32) {
    let (wv, prefix) = target.wal_vfs();
    let mut wal = Wal::open(wv, prefix, 0).unwrap();
    let graph = Graph::default();
    let ts1 = commit_node(&graph, &mut wal, "A");
    assert_eq!(wal.lsn, 1);
    let nodes =
        crate::codec::parquet::nodes_parquet_ids(&graph, ts1, &graph.all_node_ids_at(ts1)).unwrap();
    snapshot::put_object(target, "nodes.parquet", &nodes).unwrap();
    let rels = crate::codec::parquet::rels_parquet_ids(&graph, ts1, &[]).unwrap();
    snapshot::put_object(target, "relationships.parquet", &rels).unwrap();
    let manifest = bare_manifest(
        version,
        graph.next_node_id(),
        graph.next_rel_id(),
        wal.lsn,
        ts1,
    );
    snapshot::put_object(
        target,
        "manifest.json",
        &serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    commit_node(&graph, &mut wal, "B");
    assert_eq!(wal.lsn, 2);
}

#[cfg(parquet_codec)]
fn on_disk_manifest(target: &StoreTarget) -> Manifest {
    serde_json::from_slice(&snapshot::get_object(target, "manifest.json").unwrap()).unwrap()
}

/// Migrate-on-open matrix over Native and Bundle targets (the crash
/// test's loop pattern): synthetic v1/v2-manifest monolith Parquet stores
/// migrate on open, serve their rows, and leave current-format markers.
#[cfg(parquet_codec)]
#[test]
fn open_migrates_v1_and_v2_manifest_stores_then_takes_zero_hops() {
    let dir = tempfile::tempdir().unwrap();
    for version in [1, 2] {
        for uri in [
            dir.path().join(format!("v{version}")),
            dir.path().join(format!("v{version}.cl")),
        ] {
            let target = parse_uri(uri.to_str().unwrap()).unwrap();
            legacy_monolith_store(&target, version);

            let (graph, wal, _, report) = open_store_with_report(&target).unwrap();
            assert_eq!(
                report.migrations(),
                &[(Subsystem::Manifest, u64::from(version))],
                "v{version} migration report at {uri:?}"
            );
            assert_eq!(
                graph.live_node_count(),
                2,
                "checkpoint row plus replayed WAL tail"
            );
            // The store now carries a current-format manifest.
            assert_eq!(
                on_disk_manifest(&target).format_version,
                crate::version::manifest::CURRENT_VERSION
            );
            // The WAL rotated to a single fresh segment with the current magic.
            {
                let (wv, prefix) = target.wal_vfs();
                let segments = wal_dir_state(&*wv, prefix);
                assert_eq!(segments.len(), 1, "{segments:?}");
                assert_eq!(segments[0].0, "00000002.log");
                assert!(segments[0].1.starts_with(MAGIC), "current segment magic");
            }
            drop((graph, wal, target));

            // Second open: current format detected, zero hops.
            let target = parse_uri(uri.to_str().unwrap()).unwrap();
            let (graph, _wal, _, report) = open_store_with_report(&target).unwrap();
            assert!(report.is_empty(), "second open must not migrate: {report}");
            assert_eq!(graph.live_node_count(), 2);
            assert_eq!(
                on_disk_manifest(&target).format_version,
                crate::version::manifest::CURRENT_VERSION
            );
        }
    }
}

#[cfg(parquet_codec)]
#[test]
fn memory_open_performs_no_migration_writes() {
    for version in [1, 2] {
        let store = memory_store();
        legacy_monolith_store(&store, version);
        let before = snapshot::get_object(&store, "manifest.json").unwrap();

        let (graph, _wal, _, report) = open_store_with_report(&store).unwrap();
        // The legacy reader still serves the old store...
        assert_eq!(graph.live_node_count(), 2, "v{version}");
        // ...but :memory: never migrates (ADR-0127 Decision 7): no hops,
        // the planted manifest is byte-identical, and no v3 shard dirs
        // appeared.
        assert!(report.is_empty(), ":memory: must not migrate: {report}");
        assert_eq!(
            snapshot::get_object(&store, "manifest.json").unwrap(),
            before,
            "v{version} manifest must stay byte-identical"
        );
        assert!(!store
            .vfs()
            .list_dir("")
            .unwrap()
            .iter()
            .any(|n| n == "nodes"));
    }
}

#[cfg(parquet_codec)]
#[test]
fn corrupt_legacy_store_error_names_the_detected_version() {
    let dir = tempfile::tempdir().unwrap();
    let target = parse_uri(dir.path().join("db").to_str().unwrap()).unwrap();
    snapshot::put_object(&target, "nodes.parquet", b"not parquet").unwrap();
    let manifest = bare_manifest(2, 0, 0, 0, 0);
    snapshot::put_object(
        &target,
        "manifest.json",
        &serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let err = open_store(&target).err().expect("corrupt store must fail");
    assert!(matches!(err, Error::Storage(_)), "{err}");
    // The failure names the monolith object, then the detected version.
    assert_eq!(
        err.to_string(),
        "storage error: legacy shard nodes.parquet: invalid or out-of-bounds \
         footer magic (ADR-0086) (store snapshot is at manifest version 2)"
    );
}

/// Value-encoding matrix over Native and Bundle targets: a legacy
/// (absent-`value_encoding`-marker, encoding 1 — ADR-0127 Decision 1) JSON
/// snapshot migrates 1 → 2 on open, then a second open takes zero hops.
#[test]
fn open_migrates_legacy_json_value_encoding_then_takes_zero_hops() {
    let dir = tempfile::tempdir().unwrap();
    for uri in [dir.path().join("jsondb"), dir.path().join("json.cl")] {
        let target =
            parse_uri_with_options(uri.to_str().unwrap(), None, Some(SnapshotCodecKind::Json))
                .unwrap();
        snapshot::put_object(&target, "graph.json", LEGACY_JSON_GRAPH.as_bytes()).unwrap();

        let (graph, wal, _, report) = open_store_with_report(&target).unwrap();
        assert_eq!(report.migrations(), &[(Subsystem::ValueEncoding, 1)]);
        assert_eq!(graph.live_node_count(), 1);
        assert_eq!(
            graph.node(1).unwrap().properties["name"],
            cypherlite_core::Value::String("Ada".into())
        );
        let rewritten: serde_json::Value =
            serde_json::from_slice(&snapshot::get_object(&target, "graph.json").unwrap()).unwrap();
        assert_eq!(
            rewritten["value_encoding"],
            crate::version::value_encoding::CURRENT_VERSION
        );
        drop((graph, wal, target));

        // Second open: current format detected, zero hops.
        let target =
            parse_uri_with_options(uri.to_str().unwrap(), None, Some(SnapshotCodecKind::Json))
                .unwrap();
        let (graph, _wal, _, report) = open_store_with_report(&target).unwrap();
        assert!(report.is_empty(), "second open must not migrate: {report}");
        assert_eq!(graph.live_node_count(), 1);
    }
}

/// Memory leg of the value_encoding matrix: the legacy reader serves the
/// old JSON store, but :memory: never migrates (ADR-0127 Decision 7).
#[test]
fn memory_json_open_performs_no_migration_writes() {
    let store = memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    snapshot::put_object(&store, "graph.json", LEGACY_JSON_GRAPH.as_bytes()).unwrap();
    let before = snapshot::get_object(&store, "graph.json").unwrap();

    let (graph, _wal, _, report) = open_store_with_report(&store).unwrap();
    assert_eq!(graph.live_node_count(), 1);
    assert!(report.is_empty(), ":memory: must not migrate: {report}");
    assert_eq!(
        snapshot::get_object(&store, "graph.json").unwrap(),
        before,
        "graph.json must stay byte-identical"
    );
}

/// Crash mid-migration (ADR-0127 Decision 4): checkpoint publication is
/// manifest-last (ADR-0089/ADR-0092), so a crash between the payload write
/// and the manifest publish leaves the old manifest in place next to the
/// new shard payloads. The next open still detects the old version and
/// re-migrates from scratch to identical contents.
#[cfg(parquet_codec)]
#[test]
fn crash_between_payload_write_and_manifest_publish_remigrates_identically() {
    let dir = tempfile::tempdir().unwrap();
    for uri in [dir.path().join("native"), dir.path().join("bundle.cl")] {
        let target = parse_uri(uri.to_str().unwrap()).unwrap();
        legacy_monolith_store(&target, 2);
        let old_manifest = snapshot::get_object(&target, "manifest.json").unwrap();
        let old_nodes = snapshot::get_object(&target, "nodes.parquet").unwrap();
        let old_rels = snapshot::get_object(&target, "relationships.parquet").unwrap();

        // Simulate the crash by hand: run the migration's data movement
        // (legacy load, WAL replay, current-format checkpoint payloads),
        // then restore the old manifest and the old payloads, as if the
        // process died after the new shard payloads landed but before the
        // manifest publish (retirement of the old payloads is post-publish,
        // so a real crash here leaves them intact).
        let (wv, prefix) = target.wal_vfs();
        let wal = Wal::open(wv, prefix, 0).unwrap();
        let graph = load_with_wal(&target, &wal).unwrap();
        assert_eq!(graph.live_node_count(), 2);
        checkpoint_with_lsn(&target, &graph, wal.lsn, None).unwrap();
        snapshot::put_object(&target, "nodes.parquet", &old_nodes).unwrap();
        snapshot::put_object(&target, "relationships.parquet", &old_rels).unwrap();
        snapshot::put_object(&target, "manifest.json", &old_manifest).unwrap();
        assert_eq!(
            on_disk_manifest(&target).format_version,
            2,
            "crash leaves the old manifest published"
        );
        drop((wal, graph, target));

        // Reopen: detection still sees manifest version 2 and re-migrates;
        // the crashed attempt's orphan payloads are swept and the contents
        // are identical to a clean migration.
        let target = parse_uri(uri.to_str().unwrap()).unwrap();
        let (graph, wal, _, report) = open_store_with_report(&target).unwrap();
        assert_eq!(report.migrations(), &[(Subsystem::Manifest, 2)]);
        assert_eq!(graph.live_node_count(), 2, "checkpoint row plus WAL tail");
        assert!(graph.node(0).is_some() && graph.node(1).is_some());
        assert_eq!(
            on_disk_manifest(&target).format_version,
            crate::version::manifest::CURRENT_VERSION
        );
        {
            let (wv, prefix) = target.wal_vfs();
            let segments = wal_dir_state(&*wv, prefix);
            assert_eq!(segments.len(), 1, "{segments:?}");
            assert!(segments[0].1.starts_with(MAGIC), "current segment magic");
        }
        drop((graph, wal, target));

        // The re-migrated store is current: a further open takes zero hops.
        let target = parse_uri(uri.to_str().unwrap()).unwrap();
        let (graph, _wal, _, report) = open_store_with_report(&target).unwrap();
        assert!(
            report.is_empty(),
            "post-crash re-open must not migrate: {report}"
        );
        assert_eq!(graph.live_node_count(), 2);
    }
}

/// The JSON codec leg of the corrupt-store contract: a legacy (absent
/// `value_encoding`) `graph.json` whose body fails to decode fails open
/// naming its detected version (ADR-0127 Decision 7).
#[test]
fn corrupt_legacy_json_store_error_names_the_detected_version() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("jsondb");
    std::fs::create_dir(&uri).unwrap();
    // Valid envelope for detection (absent marker = encoding 1), corrupt
    // body for the legacy reader.
    std::fs::write(
        uri.join("graph.json"),
        r#"{"nodes":"corrupt","relationships":[],"next_node_id":0,"next_rel_id":1}"#,
    )
    .unwrap();
    let target =
        parse_uri_with_options(uri.to_str().unwrap(), None, Some(SnapshotCodecKind::Json)).unwrap();

    let err = open_store(&target).err().expect("corrupt store must fail");
    assert!(matches!(err, Error::Storage(_)), "{err}");
    assert!(
        err.to_string().contains("value_encoding 1"),
        "detected version must be named: {err}"
    );
    assert_eq!(
        err.to_string().matches("storage error:").count(),
        1,
        "no doubled prefix: {err}"
    );
}

/// Read-counting VFS (the `snapshot::native::tests` `InjectVfs` pattern):
/// counts read-only opens per path to prove detection costs no extra I/O.
struct ReadCountingVfs {
    inner: Arc<dyn Vfs>,
    opens: std::sync::Mutex<std::collections::BTreeMap<String, usize>>,
}

impl ReadCountingVfs {
    fn new(inner: Arc<dyn Vfs>) -> Self {
        Self {
            inner,
            opens: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    fn read_opens(&self, path: &str) -> usize {
        self.opens.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

impl Vfs for ReadCountingVfs {
    fn durable(&self) -> bool {
        self.inner.durable()
    }
    fn create_dir_all(&self, path: &str) -> Result<()> {
        self.inner.create_dir_all(path)
    }
    fn create_dir_all_for_atomic(&self, path: &str) -> Result<()> {
        self.inner.create_dir_all_for_atomic(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        self.inner.list_dir(path)
    }
    fn exists(&self, path: &str) -> Result<bool> {
        self.inner.exists(path)
    }
    fn metadata(&self, path: &str) -> Result<VfsMeta> {
        self.inner.metadata(path)
    }
    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>> {
        if opts.read && !opts.write && !opts.append && !opts.create && !opts.truncate {
            *self
                .opens
                .lock()
                .unwrap()
                .entry(path.to_string())
                .or_insert(0) += 1;
        }
        self.inner.open(path, opts)
    }
    fn remove_file(&self, path: &str) -> Result<()> {
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &str) -> Result<()> {
        self.inner.remove_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.inner.rename(from, to)
    }
    fn sync_dir(&self, path: &str) -> Result<()> {
        self.inner.sync_dir(path)
    }
}

/// ADR-0127 Decision 2: detection is the marker the loader already
/// decoded, so opening a current-format JSON store reads `graph.json`
/// exactly once — there is no separate detection read or re-parse.
#[test]
fn current_json_open_reads_graph_json_exactly_once() {
    let store = memory_store_with_codec(SnapshotCodecKind::Json).unwrap();
    let (graph, mut wal) = person_ada();
    checkpoint_with_lsn(&store, &graph, wal.lsn, None).unwrap();
    let cut = wal.rotate().unwrap();
    wal.recycle(cut).unwrap();
    drop(wal);

    let counting = Arc::new(ReadCountingVfs::new(store.vfs()));
    let mut counted = store.clone();
    counted.vfs = counting.clone();
    let (graph, _wal, _, report) = open_store_with_report(&counted).unwrap();
    assert_eq!(graph.live_node_count(), 1);
    assert!(
        report.is_empty(),
        "current store must not migrate: {report}"
    );
    assert_eq!(
        counting.read_opens("graph.json"),
        1,
        "graph.json read-only opens: {:?}",
        counting.opens.lock().unwrap()
    );
}
