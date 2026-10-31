use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cypherlite_catalog::{Catalog, ConstraintDef, IndexDef};
use cypherlite_core::{Namespace, Node, NodeId, RelId, Relationship, Result, Value};

use crate::group_key::GroupKey;
use crate::pull_identity::PullIdentityIndex;
use crate::redo::{self, PullKind, RedoOp};
use crate::store::{Interner, LiveNode, LiveRel, TypedAdj};

use adjacency::{adj_insert, adj_reposition};
use degree_stats::CountDelta;

// The count-only reader's probe shape (LABS-370 round 3, F-1-5): re-exported
// for the recognizer in `exec` — the window itself lives on `Graph`.
pub(crate) use degree_stats::CountProbe;

mod adjacency;
mod apply_redo;
mod conflict;
mod declared;
mod degree_stats;
mod degree_stats_estimate;
mod degree_stats_incident;
mod index;
mod label_mutation;
mod label_probe;
mod query_stats;
mod redo_preflight;
mod rel_postings;
mod restore;
mod services;
#[cfg(test)]
pub(crate) mod test_support;
mod theater;

pub(crate) use declared::{spatial_holds, trigram_count, RangeSeek, SpatialSeek};
use declared::{GramIndex, PointIndex, RangeIndex};
use index::Indexes;
#[cfg(test)]
pub(crate) use label_probe::take_label_probe_checks;
pub(crate) use label_probe::LabelProbe;
pub use query_stats::QueryStats;
pub(crate) use services::StoreServices;

use theater::TheaterData;

/// The core-store map: integer keys, FxHash — SipHash + the default
/// hasher on dense engine-generated ids was profiled as a top-2 cost
/// (LABS-337/LABS-358); ADR-0355's rule: a key that is attacker-writable
/// must not use it.
pub(crate) type CoreMap<K, V> =
    dashmap::DashMap<K, V, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>;

/// The visible version at `ts` — the one walk every `*_at` read takes
/// over a version chain (LABS-337's extraction of the previously
/// five-times-spelled rev-find).
fn visible_version<Live>(vers: &[Ver<Live>], ts: u64) -> Option<&Ver<Live>> {
    vers.iter().rev().find(|v| visible(v.xmin, v.xmax, ts))
}

/// The LIVE version — the last `xmax == 0` one, not a ts-probe: the
/// maintained-statistics paths run inside the write window, where a rel
/// created earlier in the same batch is invisible at `commit_ts` and one
/// deleted in the batch keeps a live-looking pre-delete version. The
/// `xmax == 0` convention is the one `live_label_ids` and the rebuild's
/// live-rel scan use. (The other historical inline spellings of this
/// walk — the delete paths, the rebuild scan — fold onto this helper as
/// night-shift backlog work, not in this fix's scope.)
fn live_version<Live>(vers: &[Ver<Live>]) -> Option<&Ver<Live>> {
    vers.iter().rev().find(|v| v.xmax == 0)
}

pub use theater::{CheckpointTheater, DatabaseEntry, SchemaSnapshot};
pub(crate) use theater::{DatabasesCatalog, ShowSchema};

/// `Atomic*::fetch_update` was renamed to `try_update` in Rust 1.95 and
/// deprecated in 1.99. MSRV is 1.88, where `try_update` does not exist, so
/// the call keeps the old name. `#[expect(deprecated)]` is wrong here: on
/// 1.88 the lint does not fire and the build would fail.
macro_rules! atomic_fetch_update {
    ($atom:expr, $set:expr, $fetch:expr, $f:expr) => {{
        #[allow(
            deprecated,
            reason = "fetch_update renamed to try_update in 1.99; try_update needs 1.95, MSRV is 1.88"
        )]
        {
            $atom.fetch_update($set, $fetch, $f)
        }
    }};
}

/// An exhaustible `i64` id counter shared by the committed graph counters and
/// the overlay scratch counters. Both allocate the current value and advance;
/// both must fail rather than wrap once `i64::MAX` is reached. The two atomic
/// widths exist because the overlay seeds its counters from the graph's.
pub(crate) trait ExhaustibleIdCounter {
    fn try_alloc_id(&self) -> std::result::Result<i64, i64>;
}

impl ExhaustibleIdCounter for AtomicI64 {
    fn try_alloc_id(&self) -> std::result::Result<i64, i64> {
        atomic_fetch_update!(self, Ordering::Relaxed, Ordering::Relaxed, |id| {
            if (0..i64::MAX).contains(&id) {
                Some(id + 1)
            } else {
                None
            }
        })
    }
}

impl ExhaustibleIdCounter for AtomicU64 {
    fn try_alloc_id(&self) -> std::result::Result<i64, i64> {
        atomic_fetch_update!(self, Ordering::Relaxed, Ordering::Relaxed, |id| {
            (id < i64::MAX as u64).then_some(id + 1)
        })
        .map(|id| id as i64)
        .map_err(|id| id as i64)
    }
}

/// The `DeleteConnectedNode` execution error, shared by every node-delete
/// path so the message never drifts between them.
pub(crate) fn delete_connected_node_error() -> cypherlite_core::Error {
    cypherlite_core::Error::execution(
        "DeleteConnectedNode: Cannot delete node with relationships unless DETACH",
    )
}

/// Allocate the next exhaustible id from `counter`, mapping exhaustion to the
/// caller's `<what> ID exhausted` error.
pub(crate) fn alloc_exhaustible_id(counter: &impl ExhaustibleIdCounter, what: &str) -> Result<i64> {
    counter
        .try_alloc_id()
        .map_err(|_| cypherlite_core::Error::execution(format!("{what} ID exhausted")))
}

#[derive(Debug, Clone, Default)]
pub struct Procedure {
    pub name: String,
    pub arg_names: Vec<String>,
    /// Cypher type names (`STRING`, `INTEGER`, `NUMBER`, …) aligned with `arg_names`.
    pub arg_types: Vec<String>,
    pub yield_names: Vec<String>,
    pub rows: Vec<BTreeMap<String, Value>>,
}

/// The one MVCC version-chain record: node and rel chains share the shape
/// (and, via [`cow`], the copy-on-write policy) through the aliases below.
#[derive(Debug, Clone)]
struct Ver<Live> {
    xmin: u64,
    xmax: u64,
    live: Live,
}

impl<Live> Ver<Live> {
    /// A version is collectable once its tombstone is at or before `horizon`.
    fn dead_at(&self, horizon: u64) -> bool {
        self.xmax != 0 && self.xmax <= horizon
    }
}

type NodeVer = Ver<LiveNode>;
type RelVer = Ver<LiveRel>;

pub(crate) fn node_version_stride() -> u64 {
    std::mem::size_of::<NodeVer>() as u64
}

pub(crate) fn rel_version_stride() -> u64 {
    std::mem::size_of::<RelVer>() as u64
}

/// Copy-on-write over a version chain: a tombstoned latest version is
/// gone; a version born in this write is reused; otherwise tombstone the
/// latest version and push its clone as this write's version.
fn cow<I, Live: Clone>(
    write_ts: &AtomicU64,
    map: &CoreMap<I, Vec<Ver<Live>>>,
    id: I,
) -> Option<Live>
where
    I: Eq + std::hash::Hash,
{
    let ts = write_ts.load(Ordering::Acquire);
    let mut vers = map.get_mut(&id)?;
    let last = vers.last_mut()?;
    if last.xmax != 0 {
        return None;
    }
    if last.xmin == ts {
        return Some(last.live.clone());
    }
    let live = last.live.clone();
    last.xmax = ts;
    vers.push(Ver {
        xmin: ts,
        xmax: 0,
        live: live.clone(),
    });
    crate::memory::fit_reserved(&mut vers);
    Some(live)
}

