//! Shared test harness: the fault-injecting VFS wrapper, the commit,
//! manifest, forged-part, legacy-layout and graph-state helpers, and the
//! store assertions the native-snapshot test themes build on.

use super::super::*;
use crate::codec::parquet::decoded_preflight;
use crate::vfs::VfsMeta;
use crate::{memory_store, open_store, StoreTarget, Vfs, VfsFile, Wal};
use cypherlite_core::Value;
use cypherlite_executor::{Graph, RedoOp};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::SeekFrom;
use std::ops::Range;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

pub(super) struct InjectVfs {
    pub(super) inner: Arc<dyn Vfs>,
    pub(super) oversized: Option<String>,
    pub(super) metadata_lies: bool,
    pub(super) metadata_error: Option<String>,
    pub(super) length_changes: bool,
    pub(super) reads: Arc<AtomicUsize>,
    pub(super) fail_part: Option<usize>,
    pub(super) fail_manifest_sync: bool,
    pub(super) fail_manifest_rename: bool,
    pub(super) part_opens: AtomicUsize,
    /// Count opens of this path in `watched_opens`; each bounded read of an
    /// object opens it once.
    pub(super) watched: Option<String>,
    pub(super) watched_opens: Arc<AtomicUsize>,
    #[cfg(unix)]
    pub(super) alias_after_sync: Option<(String, std::path::PathBuf, std::path::PathBuf)>,
}

impl InjectVfs {
    fn new(inner: Arc<dyn Vfs>) -> Self {
        Self {
            inner,
            oversized: None,
            metadata_lies: false,
            metadata_error: None,
            length_changes: false,
            reads: Arc::new(AtomicUsize::new(0)),
            fail_part: None,
            fail_manifest_sync: false,
            fail_manifest_rename: false,
            part_opens: AtomicUsize::new(0),
            watched: None,
            watched_opens: Arc::new(AtomicUsize::new(0)),
            #[cfg(unix)]
            alias_after_sync: None,
        }
    }
    fn selected(&self, path: &str) -> bool {
        self.oversized.as_deref() == Some(path)
    }
}

/// `store` behind an [`InjectVfs`] with the faults `configure` sets;
/// returns the wrapped store and the wrapper, whose counters the caller
/// reads.
pub(super) fn inject_vfs(
    store: &StoreTarget,
    configure: impl FnOnce(&mut InjectVfs),
) -> (StoreTarget, Arc<InjectVfs>) {
    let mut fake = InjectVfs::new(store.vfs());
    configure(&mut fake);
    let fake = Arc::new(fake);
    let mut injected = store.clone();
    injected.vfs = fake.clone();
    (injected, fake)
}

/// `store` behind an [`InjectVfs`] that opens object `name` with an
/// oversized length, and reports that length in its metadata when
/// `metadata_lies`; returns the wrapped store and the wrapper.
pub(super) fn inject_oversized(
    store: &StoreTarget,
    name: &str,
    metadata_lies: bool,
) -> (StoreTarget, Arc<InjectVfs>) {
    inject_vfs(store, |fake| {
        fake.oversized = Some(name.to_owned());
        fake.metadata_lies = metadata_lies;
    })
}

/// Whether `target` keeps superseded and orphaned shard objects rather
/// than deleting them: on a Windows native directory, `retire_superseded`
/// and `sweep_orphans` retain older generations for a fallback manifest.
pub(super) fn deferred_retire(target: &StoreTarget) -> bool {
    cfg!(windows) && target.native_root.is_some()
}

fn fake_oversized_len(name: &str) -> u64 {
    if name == MANIFEST_JSON {
        MAX_JSON_OBJECT + 1
    } else {
        MAX_FILE + 1
    }
}

#[derive(Clone, Copy)]
enum OversizeMode {
    Always,
    AfterFirstRead,
}