#[derive(Debug)]
pub struct Graph {
    /// Which fixed namespace of the store this graph is (ADR-0116 §1, §10).
    /// The executor needs it for namespace-qualified element ids (ADR-0116
    /// §7); `neo4j` is the default so existing construction sites are
    /// unchanged.
    namespace: Namespace,
    /// Store-level services (procedure registry, LOAD CSV policy, preview
    /// set) shared by every namespace graph of a store (ADR-0116 §10) as one
    /// `Arc` bundle. Not a field consumers can reach: the `Arc` identity is
    /// the one-bundle invariant, so access goes through
    /// [`Graph::procedure`], [`Graph::has_procedure`],
    /// [`Graph::register_procedure`], [`Graph::csv_policy`],
    /// [`Graph::preview`], [`Graph::databases`],
    /// [`Graph::configure_load_csv`], [`Graph::configure_preview`] and
    /// [`Graph::share_services_from`].
    services: Arc<StoreServices>,
    next_node_id: AtomicI64,
    next_rel_id: AtomicI64,
    /// Last published commit (ADR-0012 / ADR-0013). Stored after apply (ADR-0018).
    published_ts: AtomicU64,
    write_ts: AtomicU64,
    in_write: AtomicBool,
    record_redo: AtomicBool,
    nodes: CoreMap<NodeId, Vec<NodeVer>>,
    rels: CoreMap<RelId, Vec<RelVer>>,
    out_adj: CoreMap<NodeId, TypedAdj>,
    in_adj: CoreMap<NodeId, TypedAdj>,
    /// Live relationships per interned relationship type. Maintained beside
    /// install and delete, and rebuilt with adjacency after abort or restore.
    /// Planning uses it as the numerator of average degree.
    rel_type_counts: Mutex<HashMap<u32, u64>>,
    /// LABS-351: per-(label, rel-type) out-degree histogram, log2-bucketed
    /// (bucket k counts nodes of the label whose out-degree on the rel type
    /// is 2^k..2^(k+1)-1). The mean degree (`rel_type_rows / label_rows`)
    /// lies on the heavy-tailed shape where the hub join applies; the
    /// histogram's tail is the honest degree for start-selection and
    /// gate estimates. This is the HIGH-WATER degree, not the live one:
    /// the typed adjacency keeps tombstoned entries (its chunk length
    /// never shrinks), so the bucket only ever moves up on create — a
    /// floor that can only overestimate and never miss a supernode,
    /// which is exactly the conservative direction a gate needs. The
    /// adjacency rebuild recomputes it from the live rels in one pass.
    degree_histograms: Mutex<HashMap<(u32, u32), [u64; Self::DEGREE_BUCKETS]>>,
    /// LABS-370: exact (start-label, rel-type, end-label) rel counts —
    /// the directional pair the counting operator answers from, and the
    /// estimate the ADR-0218 ladder and the LABS-348 gate can consult.
    /// Charged to `used_bytes` on entry creation (the LABS-351 security
    /// discipline: an uncharged query-controlled map is a memory-limit
    /// bypass).
    pair_counts: Mutex<HashMap<(u32, u32, u32), u64>>,
    /// LABS-370 (v1 follow-up): the exact per-label live node counts —
    /// the counting operator's single-node answer. Unlike the label
    /// index (an upper bound that keeps deleted ids until checkpoint
    /// GC), this map is bumped at every label-set mutation seam and
    /// drained on zero, charged to `used_bytes` per entry like the pair
    /// counts (graph/degree_stats.rs owns the bump discipline).
    label_counts: Mutex<HashMap<u32, u64>>,
    interner: Interner,
    catalog: Mutex<Catalog>,
    pub(crate) theater: Mutex<TheaterData>,
    /// Exact old definitions whose enforcement was absent from a legacy image/WAL.
    legacy_unenforced: Mutex<HashMap<String, ConstraintDef>>,
    stats: Mutex<QueryStats>,
    /// All lazily maintained index state: the label index, the equality and
    /// declared pair maps, the interned sentinels and the committed index
    /// memory charge — one bundle, [`Indexes`].
    indexes: Indexes,
    pending_redo: Mutex<Vec<RedoOp>>,
    /// Source-scoped pull identity (ADR-0080).
    pull_identity: PullIdentityIndex,
    /// Ids mutated since the last successful checkpoint (ADR-0022).
    dirty_nodes: Mutex<HashSet<NodeId>>,
    dirty_rels: Mutex<HashSet<RelId>>,
    /// In-flight snapshot pins (ADR-0038). Each slot is free / claiming / ts+1.
    pins: Box<[AtomicU64]>,
    pin_cursor: AtomicU64,
    /// Live graph + index logical bytes (ADR-0042).
    used_bytes: AtomicU64,
}

impl Default for Graph {
    fn default() -> Self {
        let interner = Interner::default();
        let indexes = Indexes::new(&interner);
        Self {
            namespace: Namespace::Neo4j,
            services: Arc::new(StoreServices::default()),
            next_node_id: AtomicI64::new(0),
            next_rel_id: AtomicI64::new(0),
            published_ts: AtomicU64::new(0),
            write_ts: AtomicU64::new(0),
            in_write: AtomicBool::new(false),
            record_redo: AtomicBool::new(true),
            nodes: CoreMap::default(),
            rels: CoreMap::default(),
            out_adj: CoreMap::default(),
            in_adj: CoreMap::default(),
            rel_type_counts: Mutex::new(HashMap::new()),
            degree_histograms: Mutex::new(HashMap::new()),
            pair_counts: Mutex::new(HashMap::new()),
            label_counts: Mutex::new(HashMap::new()),
            interner,
            catalog: Mutex::new(Catalog::default()),
            theater: Mutex::new(TheaterData::default()),
            indexes,
            legacy_unenforced: Mutex::new(HashMap::new()),
            stats: Mutex::new(QueryStats::default()),
            pending_redo: Mutex::new(Vec::new()),
            pull_identity: PullIdentityIndex::default(),
            dirty_nodes: Mutex::new(HashSet::new()),
            dirty_rels: Mutex::new(HashSet::new()),
            pins: (0..PIN_SLOTS).map(|_| AtomicU64::new(PIN_FREE)).collect(),
            pin_cursor: AtomicU64::new(0),
            used_bytes: AtomicU64::new(0),
        }
    }
}

/// 256 in-flight statements; excess pins retry a bounded number of times and
/// then fail with a typed "snapshot pin" error (ADR-0038, ADR-0111).
const PIN_SLOTS: usize = 256;
const PIN_FREE: u64 = 0;
const PIN_CLAIMING: u64 = u64::MAX;
/// Bounded retry budget for a full pin table. Each retry scans all 256 slots
/// and yields once; the budget is deliberately small so a saturated table
/// fails in microseconds to low milliseconds rather than stalling forever
/// (ADR-0111). The slot array and lock-free protocol are unchanged (ADR-0038).
const PIN_ACQUIRE_RETRIES: u32 = 1024;

/// Registered `snapshot_ts` until drop (ADR-0023 / ADR-0038).
pub struct SnapshotPin<'a> {
    graph: &'a Graph,
    ts: u64,
    slot: usize,
}