struct InjectFile {
    inner: Box<dyn VfsFile>,
    oversized_len: u64,
    oversize_mode: OversizeMode,
    pub(super) reads: Arc<AtomicUsize>,
}
impl VfsFile for InjectFile {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.inner.write_all(buf)
    }
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.read_exact(buf)
    }
    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        self.inner.seek(pos)
    }
    fn sync_all(&mut self) -> Result<()> {
        self.inner.sync_all()
    }
    fn len(&self) -> Result<u64> {
        let changes = matches!(self.oversize_mode, OversizeMode::AfterFirstRead);
        if !changes || self.reads.load(Ordering::SeqCst) > 0 {
            Ok(self.oversized_len)
        } else {
            self.inner.len()
        }
    }
    fn set_len(&mut self, len: u64) -> Result<()> {
        self.inner.set_len(len)
    }
}
impl Vfs for InjectVfs {
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
        if self.metadata_error.as_deref() == Some(path) {
            return Err(Error::io(format!("injected metadata failure for {path}")));
        }
        let mut m = self.inner.metadata(path)?;
        if self.selected(path) && self.metadata_lies {
            m.len = fake_oversized_len(path);
        }
        Ok(m)
    }
    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>> {
        if self.watched.as_deref() == Some(path) {
            self.watched_opens.fetch_add(1, Ordering::SeqCst);
        }
        let inner = self.inner.open(path, opts)?;
        if self.selected(path) {
            Ok(Box::new(InjectFile {
                inner,
                oversized_len: fake_oversized_len(path),
                oversize_mode: if self.length_changes {
                    OversizeMode::AfterFirstRead
                } else {
                    OversizeMode::Always
                },
                reads: Arc::clone(&self.reads),
            }))
        } else {
            Ok(inner)
        }
    }
    fn open_exclusive(&self, path: &str) -> Result<Option<Box<dyn VfsFile>>> {
        if path.ends_with(".parquet")
            && self.fail_part == Some(self.part_opens.fetch_add(1, Ordering::SeqCst) + 1)
        {
            return Err(Error::storage("injected second part failure"));
        }
        self.inner.open_exclusive(path)
    }
    fn remove_file(&self, path: &str) -> Result<()> {
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &str) -> Result<()> {
        self.inner.remove_dir_all(path)
    }
    fn rename(&self, from: &str, to: &str) -> Result<()> {
        if to == MANIFEST_JSON && self.fail_manifest_rename {
            return Err(Error::io("injected manifest rename failure"));
        }
        self.inner.rename(from, to)
    }
    fn sync_dir(&self, path: &str) -> Result<()> {
        #[cfg(unix)]
        if let Some((dir, source, destination)) = &self.alias_after_sync {
            if path == dir {
                use std::os::unix::fs::symlink;
                self.inner.sync_dir(path)?;
                std::fs::rename(source, destination)?;
                symlink(destination, source)?;
                return Ok(());
            }
        }
        if path.is_empty() && self.fail_manifest_sync && self.part_opens.load(Ordering::SeqCst) > 0
        {
            return Err(Error::io("injected manifest directory sync failure"));
        }
        self.inner.sync_dir(path)
    }
}

/// The no-ontology spelling of
/// [`super::super::checkpoint_at_with_bytes`] every pre-ADR-0120 test
/// wants: the ontology document is `None` unless a test exercises
/// ADR-0120 itself.
pub(super) fn checkpoint_at(
    store: &StoreTarget,
    graph: &Graph,
    checkpoint_lsn: u64,
    snapshot_ts: u64,
    target_bytes: u64,
) -> Result<()> {
    super::super::checkpoint_at_with_bytes(
        store,
        graph,
        checkpoint_lsn,
        snapshot_ts,
        target_bytes,
        None,
    )
}

/// Take the open write off the graph. The WAL append in
/// [`commit_open_write`] has to see the ops before `apply_redo` consumes them,
/// so the two commit helpers share this prefix rather than calling each other.
pub(super) fn finish_write(graph: &Graph) -> (Vec<RedoOp>, u64) {
    let ops = graph.take_redo();
    let ts = graph.pending_commit_ts();
    graph.abort_write();
    (ops, ts)
}

/// Apply a taken write onto the graph and return its commit timestamp.
pub(super) fn apply_commit(graph: &Graph) -> u64 {
    let (ops, ts) = finish_write(graph);
    graph.apply_redo(ops, ts).unwrap();
    ts
}