impl SnapshotPin<'_> {
    pub fn ts(&self) -> u64 {
        self.ts
    }

    /// Keep the pin after this guard is forgotten (explicit txn, ADR-0028 / ADR-0038).
    pub fn detach(self) -> usize {
        let slot = self.slot;
        std::mem::forget(self);
        slot
    }
}

impl Drop for SnapshotPin<'_> {
    fn drop(&mut self) {
        self.graph.unpin_slot(self.slot);
    }
}

fn visible(xmin: u64, xmax: u64, ts: u64) -> bool {
    xmin != 0 && xmin <= ts && (xmax == 0 || xmax > ts)
}

fn checked_persisted_id(id: i64, what: &str) -> Result<i64> {
    cypherlite_core::validate_persisted_id(id, what)?;
    Ok(id + 1)
}

fn requires_enforced_label(def: &ConstraintDef) -> bool {
    def.kind
        .parse::<cypherlite_model::ConstraintType>()
        .is_ok_and(|kind| kind.is_enforced_label_kind())
}

fn matching_enforced_label(def: &ConstraintDef, c: &cypherlite_model::Constraint) -> bool {
    c.label == def.label
        && c.properties == def.properties
        && def.kind.parse::<cypherlite_model::ConstraintType>() == Ok(c.constraint_type)
        && c.property_type == def.property_type
        && c.constraint_type
            .enforced_label_site()
            .is_some_and(|site| site.is_relationship() == def.relationship)
        && c.enforced_label
            .as_deref()
            .is_some_and(|label| !label.is_empty())
}

impl Graph {
    /// Whether the committed current graph type could carry property
    /// default values (CIP-262): the committed-theater half of the
    /// statement boundary's scratch-aware gate
    /// ([`ExecGraph::has_property_defaults`] answers over both
    /// vintages). A false positive only costs one overlay read.
    pub(crate) fn has_property_defaults(&self) -> bool {
        let theater = self.theater.lock().expect("theater lock poisoned");
        spec_has_property_defaults(&theater.current_graph_type)
    }

    /// Which fixed namespace of the store this graph is (ADR-0116 §1).
    pub fn namespace(&self) -> Namespace {
        self.namespace
    }

    /// Set the graph's namespace identity. Called by the store when it
    /// creates or swaps a namespace slot (ADR-0116 §10); not public API.
    #[doc(hidden)]
    pub fn set_namespace(&mut self, namespace: Namespace) {
        self.namespace = namespace;
    }

    pub fn commit_ts(&self) -> u64 {
        self.published_ts.load(Ordering::Acquire)
    }

    /// Whether a base write window is in flight (`begin_write`..
    /// `commit_write` / `apply_redo`): the maintained statistics are
    /// already bumped for it, but the commit is not published yet — a
    /// concurrent reader must not answer from them (the count-only
    /// short-circuit's exactness precondition, LABS-370 v1 follow-up).
    pub(crate) fn in_write_window(&self) -> bool {
        self.in_write.load(Ordering::Acquire)
    }

    pub fn set_commit_ts(&self, ts: u64) {
        self.published_ts.store(ts, Ordering::Release);
    }

    /// Live graph + index logical bytes (ADR-0042, ADR-0202).
    pub fn used_bytes(&self) -> u64 {
        self.used_bytes
            .load(Ordering::Relaxed)
            .saturating_add(self.interner.bytes())
    }

    /// Full walk of retained versions, adjacency, and the interner
    /// (test / `rebuild_indexes` oracle).
    pub fn accounted_bytes(&self) -> u64 {
        self.structure_bytes().saturating_add(self.interner.bytes())
    }

    fn structure_bytes(&self) -> u64 {
        let charge = self.index_charge();
        let mut n = 0u64;
        for entry in self.nodes.iter() {
            let vers = entry.value();
            n = n.saturating_add(crate::memory::node_chain_bytes(vers.len()));
            for v in vers.iter() {
                n = n.saturating_add(crate::memory::live_node_account_live(
                    &v.live,
                    &self.interner,
                    &charge,
                ));
            }
        }
        for entry in self.rels.iter() {
            let vers = entry.value();
            n = n.saturating_add(crate::memory::rel_chain_bytes(vers.len()));
            for v in vers.iter() {
                n = n.saturating_add(crate::memory::live_rel_account_live(&v.live));
            }
        }
        for entry in self.out_adj.iter() {
            n = n.saturating_add(entry.value().floor_bytes());
        }
        for entry in self.in_adj.iter() {
            n = n.saturating_add(entry.value().floor_bytes());
        }
        // LABS-351 / LABS-370: the degree histograms and the pair
        // counts are retained structure — the recount oracle must
        // include them or the incremental entry charges drift from it
        // (the #398 lesson). The term lives with the family
        // (graph/degree_stats.rs).
        n = n.saturating_add(self.degree_stats_structure_bytes());
        n
    }

    pub fn recompute_used(&self) {
        self.used_bytes
            .store(self.structure_bytes(), Ordering::Relaxed);
    }

    /// Live-size delta of `ops`. Nodes are accounted under the charge in
    /// force after `ops`; when that differs from the current charge (DDL in
    /// the batch), every retained version — including ones `ops` tombstone —
    /// is moved from the old charge to the new one. A delete keeps its
    /// record until GC, so it adds nothing. A copy-on-write set adds the
    /// new version.
    pub fn estimate_redo_delta(&self, ops: &[RedoOp]) -> i64 {
        let before = self.index_charge();
        let after = self.charge_after_ops(ops);
        let mut delta = crate::memory::estimate_redo_delta(
            ops,
            |id| self.node(id),
            |id| self.rel_form(id),
            |id| self.nodes.get(&id).map(|v| v.len()).unwrap_or(0),
            |id| self.rels.get(&id).map(|v| v.len()).unwrap_or(0),
            &after,
        );
        delta += self.estimate_adj_delta(ops);
        delta += self.estimate_intern_delta(ops);
        // LABS-370 (security follow-up): the count family's entry growth
        // is committed structure too — an oversized-label write must be
        // rejected at admission, not after its entries land past the cap
        // (graph/degree_stats.rs owns the term).
        delta += self.estimate_stats_entry_growth(ops);
        if before.as_ref() != &after {
            for entry in self.nodes.iter() {
                let id = *entry.key();
                for v in entry.value().iter() {
                    let n = v.live.to_node(id, &self.interner);
                    delta += crate::memory::live_node_account(&n, &after) as i64
                        - crate::memory::live_node_account(&n, &before) as i64;
                }
            }
        }
        delta
    }

    fn estimate_adj_delta(&self, ops: &[RedoOp]) -> i64 {
        let mut out: HashMap<NodeId, Vec<(u32, usize)>> = HashMap::new();
        let mut inn: HashMap<NodeId, Vec<(u32, usize)>> = HashMap::new();
        let mut fresh_ids = FreshIds::new();
        let mut delta = 0i64;
        for op in ops {
            let RedoOp::CreateRel {
                start,
                end,
                rel_type,
                ..
            } = op
            else {
                continue;
            };
            let type_id = fresh_ids.resolve(self, rel_type);
            delta += bump_adj(&mut out, *start, type_id, || {
                hop_counts(&self.out_adj, *start)
            });
            delta += bump_adj(&mut inn, *end, type_id, || hop_counts(&self.in_adj, *end));
        }
        delta
    }