/// Finish an open write: the redo leaves the scratch, lands in the WAL, and
/// is applied onto `graph`. Callers open the write and stage their ops.
pub(super) fn commit_open_write(graph: &Graph, wal: &mut Wal) {
    let (ops, ts) = finish_write(graph);
    wal.append_commit(ts, &ops).unwrap();
    graph.apply_redo(ops, ts).unwrap();
}

pub(super) fn committed_nodes(graph: &Graph, wal: &mut Wal, labels: [&str; 2]) {
    graph.begin_write().unwrap();
    for label in labels {
        graph
            .create_node(vec![label.into()], Default::default())
            .unwrap();
    }
    commit_open_write(graph, wal);
}

pub(super) fn commit_two_nodes(graph: &Graph, wal: &mut Wal) {
    graph.begin_write().unwrap();
    graph.set_next_node_id(0);
    graph
        .create_node(vec!["A".into()], Default::default())
        .unwrap();
    graph.set_next_node_id(300_000);
    graph
        .create_node(vec!["B".into()], Default::default())
        .unwrap();
    commit_open_write(graph, wal);
}

pub(super) fn modify_two_nodes(graph: &Graph, wal: &mut Wal) {
    graph.begin_write().unwrap();
    graph.set_property_node(0, "x", cypherlite_core::Value::Int(7));
    graph.set_property_node(300_000, "x", cypherlite_core::Value::Int(8));
    commit_open_write(graph, wal);
}

pub(super) fn commit_test_rel(graph: &Graph, wal: &mut Wal) {
    graph.begin_write().unwrap();
    graph
        .create_rel(0, 300_000, "LINK".into(), Default::default())
        .unwrap();
    commit_open_write(graph, wal);
}

/// [`commit_test_rel`], then a relationship back at id 300_000, so each
/// kind's one part over the two nodes of [`commit_two_nodes`] holds rows at
/// both ends.
pub(super) fn commit_spread_rels(graph: &Graph, wal: &mut Wal) {
    commit_test_rel(graph, wal);
    graph.begin_write().unwrap();
    graph.set_next_rel_id(300_000);
    graph
        .create_rel(300_000, 0, "LINK".into(), Default::default())
        .unwrap();
    commit_open_write(graph, wal);
}

/// One `text` property of `len` characters.
pub(super) fn text(len: usize) -> BTreeMap<String, Value> {
    BTreeMap::from([("text".to_owned(), Value::String("x".repeat(len).into()))])
}

/// Stage `n` nodes, each with a `len`-character string property, in a new
/// write; the caller commits it.
pub(super) fn stage_nodes(graph: &Graph, n: usize, len: usize) {
    graph.begin_write().unwrap();
    for _ in 0..n {
        graph.create_node(vec!["Item".into()], text(len)).unwrap();
    }
}

/// Stage a property change on every node `pick` selects, in a new write;
/// the caller commits it.
pub(super) fn stage_dirty(graph: &Graph, round: i64, pick: impl Fn(i64) -> bool) {
    let ids = graph.all_node_ids();
    graph.begin_write().unwrap();
    for id in ids.into_iter().filter(|id| pick(*id)) {
        graph.set_property_node(id, "round", Value::Int(round));
    }
}

/// The manifest `store` publishes.
pub(super) fn manifest(store: &StoreTarget) -> Manifest {
    read_manifest_for_test(store).unwrap().unwrap()
}

/// Publish `manifest`, which may be forged, as `store`'s manifest; returns
/// the published bytes.
pub(super) fn put_manifest(store: &StoreTarget, manifest: &impl Serialize) -> Vec<u8> {
    let bytes = serde_json::to_vec(manifest).unwrap();
    put_object(store, MANIFEST_JSON, &bytes).unwrap();
    bytes
}

/// A node part over `[lo, hi)` under a generated name, whose file holds
/// `rows` rows in `bytes`.
pub(super) fn node_part(lo: i64, hi: i64, rows: u64, bytes: u64) -> ShardPart {
    ShardPart {
        path: ShardName::random(Kind::Node, lo, hi).unwrap().path(),
        id_lo: lo,
        id_hi: hi,
        rows,
        bytes,
    }
}