    fn estimate_intern_delta(&self, ops: &[RedoOp]) -> i64 {
        let mut seen = HashSet::new();
        let mut extra = 0i64;
        let mut consider = |s: &str| {
            if !seen.insert(s.to_string()) {
                return;
            }
            if self.interner.get_id(s).is_none() {
                extra += crate::memory::interned_string_bytes(s.len()) as i64;
            }
        };
        for op in ops {
            match op {
                RedoOp::CreateNode {
                    labels, properties, ..
                } => {
                    for label in labels.iter() {
                        consider(label);
                    }
                    for key in properties.keys() {
                        consider(key);
                    }
                }
                RedoOp::CreateRel {
                    rel_type,
                    properties,
                    ..
                } => {
                    consider(rel_type);
                    for key in properties.keys() {
                        consider(key);
                    }
                }
                RedoOp::SetNodeProp { key, .. }
                | RedoOp::RemoveNodeProp { key, .. }
                | RedoOp::SetRelProp { key, .. }
                | RedoOp::RemoveRelProp { key, .. } => consider(key),
                RedoOp::AddLabels { labels, .. } | RedoOp::RemoveLabels { labels, .. } => {
                    for label in labels {
                        consider(label);
                    }
                }
                _ => {}
            }
        }
        extra
    }

    fn index_charge(&self) -> Arc<crate::memory::IndexCharge> {
        Arc::clone(&self.indexes.charge_cache.lock().unwrap())
    }

    fn refresh_index_charge(&self) {
        let charge = {
            let theater = self.theater.lock().unwrap();
            Arc::new(crate::memory::IndexCharge::from_details(
                theater.index_details.values(),
            ))
        };
        *self.indexes.charge_cache.lock().unwrap() = charge;
    }

    fn charge_after_ops(&self, ops: &[RedoOp]) -> crate::memory::IndexCharge {
        let theater = self.theater.lock().unwrap();
        let mut details = theater.index_details.clone();
        drop(theater);
        crate::redo::project_index_details(&mut details, ops);
        crate::memory::IndexCharge::from_details(details.values())
    }

    fn charge(&self, delta: i64) {
        if delta > 0 {
            self.used_bytes.fetch_add(delta as u64, Ordering::Relaxed);
        } else if delta < 0 {
            let sub = (-delta) as u64;
            atomic_fetch_update!(self.used_bytes, Ordering::Relaxed, Ordering::Relaxed, |u| {
                Some(u.saturating_sub(sub))
            })
            .ok();
        }
    }

    fn charge_node_swap(&self, before: Option<&Node>, after: Option<&Node>) {
        let charge = self.index_charge();
        let b = before
            .map(|n| crate::memory::live_node_account(n, &charge))
            .unwrap_or(0);
        let a = after
            .map(|n| crate::memory::live_node_account(n, &charge))
            .unwrap_or(0);
        self.charge(a as i64 - b as i64);
    }

    fn charge_rel_after_edit(&self, id: RelId, before_bytes: u64, before_len: usize) {
        let after_len = self.rel_chain_len(id);
        let after_bytes = self.visible_rel_account(id);
        if after_len > before_len {
            let growth = crate::memory::rel_chain_bytes(after_len) as i64
                - crate::memory::rel_chain_bytes(before_len) as i64;
            self.charge(after_bytes as i64 + growth);
        } else {
            self.charge(after_bytes as i64 - before_bytes as i64);
        }
    }

    fn node_chain_len(&self, id: NodeId) -> usize {
        self.nodes.get(&id).map(|v| v.len()).unwrap_or(0)
    }

    fn rel_chain_len(&self, id: RelId) -> usize {
        self.rels.get(&id).map(|v| v.len()).unwrap_or(0)
    }

    /// Copy-on-write keeps the previous version. An in-place edit replaces it.
    fn charge_node_after_edit(&self, id: NodeId, before: Option<&Node>, before_len: usize) {
        let after = self.node(id);
        let after_len = self.node_chain_len(id);
        if after_len > before_len {
            let charge = self.index_charge();
            let added = after
                .as_ref()
                .map(|n| crate::memory::live_node_account(n, &charge))
                .unwrap_or(0);
            let growth = crate::memory::node_chain_bytes(after_len) as i64
                - crate::memory::node_chain_bytes(before_len) as i64;
            self.charge(added as i64 + growth);
        } else {
            self.charge_node_swap(before, after.as_ref());
        }
    }

    pub fn next_node_id(&self) -> NodeId {
        self.next_node_id.load(Ordering::Relaxed)
    }

    pub fn set_next_node_id(&self, id: NodeId) {
        self.next_node_id.store(id, Ordering::Relaxed);
    }

    pub fn next_rel_id(&self) -> RelId {
        self.next_rel_id.load(Ordering::Relaxed)
    }

    fn mark_node_dirty(&self, id: NodeId) {
        self.dirty_nodes.lock().unwrap().insert(id);
    }

    fn mark_rel_dirty(&self, id: RelId) {
        self.dirty_rels.lock().unwrap().insert(id);
    }

    /// Pin `commit_ts` so GC cannot drop versions this statement still sees.
    ///
    /// The slot protocol is lock-free (ADR-0038). When every slot is
    /// occupied the wait is bounded: the array is retried a fixed number of
    /// times and then acquisition fails with a typed
    /// `Error::Execution("snapshot pin: …")` instead of spinning forever
    /// (ADR-0111).
    pub fn pin_now(&self) -> Result<SnapshotPin<'_>> {
        self.pin_now_with_retry(std::thread::yield_now)
    }

    /// Run the callback once after each unsuccessful full-table scan.
    fn pin_now_with_retry(&self, mut retry: impl FnMut()) -> Result<SnapshotPin<'_>> {
        let start = self.pin_cursor.fetch_add(1, Ordering::Relaxed) as usize;
        for _ in 0..PIN_ACQUIRE_RETRIES {
            for i in 0..PIN_SLOTS {
                let slot = (start + i) % PIN_SLOTS;
                if self.pins[slot]
                    .compare_exchange(PIN_FREE, PIN_CLAIMING, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    let ts = self.commit_ts();
                    self.pins[slot].store(ts.saturating_add(1), Ordering::Release);
                    return Ok(SnapshotPin {
                        graph: self,
                        ts,
                        slot,
                    });
                }
            }
            retry();
        }
        Err(cypherlite_core::Error::execution(format!(
            "snapshot pin: all {PIN_SLOTS} slots are held"
        )))
    }

    pub fn unpin_slot(&self, slot: usize) {
        self.pins[slot].store(PIN_FREE, Ordering::Release);
    }

    /// True while any statement or explicit transaction holds a snapshot pin.
    pub fn snapshot_pinned(&self) -> bool {
        self.pins
            .iter()
            .any(|slot| slot.load(Ordering::Acquire) != PIN_FREE)
    }

    /// Lowest timestamp that may still be read. Dead iff `xmax != 0 && xmax <= horizon`.
    pub fn gc_horizon(&self) -> u64 {
        let commit = self.commit_ts();
        let mut min_pin: Option<u64> = None;
        for slot in self.pins.iter() {
            let v = slot.load(Ordering::Acquire);
            if v == PIN_CLAIMING {
                return 0;
            }
            if v != PIN_FREE {
                let ts = v - 1;
                min_pin = Some(min_pin.map_or(ts, |m| m.min(ts)));
            }
        }
        min_pin.unwrap_or(commit).min(commit)
    }

    pub fn set_next_rel_id(&self, id: RelId) {
        self.next_rel_id.store(id, Ordering::Relaxed);
    }

    fn read_ts(&self) -> u64 {
        if self.in_write.load(Ordering::Acquire) {
            self.write_ts.load(Ordering::Acquire)
        } else {
            self.commit_ts()
        }
    }

    /// Begin a write statement: versions get xmin = commit_ts+1 until publish.
    /// Returns `Execution` if the next timestamp is the unsupported sentinel.
    pub fn begin_write(&self) -> Result<()> {
        let ts = self
            .commit_ts()
            .checked_add(1)
            .filter(|ts| *ts < u64::MAX)
            .ok_or_else(|| cypherlite_core::Error::execution("write timestamp exhausted"))?;
        self.begin_write_at(ts)
    }

    /// Begin a write with an explicit txn id (ADR-0015 optimistic snapshot).
    /// Returns `Execution` if `write_ts` is the unsupported sentinel.
    pub fn begin_write_at(&self, write_ts: u64) -> Result<()> {
        if write_ts == u64::MAX {
            return Err(cypherlite_core::Error::execution(
                "write timestamp exhausted",
            ));
        }
        self.in_write.store(true, Ordering::Release);
        self.write_ts.store(write_ts, Ordering::Release);
        self.pending_redo.lock().unwrap().clear();
        self.record_redo.store(true, Ordering::Release);
        self.reset_stats();
        Ok(())
    }

    fn alloc_node_id(&self) -> Result<NodeId> {
        alloc_exhaustible_id(&self.next_node_id, "node")
    }

    fn alloc_rel_id(&self) -> Result<RelId> {
        alloc_exhaustible_id(&self.next_rel_id, "relationship")
    }

    pub fn pending_commit_ts(&self) -> u64 {
        self.write_ts.load(Ordering::Acquire)
    }

    pub fn take_redo(&self) -> Vec<RedoOp> {
        std::mem::take(&mut *self.pending_redo.lock().unwrap())
    }

    /// Publish after WAL fsync.
    pub fn commit_write(&self) {
        self.set_commit_ts(self.write_ts.load(Ordering::Acquire));
        self.in_write.store(false, Ordering::Release);
        self.pending_redo.lock().unwrap().clear();
    }

    pub fn abort_write(&self) {
        let ts = self.write_ts.load(Ordering::Acquire);
        for mut vers in self.nodes.iter_mut() {
            vers.retain(|v| v.xmin != ts);
            for v in vers.iter_mut() {
                if v.xmax == ts {
                    v.xmax = 0;
                }
            }
        }
        for mut vers in self.rels.iter_mut() {
            vers.retain(|v| v.xmin != ts);
            for v in vers.iter_mut() {
                if v.xmax == ts {
                    v.xmax = 0;
                }
            }
        }
        self.in_write.store(false, Ordering::Release);
        self.pending_redo.lock().unwrap().clear();
        self.rebuild_adj();
        self.rebuild_indexes();
    }

    pub fn reset_stats(&self) {
        *self.stats.lock().unwrap() = QueryStats::default();
    }

    pub fn node_at(&self, id: NodeId, ts: u64) -> Option<Node> {
        self.nodes.get(&id).and_then(|vers| {
            visible_version(vers.as_slice(), ts).map(|v| v.live.to_node(id, &self.interner))
        })
    }

    /// Label-only view of a node — no property-map materialization (ADR-0033).
    /// Callers that only need labels avoid `node_at`, which builds the full
    /// property map per node (the profile-hot `CompactProps::to_map` path).
    pub fn node_labels_at(&self, id: NodeId, ts: u64) -> Option<Vec<String>> {
        self.nodes.get(&id).and_then(|vers| {
            visible_version(vers.as_slice(), ts).map(|v| v.live.label_strings(&self.interner))
        })
    }

    pub fn rel_at(&self, id: RelId, ts: u64) -> Option<Relationship> {
        self.rels.get(&id).and_then(|vers| {
            visible_version(vers.as_slice(), ts).map(|v| v.live.to_rel(id, &self.interner))
        })
    }

    /// Endpoints only — expand / BFS must not materialize property maps (ADR-0033).
    pub fn rel_ends_at(&self, id: RelId, ts: u64) -> Option<(NodeId, NodeId)> {
        self.rels.get(&id).and_then(|vers| {
            visible_version(vers.as_slice(), ts).map(|v| (v.live.start, v.live.end))
        })
    }

    /// The rel's LIVE identity — its endpoints and type (round 3,
    /// LABS-370 F-1-2): the live version found by [`live_version`], not
    /// a ts-probe — the maintained-statistics paths run inside the write
    /// window, where a rel created earlier in the same batch is
    /// invisible at `commit_ts` (its xmin > commit_ts) and one deleted
    /// in the batch keeps a live-looking pre-delete version. The type
    /// rides along because a re-used rel id can come back as a different
    /// rel type: a live-incidence match must hold on all three axes,
    /// not the endpoints alone.
    fn rel_live_identity(&self, id: RelId) -> Option<(NodeId, NodeId, u32)> {
        self.rels.get(&id).and_then(|vers| {
            live_version(vers.as_slice()).map(|v| (v.live.start, v.live.end, v.live.type_id))
        })
    }

    pub fn node_stub_at(&self, id: NodeId, ts: u64) -> Option<Node> {
        self.nodes.get(&id).and_then(|vers| {
            visible_version(vers.as_slice(), ts).map(|v| v.live.stub(id, &self.interner))
        })
    }

    pub fn rel_stub_at(&self, id: RelId, ts: u64) -> Option<Relationship> {
        self.rels
            .get(&id)
            .and_then(|vers| visible_version(vers.as_slice(), ts).map(|v| v.live.stub(id)))
    }

    pub fn node_prop_at(&self, id: NodeId, ts: u64, key: &str) -> Option<Value> {
        self.nodes.get(&id).and_then(|vers| {
            visible_version(vers.as_slice(), ts).and_then(|v| v.live.prop(&self.interner, key))
        })
    }

    pub fn rel_prop_at(&self, id: RelId, ts: u64, key: &str) -> Option<Value> {
        self.rels.get(&id).and_then(|vers| {
            visible_version(vers.as_slice(), ts)
                .and_then(|v| v.live.payload.get(&self.interner, key))
        })
    }

    pub fn node_prop_keys_at(&self, id: NodeId, ts: u64) -> Vec<String> {
        self.nodes
            .get(&id)
            .and_then(|vers| {
                vers.iter()
                    .rev()
                    .find(|v| visible(v.xmin, v.xmax, ts))
                    .map(|v| v.live.props.keys_as_strings(&self.interner))
            })
            .unwrap_or_default()
    }

    pub fn rel_prop_keys_at(&self, id: RelId, ts: u64) -> Vec<String> {
        self.rels
            .get(&id)
            .and_then(|vers| {
                vers.iter()
                    .rev()
                    .find(|v| visible(v.xmin, v.xmax, ts))
                    .map(|v| v.live.payload.keys(&self.interner))
            })
            .unwrap_or_default()
    }

    pub(crate) fn rel_visible_at(&self, id: RelId, ts: u64) -> bool {
        self.rels
            .get(&id)
            .is_some_and(|vers| visible_version(vers.as_slice(), ts).is_some())
    }

    pub fn node(&self, id: NodeId) -> Option<Node> {
        self.node_at(id, self.read_ts())
    }

    pub fn rel(&self, id: RelId) -> Option<Relationship> {
        self.rel_at(id, self.read_ts())
    }

    /// Account of the relationship version visible at the read timestamp.
    /// Uses the stored `Triple` / `Other` form, not the materialized map.
    fn visible_rel_account(&self, id: RelId) -> u64 {
        let ts = self.read_ts();
        self.rels
            .get(&id)
            .and_then(|vers| {
                vers.iter()
                    .rev()
                    .find(|v| visible(v.xmin, v.xmax, ts))
                    .map(|v| crate::memory::live_rel_account_live(&v.live))
            })
            .unwrap_or(0)
    }

    fn rel_form(&self, id: RelId) -> Option<crate::store::PayloadForm> {
        let ts = self.read_ts();
        self.rels.get(&id).and_then(|vers| {
            vers.iter()
                .rev()
                .find(|v| visible(v.xmin, v.xmax, ts))
                .map(|v| v.live.payload.form(&self.interner))
        })
    }

    pub fn node_exists(&self, id: NodeId) -> bool {
        self.node_visible_at(id, self.read_ts())
    }

    pub fn rel_exists(&self, id: RelId) -> bool {
        self.rel_visible_at(id, self.read_ts())
    }

    pub fn live_node_count(&self) -> usize {
        self.all_node_ids().len()
    }

    /// True inside a write that records redo; replay (`apply_redo_inner`) does not.
    fn recording(&self) -> bool {
        self.in_write.load(Ordering::Acquire) && self.record_redo.load(Ordering::Acquire)
    }

    fn push_redo(&self, op: RedoOp) {
        if self.recording() {
            self.pending_redo.lock().unwrap().push(op);
        }
    }

    fn bump_stats(&self, f: impl FnOnce(&mut QueryStats)) {
        f(&mut self.stats.lock().unwrap());
    }

    fn xmin_for_install(&self) -> u64 {
        if self.in_write.load(Ordering::Acquire) {
            self.write_ts.load(Ordering::Acquire)
        } else {
            let ts = self.commit_ts();
            if ts == 0 {
                self.set_commit_ts(1);
                1
            } else {
                ts
            }
        }
    }

    pub fn create_node(
        &self,
        labels: Vec<String>,
        properties: BTreeMap<String, Value>,
    ) -> Result<NodeId> {
        let id = self.alloc_node_id()?;
        self.install_node(id, labels, properties)?;
        Ok(id)
    }

    fn install_node(
        &self,
        id: NodeId,
        labels: Vec<String>,
        properties: BTreeMap<String, Value>,
    ) -> Result<()> {
        let next = checked_persisted_id(id, "node")?;
        self.next_node_id.fetch_max(next, Ordering::Relaxed);
        self.catalog
            .lock()
            .unwrap()
            .record_labels(labels.iter().cloned());
        self.bump_stats(|s| {
            s.nodes_created += 1;
            s.labels_added += labels.len() as i64;
            s.properties_set += properties.len() as i64;
        });
        if self.recording() {
            self.push_redo(RedoOp::create_node(
                id,
                std::sync::Arc::new(labels.clone()),
                properties.clone(),
            ));
        }
        let xmin = self.xmin_for_install();
        let live = LiveNode::from_parts(&self.interner, labels, properties);
        let charge =
            crate::memory::live_node_account_live(&live, &self.interner, &self.index_charge());
        // LABS-370 (v1 follow-up): the exact label counts follow every
        // create, before the version owns the live set.
        self.bump_label_counts(&live.labels, CountDelta::Add);
        {
            let mut vers = self.nodes.entry(id).or_default();
            vers.push(NodeVer {
                xmin,
                xmax: 0,
                live,
            });
            crate::memory::fit_reserved(&mut vers);
        }
        self.index_add(id);
        self.mark_node_dirty(id);
        // ADR-0063: lazy string postings — just dirty-mark built pairs, and
        // only after the version is installed (apply-before-dirty ordering).
        if let Some(vers) = self.nodes.get(&id) {
            if let Some(v) = vers.last() {
                self.string_dirty_mark_node(&v.live, id);
            }
        }
        self.charge(charge as i64 + crate::memory::node_chain_bytes(1) as i64);
        Ok(())
    }

    pub fn create_rel(
        &self,
        start: NodeId,
        end: NodeId,
        rel_type: String,
        properties: BTreeMap<String, Value>,
    ) -> Result<RelId> {
        self.validate_rel_endpoints(start, end)?;
        let id = self.alloc_rel_id()?;
        self.install_rel(id, start, end, rel_type, properties)?;
        Ok(id)
    }

    fn validate_rel_endpoints(&self, start: NodeId, end: NodeId) -> Result<()> {
        checked_persisted_id(start, "relationship start")?;
        checked_persisted_id(end, "relationship end")?;
        // Endpoint existence without materializing the endpoint nodes.
        let ts_now = self.read_ts();
        if !self.node_visible_at(start, ts_now) || !self.node_visible_at(end, ts_now) {
            return Err(cypherlite_core::Error::semantic(
                "cannot create relationship on missing node",
            ));
        }
        Ok(())
    }

    fn install_rel(
        &self,
        id: RelId,
        start: NodeId,
        end: NodeId,
        rel_type: String,
        properties: BTreeMap<String, Value>,
    ) -> Result<()> {
        let next = checked_persisted_id(id, "relationship")?;
        self.validate_rel_endpoints(start, end)?;
        self.next_rel_id.fetch_max(next, Ordering::Relaxed);
        self.catalog
            .lock()
            .unwrap()
            .record_rel_type(rel_type.clone());
        self.bump_stats(|s| {
            s.relationships_created += 1;
            s.properties_set += properties.len() as i64;
        });
        if self.recording() {
            let op = RedoOp::create_rel(id, start, end, rel_type.clone(), properties.clone());
            self.push_redo(op);
        }
        let xmin = self.xmin_for_install();
        let live = LiveRel::from_parts(&self.interner, start, end, &rel_type, properties);
        let charge = crate::memory::live_rel_account_live(&live);
        let ts = live.payload.ts(&self.interner);
        let out_before = adj_floor(&self.out_adj, start);
        let in_before = adj_floor(&self.in_adj, end);
        let degree_before = self.out_chunk_len(start, live.type_id);
        adj_insert(&self.out_adj, start, live.type_id, id, ts);
        adj_insert(&self.in_adj, end, live.type_id, id, ts);
        self.bump_rel_type(live.type_id, CountDelta::Add);
        self.bump_degree_histograms(start, live.type_id, degree_before);
        self.bump_pair_counts(start, live.type_id, end, CountDelta::Add);
        let adj = adj_floor(&self.out_adj, start) as i64 - out_before as i64
            + adj_floor(&self.in_adj, end) as i64
            - in_before as i64;
        {
            let mut vers = self.rels.entry(id).or_default();
            vers.push(RelVer {
                xmin,
                xmax: 0,
                live,
            });
            crate::memory::fit_reserved(&mut vers);
        }
        self.mark_rel_dirty(id);
        // ADR-0185: lazy relationship postings — dirty-mark built pairs, and
        // only after the version is installed (apply-before-dirty ordering).
        self.rel_index_add(id);
        self.charge(charge as i64 + crate::memory::rel_chain_bytes(1) as i64 + adj);
        Ok(())
    }

    pub fn named_indexes(&self) -> Vec<IndexDef> {
        self.catalog
            .lock()
            .unwrap()
            .indexes
            .values()
            .cloned()
            .collect()
    }

    /// Capture named indexes, constraints and their SHOW metadata at one
    /// admission boundary. Embedded callers hold the publication read guard
    /// across this call and `pin_now`; direct callers must serialize with
    /// their own graph writers in the same way.
    pub fn schema_snapshot(&self) -> SchemaSnapshot {
        self.schema_snapshot_with_hook(|| {})
    }

    fn schema_snapshot_with_hook(&self, after_catalog: impl FnOnce()) -> SchemaSnapshot {
        let (indexes, constraints) = {
            let catalog = self.catalog.lock().unwrap();
            (
                catalog.indexes.values().cloned().collect(),
                catalog.constraints.values().cloned().collect(),
            )
        };
        after_catalog();
        // Publication serialization makes these non-overlapping captures
        // (catalog above, then databases + theater in capture_show_schema)
        // atomic to embedded readers, without ever nesting catalog/theater
        // locks.
        let show = self.capture_show_schema();
        SchemaSnapshot {
            indexes,
            constraints,
            show,
        }
    }

    /// The committed SHOW metadata at this instant: the store-level
    /// databases table plus this graph's theater, under the theater lock.
    pub(crate) fn capture_show_schema(&self) -> ShowSchema {
        ShowSchema::capture(self.databases(), &self.theater.lock().unwrap())
    }

    pub fn named_constraints(&self) -> Vec<ConstraintDef> {
        self.catalog
            .lock()
            .unwrap()
            .constraints
            .values()
            .cloned()
            .collect()
    }

    pub fn install_named_constraint(&self, def: ConstraintDef) -> Result<()> {
        self.catalog.lock().unwrap().insert_constraint(def)
    }

    pub fn named_index(&self, name: &str) -> Option<IndexDef> {
        self.catalog.lock().unwrap().indexes.get(name).cloned()
    }

    /// By-name constraint probe, the constraint side of [`Self::named_index`].
    pub fn has_constraint(&self, name: &str) -> bool {
        self.catalog.lock().unwrap().constraints.contains_key(name)
    }

    pub fn named_index_by_schema(&self, label: &str, property: &str) -> Option<IndexDef> {
        self.catalog
            .lock()
            .unwrap()
            .index_by_schema(label, property)
            .cloned()
    }

    pub fn install_named_index(&self, def: IndexDef) -> cypherlite_core::Result<()> {
        self.catalog.lock().unwrap().insert_index(def)
    }

    pub fn node_value(&self, id: NodeId) -> Value {
        self.node(id).map(Value::from_node).unwrap_or(Value::Null)
    }

    pub fn rel_value(&self, id: RelId) -> Value {
        self.rel(id)
            .map(Value::from_relationship)
            .unwrap_or(Value::Null)
    }

    fn cow_node(&self, id: NodeId) -> Option<LiveNode> {
        cow(&self.write_ts, &self.nodes, id)
    }

    fn put_node(&self, id: NodeId, live: LiveNode) {
        if let Some(mut vers) = self.nodes.get_mut(&id) {
            if let Some(last) = vers.last_mut() {
                last.live = live;
            }
        }
    }

    fn cow_rel(&self, id: RelId) -> Option<LiveRel> {
        cow(&self.write_ts, &self.rels, id)
    }

    fn put_rel(&self, id: RelId, live: LiveRel) {
        if let Some(mut vers) = self.rels.get_mut(&id) {
            if let Some(last) = vers.last_mut() {
                last.live = live;
            }
        }
    }

    pub fn set_property_node(&self, id: NodeId, key: &str, value: Value) {
        let before = self.node(id);
        let before_len = self.node_chain_len(id);
        let mut op = None;
        let mut props_set = 0i64;
        if let Some(mut n) = self.cow_node(id) {
            let kid = self.interner.intern(key);
            if value.is_null() {
                n.props.remove(kid);
                op = Some(RedoOp::remove_node_prop(id, key.to_string()));
            } else {
                n.props.set(kid, value.clone());
                props_set = 1;
                op = Some(RedoOp::set_node_prop(id, key.to_string(), value.clone()));
            }
            self.put_node(id, n);
        }
        self.bump_stats(|s| s.properties_set += props_set);
        if let Some(op) = op {
            self.push_redo(op);
            self.mark_node_dirty(id);
        }
        // Dirty-mark only after the new version is installed — the ADR-0185
        // invariant: `index_add` marks every (label, key) pair the installed
        // version carries; a removal leaves stale postings, which the
        // residual re-verification filters.
        self.index_add(id);
        self.charge_node_after_edit(id, before.as_ref(), before_len);
    }

    pub fn set_property_rel(&self, id: RelId, key: &str, value: Value) {
        let before_bytes = self.visible_rel_account(id);
        let before_len = self.rel_chain_len(id);
        let mut op = None;
        let mut props_set = 0i64;
        if let Some(mut r) = self.cow_rel(id) {
            let old_ts = r.payload.ts(&self.interner);
            let start = r.start;
            let end = r.end;
            let type_id = r.type_id;
            if value.is_null() {
                r.payload.remove(&self.interner, key);
                op = Some(RedoOp::remove_rel_prop(id, key.to_string()));
            } else {
                r.payload.set(&self.interner, key, value.clone());
                props_set = 1;
                op = Some(RedoOp::set_rel_prop(id, key.to_string(), value.clone()));
            }
            let new_ts = r.payload.ts(&self.interner);
            self.put_rel(id, r);
            if old_ts != new_ts {
                adj_reposition(&self.out_adj, start, type_id, id, new_ts);
                adj_reposition(&self.in_adj, end, type_id, id, new_ts);
            }
            // Dirty-mark only after the new version is installed — the
            // ADR-0185 invariant: `rel_index_add` reads the installed
            // version's own type and keys; a removal's key is gone from the
            // payload, and its stale postings are filtered by the residual
            // re-verification.
            self.rel_index_add(id);
        }
        self.bump_stats(|s| s.properties_set += props_set);
        if let Some(op) = op {
            self.push_redo(op);
            self.mark_rel_dirty(id);
        }
        self.charge_rel_after_edit(id, before_bytes, before_len);
    }

    pub fn merge_properties_node(&self, id: NodeId, map: &BTreeMap<String, Value>) {
        for (k, v) in map {
            self.set_property_node(id, k, v.clone());
        }
    }

    /// Tombstone timestamp for a delete: the write ts while in a write,
    /// else the commit ts clamped to ≥ 1 so the tombstone is never the
    /// "no tombstone" sentinel 0. Both MVCC delete paths share this rule.
    fn tombstone_ts(&self) -> u64 {
        if self.in_write.load(Ordering::Acquire) {
            self.write_ts.load(Ordering::Acquire)
        } else {
            self.commit_ts().max(1)
        }
    }

    /// A node's current live labels in the interned domain (u32 ids) —
    /// the histogram's key domain, read from the live version (xmax == 0)
    /// rather than a snapshot-visible one: the histogram is maintained at
    /// create time, inside the writing transaction, where the just-created
    /// node is not yet visible at `commit_ts` (the bug the first version
    /// of this read had — every bump looped over zero labels). The
    /// `xmax == 0` convention matches the rebuild's live-rel scan.
    fn live_label_ids(&self, node: NodeId) -> Vec<u32> {
        self.nodes
            .get(&node)
            .and_then(|vers| {
                vers.iter()
                    .rev()
                    .find(|v| v.xmax == 0)
                    .map(|v| v.live.labels.clone())
            })
            .unwrap_or_default()
    }

    pub fn delete_rel(&self, id: RelId) {
        let ts = self.tombstone_ts();
        let mut found = false;
        if let Some(mut vers) = self.rels.get_mut(&id) {
            if let Some(v) = vers.iter_mut().rev().find(|v| v.xmax == 0) {
                let type_id = v.live.type_id;
                let (start, end) = (v.live.start, v.live.end);
                v.xmax = ts;
                self.bump_rel_type(type_id, CountDelta::Remove);
                // LABS-370: the pair counts are exact (the counting
                // operator answers from them), so a delete leaves its
                // (start-label, type, end-label) triples — the mirror of
                // the create-path bump.
                self.bump_pair_counts(start, type_id, end, CountDelta::Remove);
                found = true;
            }
        }
        if found {
            self.bump_stats(|s| s.relationships_deleted += 1);
            self.push_redo(RedoOp::delete_rel(id));
            self.mark_rel_dirty(id);
        }
    }

    pub fn delete_node(&self, id: NodeId, detach: bool) -> Result<()> {
        // The canonical incident scan: visibility-filtered at the read
        // timestamp and self-loop-deduplicated (graph/adjacency.rs).
        let incident = self.neighbors(id, cypherlite_ir::Direction::Both);
        if !detach && !incident.is_empty() {
            return Err(delete_connected_node_error());
        }
        for rid in incident {
            self.delete_rel(rid);
        }
        let ts = self.tombstone_ts();
        let mut found = false;
        if let Some(mut vers) = self.nodes.get_mut(&id) {
            if let Some(v) = vers.iter_mut().rev().find(|v| v.xmax == 0) {
                v.xmax = ts;
                // LABS-370 (v1 follow-up): the exact label counts follow
                // every delete — the label index keeps the tombstoned id
                // until GC, this map does not.
                self.bump_label_counts(&v.live.labels, CountDelta::Remove);
                found = true;
            }
        }
        if found {
            self.bump_stats(|s| s.nodes_deleted += 1);
            self.push_redo(RedoOp::delete_node(id));
            self.mark_node_dirty(id);
        }
        Ok(())
    }

    pub fn pull_identity_get(&self, source: &str, kind: PullKind, remote_id: &str) -> Option<i64> {
        self.pull_identity.get(source, kind, remote_id)
    }

    pub fn pull_identity_put(
        &self,
        source: String,
        kind: PullKind,
        remote_id: String,
        local_id: i64,
    ) {
        self.pull_identity.put(source, kind, remote_id, local_id);
    }

    pub fn drop_pull_identity(&self, kind: PullKind, local_id: i64) {
        self.pull_identity.remove(kind, local_id);
    }
}
/// Whether a graph-type spec could carry property default values
/// (CIP-262): any element type's property declares one, or any implied
/// label identifies another node element type (whose chain may inherit
/// one down). The one predicate both vintages of the statement
/// boundary's gate consult -- the committed theater
/// ([`Graph::has_property_defaults`]) and the scratch redo slice
/// ([`ExecGraph::has_property_defaults`]) -- so the two can never
/// answer the same yes/no differently.
pub(crate) fn spec_has_property_defaults(spec: &crate::graph_type::GraphTypeSpec) -> bool {
    let identifying: std::collections::BTreeSet<&str> = spec
        .node_types
        .iter()
        .map(|n| n.identifying_label.as_str())
        .collect();
    spec.node_types.iter().any(|n| {
        n.properties.iter().any(|p| p.default_value.is_some())
            || n.implied_labels
                .iter()
                .any(|l| identifying.contains(l.as_str()))
    }) || spec
        .edge_types
        .iter()
        .any(|e| e.properties.iter().any(|p| p.default_value.is_some()))
}