/// Give `kind`'s part `index` in `manifest` a new file over the part's
/// range holding `bytes`, which hold `rows` rows, and publish `manifest`.
/// The part's previous file, if any, stays. Returns the part's path and the
/// published manifest bytes.
pub(super) fn forge_part(
    store: &StoreTarget,
    manifest: &mut Manifest,
    kind: Kind,
    index: usize,
    bytes: &[u8],
    rows: u64,
) -> (String, Vec<u8>) {
    let part = &mut kind.parts_mut(manifest)[index];
    part.path = ShardName::random(kind, part.id_lo, part.id_hi)
        .unwrap()
        .path();
    put_object(store, &part.path, bytes).unwrap();
    (part.rows, part.bytes) = (rows, bytes.len() as u64);
    let path = part.path.clone();
    (path, put_manifest(store, &*manifest))
}

/// Append a legacy node part over `range` to `manifest`, its file holding
/// `ids` as `graph` has them at `ts`, and publish `manifest`. A
/// v0.4.0–v0.11.x store can hold parts that overlap and both hold an id.
pub(super) fn add_legacy_part(
    store: &StoreTarget,
    manifest: &mut Manifest,
    graph: &Graph,
    ts: u64,
    range: Range<i64>,
    ids: impl IntoIterator<Item = i64>,
) {
    let ids: Vec<i64> = ids.into_iter().collect();
    let bytes = nodes_parquet_ids(graph, ts, &ids).unwrap();
    let parts = &mut manifest.node_parts;
    parts.push(ShardPart {
        path: String::new(),
        id_lo: range.start,
        id_hi: range.end,
        rows: 0,
        bytes: 0,
    });
    let index = parts.len() - 1;
    forge_part(store, manifest, Kind::Node, index, &bytes, ids.len() as u64);
}

/// The published manifest with its node parts and their files removed, for
/// a test to lay out legacy parts with [`add_legacy_part`].
pub(super) fn without_node_parts(store: &StoreTarget) -> Manifest {
    let mut legacy = manifest(store);
    for part in legacy.node_parts.drain(..) {
        delete_object(store, &part.path).unwrap();
    }
    legacy
}

/// A store whose `n` nodes, each with a 24-character `text`, are held by
/// legacy node parts over `ranges`, each holding the ids of its own range.
/// The store must reopen as the graph that wrote it; returns the store and
/// the reopened graph.
pub(super) fn legacy_layout(
    n: usize,
    ranges: impl IntoIterator<Item = Range<i64>>,
) -> (StoreTarget, Graph) {
    let store = memory_store();
    let graph = Graph::default();
    stage_nodes(&graph, n, 24);
    let ts = apply_commit(&graph);
    checkpoint_at_with_bytes(&store, &graph, 0, ts, MAX_FILE, None).unwrap();
    let mut legacy = without_node_parts(&store);
    for range in ranges {
        add_legacy_part(&store, &mut legacy, &graph, ts, range.clone(), range);
    }
    let opened = assert_reopens_as(&store, &graph);
    (store, opened)
}

/// The `[id_lo, id_hi)` range of each of `parts`.
pub(super) fn ranges(parts: &[ShardPart]) -> Vec<(i64, i64)> {
    parts.iter().map(|p| (p.id_lo, p.id_hi)).collect()
}

/// The ids in published shard `path`, in file order.
pub(super) fn shard_ids(store: &StoreTarget, path: &str) -> Vec<i64> {
    use arrow::array::{Array, Int64Array};
    let file = bytes::Bytes::from(get_object(store, path).unwrap());
    let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
        .unwrap()
        .build()
        .unwrap();
    let mut ids = Vec::new();
    for batch in reader {
        let batch = batch.unwrap();
        let column = batch.column(0).as_any().downcast_ref::<Int64Array>();
        ids.extend(column.unwrap().values().iter());
    }
    ids
}