fn hop_counts(map: &CoreMap<NodeId, TypedAdj>, node: NodeId) -> Vec<(u32, usize)> {
    map.get(&node)
        .map(|adj| adj.chunks.iter().map(|(t, v)| (*t, v.len())).collect())
        .unwrap_or_default()
}

/// The admission estimates' fresh-name resolver (LABS-425, stellar round
/// 1): a name the interner does not know — a label or rel type the
/// not-yet-applied batch mints — resolves to a descending id below the
/// committed range, one id per distinct name. Both estimate terms
/// ([`Graph::estimate_adj_delta`] here,
/// [`Graph::estimate_stats_entry_growth`] in
/// graph/degree_stats_estimate.rs) resolved this ladder by hand before;
/// the descending-id collision invariant (fresh ids never collide with
/// committed ids or each other) now lives in exactly one place.
struct FreshIds {
    fresh: HashMap<String, u32>,
    next: u32,
}

impl FreshIds {
    fn new() -> Self {
        Self {
            fresh: HashMap::new(),
            next: u32::MAX,
        }
    }

    /// The interned id, else this batch's earlier mint, else a new
    /// descending id minted and memoized.
    fn resolve(&mut self, g: &Graph, name: &str) -> u32 {
        g.interner
            .get_id(name)
            .or_else(|| self.fresh.get(name).copied())
            .unwrap_or_else(|| {
                let id = self.next;
                self.next = self.next.saturating_sub(1);
                self.fresh.insert(name.to_string(), id);
                id
            })
    }
}

fn adj_floor(map: &CoreMap<NodeId, TypedAdj>, node: NodeId) -> u64 {
    map.get(&node).map(|adj| adj.floor_bytes()).unwrap_or(0)
}

fn bump_adj(
    sim: &mut HashMap<NodeId, Vec<(u32, usize)>>,
    node: NodeId,
    type_id: u32,
    init: impl FnOnce() -> Vec<(u32, usize)>,
) -> i64 {
    let chunks = sim.entry(node).or_insert_with(init);
    let before =
        crate::memory::adjacency_bytes(&chunks.iter().map(|(_, n)| *n).collect::<Vec<_>>());
    if let Some(slot) = chunks.iter_mut().find(|c| c.0 == type_id) {
        slot.1 += 1;
    } else {
        chunks.push((type_id, 1));
    }
    let after = crate::memory::adjacency_bytes(&chunks.iter().map(|(_, n)| *n).collect::<Vec<_>>());
    after as i64 - before as i64
}
impl Graph {
    pub fn all_rel_ids(&self) -> Vec<RelId> {
        self.all_rel_ids_at(self.read_ts())
    }

    pub fn all_rel_ids_at(&self, ts: u64) -> Vec<RelId> {
        let mut ids: Vec<_> = self
            .rels
            .iter()
            .map(|e| *e.key())
            .filter(|id| self.rel_visible_at(*id, ts))
            .collect();
        ids.sort();
        ids
    }

    pub fn all_node_ids(&self) -> Vec<NodeId> {
        self.all_node_ids_at(self.read_ts())
    }

    pub fn all_node_ids_at(&self, ts: u64) -> Vec<NodeId> {
        let mut ids: Vec<_> = self
            .nodes
            .iter()
            .map(|e| *e.key())
            .filter(|id| self.node_visible_at(*id, ts))
            .collect();
        ids.sort();
        ids
    }

    pub(crate) fn node_visible_at(&self, id: NodeId, ts: u64) -> bool {
        self.nodes
            .get(&id)
            .is_some_and(|vers| visible_version(vers.as_slice(), ts).is_some())
    }
}

#[cfg(test)]
mod tests;