/// The published node parts are disjoint, hold each live node id once
/// between them, are the only node files, and reopen as `graph`.
pub(super) fn assert_healed(store: &StoreTarget, graph: &Graph) -> Vec<ShardPart> {
    let parts = manifest(store).node_parts;
    for pair in parts.windows(2) {
        assert!(pair[0].id_hi <= pair[1].id_lo, "{parts:?}");
    }
    let held: Vec<i64> = parts
        .iter()
        .flat_map(|p| shard_ids(store, &p.path))
        .collect();
    let mut live = graph.all_node_ids();
    live.sort_unstable();
    assert_eq!(held, live);
    assert_eq!(store.vfs.list_dir("nodes").unwrap().len(), parts.len());
    assert_reopens_as(store, graph);
    parts
}

/// Live decoded node bytes of `ids` in `graph` at `ts`, measured as
/// planning measures them.
pub(super) fn decoded(graph: &Graph, ts: u64, ids: &[i64]) -> u64 {
    decoded_preflight(graph, ts, ids, Kind::Node).unwrap()
}

/// The published node parts tile `range` edge to edge, and each file holds
/// at most `target` decoded bytes of `graph` at `ts`, unless it holds a
/// single row, which can be over the target by itself (ADR-0183 Decision
/// 3). Returns the parts.
pub(super) fn assert_tiles(
    store: &StoreTarget,
    graph: &Graph,
    ts: u64,
    range: Range<i64>,
    target: u64,
) -> Vec<ShardPart> {
    let parts = manifest(store).node_parts;
    let ends = (
        parts.first().map(|p| p.id_lo),
        parts.last().map(|p| p.id_hi),
    );
    assert_eq!(ends, (Some(range.start), Some(range.end)), "{parts:?}");
    for pair in parts.windows(2) {
        assert_eq!(pair[0].id_hi, pair[1].id_lo, "{parts:?}");
    }
    for part in &parts {
        let ids = shard_ids(store, &part.path);
        let bytes = decoded(graph, ts, &ids);
        assert!(bytes <= target || ids.len() == 1, "{bytes} bytes: {part:?}");
    }
    parts
}

/// `store` behind an [`InjectVfs`] that counts opens of `path`; returns the
/// wrapped store and the counter.
pub(super) fn watch_opens(store: &StoreTarget, path: &str) -> (StoreTarget, Arc<AtomicUsize>) {
    let (watched, fake) = inject_vfs(store, |fake| fake.watched = Some(path.to_owned()));
    (watched, Arc::clone(&fake.watched_opens))
}

/// Row counts of the row groups in published shard `path`.
pub(super) fn row_group_rows(store: &StoreTarget, path: &str) -> Vec<i64> {
    let file = bytes::Bytes::from(get_object(store, path).unwrap());
    parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file)
        .unwrap()
        .metadata()
        .row_groups()
        .iter()
        .map(|group| group.num_rows())
        .collect()
}

#[derive(Debug, PartialEq)]
pub(super) struct GraphState {
    nodes: Vec<cypherlite_core::Node>,
    rels: Vec<cypherlite_core::Relationship>,
}

pub(super) fn graph_state(graph: &Graph) -> GraphState {
    let mut nodes: Vec<_> = graph
        .all_node_ids()
        .into_iter()
        .map(|id| graph.node(id).unwrap())
        .collect();
    nodes.sort_by_key(|n| n.id);
    let mut rels: Vec<_> = graph
        .all_rel_ids()
        .into_iter()
        .map(|id| graph.rel(id).unwrap())
        .collect();
    rels.sort_by_key(|r| r.id);
    GraphState { nodes, rels }
}

/// Reopen `store`, which must reopen as `graph`; returns the reopened graph.
pub(super) fn assert_reopens_as(store: &StoreTarget, graph: &Graph) -> Graph {
    let (reopened, _) = open_store(store).unwrap();
    assert_eq!(graph_state(&reopened), graph_state(graph));
    reopened
}

/// Opening `store` must fail with a storage error that names manifest part
/// `path` and says `expected`.
pub(super) fn assert_fails_open(store: &StoreTarget, path: &str, expected: &str) {
    let err = open_store(store)
        .err()
        .expect("a part that breaks the shard rules must fail open");
    assert!(matches!(err, Error::Storage(_)), "{err}");
    let named = format!("manifest part {path}: ");
    assert!(err.to_string().contains(&named), "{err}");
    assert!(err.to_string().contains(expected), "{err}");
}
