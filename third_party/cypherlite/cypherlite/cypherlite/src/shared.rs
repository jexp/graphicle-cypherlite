//! The store handle connections share: [`Shared`], the [`GraphCell`]
//! publication contract it holds, and the guards that read it.

#[cfg(not(target_arch = "wasm32"))]
use cypherlite_storage::WriterLease;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use cypherlite_core::{ce_messages, Namespace};
use cypherlite_executor::Graph;
use cypherlite_executor::RedoOp;
use cypherlite_storage::{StoreTarget, Wal};

use crate::checkpoint::{self, rotate_for_mode, schedule_checkpoint, try_begin_checkpoint};
use crate::commit::{CommitTimingBuf, GroupCommit};
#[cfg(test)]
use crate::follow;
use crate::{cdc, ontology, token_budget, DurabilityMode, Error, Result, ThreadStop};

/// `Graph` behind a cell so a follower can replace it while holding the
/// exclusive publication lock (ADR-0137). Reads use [`std::ops::Deref`].
///
/// `deref` is only used under `read_guard` or a snapshot pin taken while
/// that guard was held. A pin outlives `read_guard`, and the `&Graph` from
/// `deref` may outlive the guard for as long as the pin keeps this cell
/// from being replaced.
pub(crate) struct GraphCell(std::cell::UnsafeCell<Graph>);

impl GraphCell {
    pub(crate) fn new(graph: Graph) -> Self {
        Self(std::cell::UnsafeCell::new(graph))
    }

    /// Replace the live graph. Caller holds the exclusive publication lock
    /// and `snapshot_pinned()` is false: the write lock alone is not
    /// enough, because a pin outlives `read_guard`.
    fn replace(&self, graph: Graph) {
        // SAFETY: the exclusive publication lock is held and
        // `snapshot_pinned()` is false, so no `read_guard`, live pin, or
        // `apply_redo` can observe this write.
        unsafe { *self.0.get() = graph };
    }
}

impl std::ops::Deref for GraphCell {
    type Target = Graph;

    fn deref(&self) -> &Graph {
        // SAFETY: callers use `deref` only under `read_guard` or a live
        // snapshot pin. `replace` waits until `snapshot_pinned()` is false
        // and holds the publication write lock, which excludes both, so
        // this reference stays on the installed graph.
        unsafe { &*self.0.get() }
    }
}

// SAFETY: `Graph` is Sync. The cell is written only by `replace`, which
// requires the exclusive publication lock and `snapshot_pinned() == false`.
// That excludes every `deref` under `read_guard` or a live pin.
unsafe impl Sync for GraphCell {}

/// The follower's health: clean, a transient failure that may be retried and
/// cleared, or a failed apply whose reload also failed. The terminal variant
/// owns its error, so "permanently failed" is a variant rather than a second
/// flag a writer could disagree with.
pub(crate) enum FollowHealth {
    /// Clean: polls may retry and readers see no follow error.
    Healthy,
    /// The last transient failure; retrying and clearing are allowed.
    Transient(Error),
    /// The failed-apply-and-failed-reload state: no retry, no clear.
    Failed(Error),
}

impl FollowHealth {
    fn is_failed(&self) -> bool {
        matches!(self, Self::Failed(_))
    }

    fn error(&self) -> Option<&Error> {
        match self {
            Self::Healthy => None,
            Self::Transient(error) | Self::Failed(error) => Some(error),
        }
    }

    /// Record a transient failure. A failed follower's error is terminal and
    /// is never downgraded or overwritten.
    fn record_transient(&mut self, error: Error) {
        if !self.is_failed() {
            *self = Self::Transient(error);
        }
    }

    /// Clear a transient failure. A failed follower cannot be cleared.
    fn clear_transient(&mut self) {
        if !self.is_failed() {
            *self = Self::Healthy;
        }
    }

    /// Enter the terminal failed-apply-and-reload state.
    fn fail_permanently(&mut self, error: Error) {
        *self = Self::Failed(error);
    }
}

/// The knobs resolved once at open and never mutated afterwards: the
/// open mode, the durability mode, the memory limit, the bulk-write
/// batching thresholds (ADR-0209), and the peak-token admission budget.
/// One named cluster, so the open path states each knob once and a reader
/// finds them together instead of loose across `Shared`.
pub(crate) struct OpenResolved {
    /// The open mode this handle was *opened* with (ADR-0136, ADR-0137).
    /// Fixed for life: it names what the handle may ever become, not what
    /// it is right now. [`Shared::writable`] is the live, mutable fact —
    /// `read_only` stays `true` for a `Follow` handle across a later
    /// `promote`/`demote` cycle (ADR-0440), because only a `Follow`-opened
    /// handle is allowed to promote, and a `ReadOnly` one never is.
    pub(crate) read_only: bool,
    /// Whether this handle was opened as a WAL follower (ADR-0137) —
    /// the one fact [`Database::promote`] checks that `read_only` alone
    /// cannot distinguish from [`OpenMode::ReadOnly`].
    pub(crate) opened_as_follow: bool,
    pub(crate) durability_mode: DurabilityMode,
    /// 0 = unlimited (ADR-0042).
    pub(crate) logical_graph_size_limit: u64,
    /// Opt-in peak-token admission. Off, reservations do not refuse.
    pub(crate) budget: Arc<token_budget::TokenBudget>,
    /// ADR-0209 bulk-write batching knobs, resolved once at open from
    /// `CYPHERLITE_BULK_BATCH_THRESHOLD` / `CYPHERLITE_BULK_BATCH_BUDGET`.
    pub(crate) bulk_batch_threshold: u64,
    pub(crate) bulk_batch_budget: u64,
    /// The checkpoint timer period this handle was opened with (ADR-0041).
    /// Resolved once, like every other field here, so `promote` (ADR-0440)
    /// can start the same timer a bootstrap writable open would have
    /// started, instead of re-reading the environment.
    pub(crate) checkpoint_interval: Duration,
    /// Gap recovery for a follower (ADR-0441). Fixed for the life of the
    /// handle, like the open mode.
    pub(crate) follow_gap: crate::FollowGapPolicy,
}

/// One background worker's stop signal and join handle, together so a
/// caller can never hold one without the other (ADR-0440): [`Shared::demote`]
/// and [`Shared::stop_background`] signal-then-join through the same pair,
/// and [`Database::promote`] / open-time spawn install both atomically.
pub(crate) type BgWorker = Mutex<Option<(Arc<ThreadStop>, JoinHandle<()>)>>;

/// Signal `worker`'s stop flag if a thread is currently installed. Shared by
/// [`Shared::signal_background`] (every worker) and [`Shared::stop_and_join`]
/// (the two joined at `Database::drop`).
pub(crate) fn signal(worker: &BgWorker) {
    let guard = worker.lock().unwrap_or_else(|err| err.into_inner());
    if let Some((stop, _)) = guard.as_ref() {
        stop.signal();
    }
}

/// Signal then join the worker installed in `worker`, clearing the slot.
/// No-op when nothing is installed. Used for the checkpoint and durability
/// timers, which `Database::drop` always waits for; the follow thread is
/// signaled through [`Shared::signal_background`] alone and never joined
/// here — a paused follow test gate would stall the join (unchanged from
/// the pre-ADR-0440 behavior, see `follow::spawn`).
pub(crate) fn stop_and_join(worker: &BgWorker) {
    signal(worker);
    let handle = worker
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .take()
        .map(|(_, h)| h);
    if let Some(handle) = handle {
        let _ = handle.join();
    }
}

pub(crate) struct Shared {
    /// The `neo4j` namespace: the live graph the WAL and snapshots persist
    /// (ADR-0116 §10), behind a cell so a follower can replace it (ADR-0137).
    pub(crate) graph: GraphCell,
    /// The `system` namespace slot (ADR-0116 §10): an empty graph. The
    /// `system_ontologies` namespace has no slot — it is listed but not
    /// queryable (ADR-0229 §1/§3), every non-administration statement
    /// routed to it failing in `Connection::namespace_gate_error`, so no
    /// graph can ever be installed there.
    ///
    /// This slot is never replaced by a follow reload, so it is also the
    /// store's surviving handle to the one `StoreServices` bundle: a
    /// replacement `neo4j` graph re-points at it with
    /// `Graph::share_services_from` instead of re-copying store-level
    /// fields. That bundle mixes configuration the store owns (the
    /// procedure registry, the LOAD CSV policy, the resolved preview set)
    /// with durable state restored from a checkpoint (the databases
    /// catalog, ADR-0276). The reload therefore first installs the catalog
    /// the checkpoint load restored into this bundle with
    /// `Graph::install_databases_from`, and only then shares it; a plain
    /// share would keep this slot's stale in-memory catalog over the
    /// checkpoint's newer one. Unlike the `GraphCell` above, this field is
    /// never replaced, so reading it needs no publication read guard — and
    /// the recovery reload, which `read_guard`'s health check would refuse,
    /// can still reach the bundle.
    pub(crate) system: Graph,
    /// The resolved open-time knobs.
    pub(crate) resolved: OpenResolved,
    /// The live writable/follower fact (ADR-0440). Starts at
    /// `!resolved.read_only`; `Database::promote` sets it, `demote` clears
    /// it. Every write path's `ensure_writable` reads this, not
    /// `resolved.read_only` — that field is the fixed open-time mode.
    pub(crate) writable: AtomicBool,
    /// Set for the span of `Database::demote` between closing new-write
    /// admission and the writer lease actually releasing (ADR-0440): a
    /// write that arrives in that window gets the retryable
    /// `writer-draining` `ResourceLimit`, never a plain `Capacity`-style
    /// refusal, so a client's retry lands on the newly promoted writer
    /// instead of failing the statement outright.
    pub(crate) draining: AtomicBool,
    /// Serializes [`Database::promote`] and [`Database::demote`] so one
    /// role change finishes, including its follow-thread repair, before
    /// the next begins.
    pub(crate) role: Mutex<()>,
    /// The one `.cypherlite.writer` lease (ADR-0136). A writable native
    /// open installs it here; [`Database::promote`] installs the same
    /// type; [`Database::demote`] drops it. `None` while this handle is a
    /// follower. Independent of the shared identity lock the store's
    /// `Vfs` holds for the process's whole life.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) writer_lease: Mutex<Option<WriterLease>>,
    pub(crate) store: StoreTarget,
    pub(crate) persist_on_drop: AtomicBool,
    pub(crate) wal: Mutex<Wal>,
    /// Serializes commit/replica application and poison publication with reads.
    pub(crate) publication: RwLock<()>,
    pub(crate) poisoned: AtomicBool,
    pub(crate) safe_lsn: AtomicU64,
    pub(crate) safe_ts: AtomicU64,
    #[cfg(test)]
    pub(crate) fail_after_append: AtomicBool,
    #[cfg(test)]
    pub(crate) fail_after_sync: AtomicBool,
    #[cfg(test)]
    pub(crate) read_pause: Mutex<Option<Arc<ReadPause>>>,
    pub(crate) group: Mutex<GroupCommit>,
    pub(crate) next_txn_id: AtomicU64,
    pub(crate) next_node_id: Arc<AtomicU64>,
    pub(crate) next_rel_id: Arc<AtomicU64>,
    /// `CYPHERLITE_COMMIT_TIMING=1`: record encode/write, sync_all, apply_redo.
    pub(crate) timing_on: AtomicBool,

    pub(crate) timing_dumped: AtomicBool,
    pub(crate) timing: Mutex<CommitTimingBuf>,
    pub(crate) checkpoint_busy: Mutex<checkpoint::CheckpointState>,
    pub(crate) checkpoint_cv: Condvar,
    /// The checkpoint timer, when one is running. Installed at open for a
    /// writable store, or by `Database::promote`; stopped and joined by
    /// `Database::demote` or `Database::drop` (ADR-0440).
    pub(crate) timer: BgWorker,
    /// The 1s WAL-sync timer (`DurabilityMode::EverySecond`), same lifecycle
    /// as `timer`.
    pub(crate) durability_timer: BgWorker,
    pub(crate) durability_error: Mutex<Option<Error>>,
    /// CDC slots + apply-only flag (ADR-0045).
    pub(crate) cdc: Mutex<cdc::CdcState>,
    /// The store's current ontology document, verbatim (ADR-0120 §1);
    /// `None` when the store holds no ontology. Shared as an `Arc<str>`
    /// handle so the per-statement capture is a pointer clone.
    pub(crate) ontology: ontology::OntologySlot,
    /// The follower's single health fact (ADR-0137): the last transient
    /// error while retrying, or the permanent failed-apply-and-reload error
    /// that fences every later poll until the process reopens.
    pub(crate) follow_health: Mutex<FollowHealth>,
    /// The follow thread, when one is running. Installed at open for
    /// `OpenMode::Follow`, or restarted by `Database::demote`; stopped
    /// (never joined here — see `stop_and_join`'s doc) by `Database::drop`,
    /// and stopped-and-joined by `Database::promote` before it takes the
    /// writer lease (ADR-0440).
    pub(crate) follow_ctl: BgWorker,
    /// Timer workers and one-shot rotation checkpoints. Joined from
    /// [`crate::Database`]'s drop before this `Arc` can release the store lock.
    pub(crate) background: Mutex<Vec<JoinHandle<()>>>,
    #[cfg(test)]
    pub(crate) follow_test: Mutex<follow::FollowTest>,
    #[cfg(test)]
    pub(crate) follow_test_cv: Condvar,
}

/// The loaded graph's next node/relationship ids as the store's `u64`
/// counters, refusing a negative persisted value — the same invariant
/// `check_loaded_graph` enforces, owned where the conversion happens.
fn loaded_graph_counters(graph: &Graph) -> Result<(u64, u64)> {
    let node = u64::try_from(graph.next_node_id())
        .map_err(|_| Error::internal("loaded graph's next node id is negative"))?;
    let rel = u64::try_from(graph.next_rel_id())
        .map_err(|_| Error::internal("loaded graph's next relationship id is negative"))?;
    Ok((node, rel))
}

pub(crate) fn check_loaded_graph(graph: &Graph) -> Result<()> {
    if graph.commit_ts() == u64::MAX || graph.next_node_id() < 0 || graph.next_rel_id() < 0 {
        return Err(Error::storage(
            "persisted timestamp or next ID is unsupported",
        ));
    }
    Ok(())
}

/// The replica write path's CDC fence (ADR-0045): a database with a CDC
/// source slot never applies replicated records. The check-then-act is
/// race-free because the caller holds the publication write guard across
/// it and `finish_applied_commit` — a slot registration takes the
/// publication read guard (`cdc_slot_admission`), which cannot cross the
/// write guard, so a registration can never interleave between the check
/// and the applied commit.
pub(crate) fn ensure_no_cdc_source_slot(shared: &Shared) -> Result<()> {
    let cdc = shared.cdc.lock()?;
    if cdc.has_source_slot() {
        return Err(Error::semantic(
            "database has a CDC source slot; cannot apply replicated records",
        ));
    }
    Ok(())
}

/// The single write-admission gate every commit path calls before it
/// enqueues anything (ADR-0042, ADR-0440): draining refuses retryably, a
/// non-writable handle (frozen read-only, or a follower that was never
/// promoted) refuses non-retryably as before.
pub(crate) fn ensure_writable(shared: &Shared) -> Result<()> {
    if shared.draining.load(Ordering::Acquire) {
        return Err(Error::resource_limit(Error::WRITER_DRAINING, 0, true));
    }
    if shared.writable.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err(ce_messages::read_only_write())
    }
}

/// The resolved open-time values `Shared` cannot derive from the loaded
/// graph/WAL pair. Resolution (env, VFS capability, timer policy) stays in
/// the open path; the type owns how those values become its initial state.
pub(crate) struct SharedOpenState {
    /// The resolved open-time knob cluster, built once by the open path —
    /// the knob vocabulary is stated there, not re-spelled here.
    pub(crate) resolved: OpenResolved,
    pub(crate) cdc: cdc::CdcState,
    pub(crate) ontology: Option<String>,
    pub(crate) commit_timing: bool,
}

impl Shared {
    /// The type's birth-state policy: build the `system` slot, derive the
    /// safe horizon, and the counters from the loaded pair and the
    /// resolved open-time knobs. Every background worker starts empty
    /// (ADR-0440): the open path decides, after this call, whether to
    /// install a checkpoint timer, a durability timer, or a follow thread
    /// — the same decision `Database::promote` / `demote` later repeat, so
    /// it lives once in `open.rs` rather than split between here and there.
    pub(crate) fn new(
        graph: Graph,
        store: StoreTarget,
        wal: Wal,
        open: SharedOpenState,
    ) -> Result<Self> {
        let mut system = Graph::default();
        system.set_namespace(Namespace::System);
        system.share_services_from(&graph);
        let writable = !open.resolved.read_only;
        let safe_lsn = wal.lsn;
        let safe_ts = graph.commit_ts();
        let next_txn_id = AtomicU64::new(graph.commit_ts().saturating_add(1));
        let (next_node_id, next_rel_id) = loaded_graph_counters(&graph)?;
        Ok(Self {
            graph: GraphCell::new(graph),
            system,
            resolved: open.resolved,
            writable: AtomicBool::new(writable),
            draining: AtomicBool::new(false),
            role: Mutex::new(()),
            #[cfg(not(target_arch = "wasm32"))]
            writer_lease: Mutex::new(None),
            store,
            persist_on_drop: AtomicBool::new(writable),
            wal: Mutex::new(wal),
            publication: RwLock::new(()),
            poisoned: AtomicBool::new(false),
            safe_lsn: AtomicU64::new(safe_lsn),
            safe_ts: AtomicU64::new(safe_ts),
            #[cfg(test)]
            fail_after_append: AtomicBool::new(false),
            #[cfg(test)]
            fail_after_sync: AtomicBool::new(false),
            #[cfg(test)]
            read_pause: Mutex::new(None),
            group: Mutex::new(GroupCommit::default()),
            next_txn_id,
            next_node_id: Arc::new(AtomicU64::new(next_node_id)),
            next_rel_id: Arc::new(AtomicU64::new(next_rel_id)),
            timing_on: AtomicBool::new(open.commit_timing),

            timing_dumped: AtomicBool::new(false),
            timing: Mutex::new(CommitTimingBuf::default()),
            checkpoint_busy: Mutex::new(checkpoint::CheckpointState::default()),
            checkpoint_cv: Condvar::new(),
            timer: Mutex::new(None),
            durability_timer: Mutex::new(None),
            durability_error: Mutex::new(None),
            cdc: Mutex::new(open.cdc),
            ontology: ontology::OntologySlot::new(open.ontology),
            follow_health: Mutex::new(FollowHealth::Healthy),
            follow_ctl: Mutex::new(None),
            background: Mutex::new(Vec::new()),
            #[cfg(test)]
            follow_test: Mutex::new(follow::FollowTest::default()),
            #[cfg(test)]
            follow_test_cv: Condvar::new(),
        })
    }

    /// Complete a fully synced and applied group or replicated record while
    /// WAL and publication are still serialized. A failed rotation makes the
    /// whole attempted publication uncertain, so the prior safe horizon stays
    /// frozen even though its redo may already be in memory and on disk.
    pub(crate) fn finish_applied_commit(
        self: &Arc<Self>,
        mut wal: MutexGuard<'_, Wal>,
        publication: checkpoint::Publication<'_>,
    ) -> Result<()> {
        let commit_ts = self.graph.commit_ts();
        self.next_node_id
            .fetch_max(self.graph.next_node_id() as u64, Ordering::Relaxed);
        self.next_rel_id
            .fetch_max(self.graph.next_rel_id() as u64, Ordering::Relaxed);
        self.next_txn_id
            .fetch_max(commit_ts.saturating_add(1), Ordering::Relaxed);

        if wal.should_checkpoint() {
            if let Err(cause) = rotate_for_mode(&mut wal, self.resolved.durability_mode) {
                return Err(self.poison(&cause));
            }
        }
        self.publish_safe_horizon(wal.lsn, commit_ts);
        let pin =
            (wal.has_closed_segments() || wal.should_checkpoint()).then(|| wal.checkpoint_pin());
        drop(wal);
        drop(publication);
        // The admission attempt is a side effect, so it runs in the body
        // rather than inside `Option::filter`, whose predicates read pure.
        if let Some((lsn, snapshot_ts)) = pin {
            if try_begin_checkpoint(self) {
                schedule_checkpoint(self, lsn, snapshot_ts);
            }
        }
        Ok(())
    }

    pub(crate) fn publish_safe_horizon(&self, lsn: u64, ts: u64) {
        self.safe_lsn.store(lsn, Ordering::Release);
        self.safe_ts.store(ts, Ordering::Release);
    }

    /// Install a graph loaded beside readers as one complete operation.
    /// Caller holds the publication write lock and `snapshot_pinned()` is
    /// false. The WAL lock is taken first and poison-mapped, before any
    /// mutation, and the service-bundle carry, the counters, the safe
    /// horizon, the graph, and the WAL swap all run under that guard: a
    /// failed WAL lock aborts the swap with the previous graph, WAL,
    /// services, and horizon intact, never a half-swapped store. The
    /// publication-write → WAL nesting matches every other taker
    /// (`flush_group`, `apply_replicated`, the CDC readers); no path takes
    /// the two locks in the other order.
    ///
    /// Owning the bundle carry here keeps the swap atomic across modules:
    /// the reload caller hands over the pair it opened and `Shared` alone
    /// decides how the surviving `system` slot and the replacement bundle
    /// meet (ADR-0116 §10, ADR-0276).
    /// `_publication` is the exclusive publication lock the swap needs: the
    /// `GraphCell` write and the `system` bundle mutation are safe only
    /// while it is held, so taking it as a parameter makes the compiler
    /// enforce that at every call site (the dynamic `snapshot_pinned() ==
    /// false` half of the contract stays the caller's loop).
    pub(crate) fn install_reloaded(
        &self,
        _publication: &checkpoint::Publication<'_>,
        mut graph: Graph,
        mut wal: Wal,
        ontology: Option<String>,
    ) -> Result<()> {
        // The horizon and the counters come from the very objects being
        // installed, so deriving them here (before the moves consume the
        // values) is the only honest source — a caller cannot pass a
        // mismatched pair.
        let commit_ts = graph.commit_ts();
        let lsn = wal.lsn;
        let (next_node_id, next_rel_id) = loaded_graph_counters(&graph)?;
        let mut slot = self.wal.lock()?;
        // ADR-0116 §10 / ADR-0371 / ADR-0378 / ADR-0276: the loaded graph
        // carries a fresh empty bundle. Re-point it at the store's one bundle
        // — carried by the `system` namespace graph, the slot a reload never
        // replaces — instead of copying store-level fields into it. The
        // bundle is frozen once shared (`configure_*` panics after
        // `share_services_from`), so this restores the procedure registry,
        // the LOAD CSV policy, and the resolved preview set exactly. Its
        // databases catalog is durable state, not configuration:
        // `open_store_readonly` already restored the checkpoint's table into
        // the loaded graph's own bundle, so install that into the surviving
        // bundle first or the share would keep the store's stale copy.
        self.system.install_databases_from(&graph);
        graph.share_services_from(&self.system);
        // ADR-0120 §1/§4: the checkpoint's ontology document is store-level
        // durable state like the catalog above — install it with the swap,
        // or a follower parked across a writer's `db.ontology.set` +
        // checkpoint serves the stale document after the gap reload. The
        // canonical installer moves the document and the WAL's segment-magic
        // flag together, so the reloaded WAL cannot disagree with the
        // document it serves.
        ontology::install_ontology_state(self, &mut wal, ontology);
        self.next_node_id.store(next_node_id, Ordering::Relaxed);
        self.next_rel_id.store(next_rel_id, Ordering::Relaxed);
        self.next_txn_id
            .store(commit_ts.saturating_add(1), Ordering::Relaxed);
        self.publish_safe_horizon(lsn, commit_ts);
        self.graph.replace(graph);
        *slot = wal;
        Ok(())
    }

    /// The one capacity-admission policy of the redo path (ADR-0042).
    /// Called under the caller's publication lock: it reads the live graph
    /// through the `GraphCell` (`estimate_redo_delta` and
    /// `total_used_bytes`). 0
    /// means unlimited; otherwise the redo delta of `ops` is estimated and
    /// checked against the live usage plus `reserved` (the bytes a
    /// group-commit batch has already admitted). Returns the new reserved
    /// total — the caller that admits a batch carries it forward; a
    /// single-record caller ignores it.
    pub(crate) fn admit_redo_capacity(&self, ops: &[RedoOp], reserved: u64) -> Result<u64> {
        let limit = self.resolved.logical_graph_size_limit;
        if limit == 0 {
            return Ok(reserved);
        }
        let delta = self.graph.estimate_redo_delta(ops);
        // A net-freeing batch (`delta <= 0`, e.g. a DROP INDEX that
        // re-accounts retained versions at a smaller charge) admits nothing:
        // the cast below is only sound for a positive delta.
        if delta <= 0 {
            return Ok(reserved);
        }
        let used = self.total_used_bytes().saturating_add(reserved);
        if used.saturating_add(delta as u64) > limit {
            return Err(Error::capacity(used, limit));
        }
        Ok(reserved.saturating_add(delta as u64))
    }

    pub(crate) fn read_guard(&self) -> Result<RwLockReadGuard<'_, ()>> {
        let guard = self.publication.read()?;
        self.check_healthy()?;
        Ok(guard)
    }

    /// The store's current commit timestamp, read under the publication
    /// read guard. This is THE way to read `commit_ts()` from a facade
    /// path outside a commit pipeline: the `GraphCell` deref contract
    /// requires the guard (a concurrent `--follow` reload may `replace`
    /// the graph mid-deref otherwise, ADR-0137). The guard is scoped to
    /// this one read and released on return — callers must not hold a
    /// read guard across `try_wal_commit`: that path's flush takes the
    /// publication WRITE lock (`flush_group` → `begin_publication`),
    /// which a held read guard would deadlock on the same thread (the
    /// `commit_optimistic` scoped-guard rule, commit.rs).
    pub(crate) fn commit_ts(&self) -> Result<u64> {
        let _read = self.read_guard()?;
        Ok(self.graph.commit_ts())
    }
    #[cfg(test)]
    pub(crate) fn pause_admitted_read(&self) {
        let pause = self.read_pause.lock().unwrap().take();
        if let Some(pause) = pause {
            pause.entered.wait();
            pause.release.wait();
        }
    }
    /// Read guard for the follow thread's own apply. It skips the stored
    /// error check: that state holds the previous poll's error, and the
    /// apply that clears it must not be refused because of it. WAL
    /// poisoning still refuses the apply.
    pub(crate) fn follow_apply_guard(&self) -> Result<RwLockReadGuard<'_, ()>> {
        let guard = self.publication.read()?;
        self.check_poisoned()?;
        Ok(guard)
    }
    /// The one spelling of a `follow_health` **write** lock: a poisoned
    /// guard is recovered, because this state fences the follower and a
    /// panicked recorder must not unlock it. The reader-side health gate
    /// (`check_healthy`) deliberately poison-maps instead: a reader cannot
    /// prove a torn state is safe to trust.
    fn follow_health_guard(&self) -> MutexGuard<'_, FollowHealth> {
        self.follow_health
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    /// Whether a failed apply's reload also failed: the poll loop must not
    /// try again, and the stored error must not be cleared. The lock is
    /// recovered through a poisoned guard, matching the writers: this is
    /// the state that fences the loop, so a panicked recorder must not
    /// unlock it.
    pub(crate) fn follow_failed(&self) -> bool {
        self.follow_health_guard().is_failed()
    }

    /// A [`crate::FollowGapPolicy::Yield`] follower latched `follow-gap`
    /// and will not load another graph itself (ADR-0441).
    pub(crate) fn needs_reopen(&self) -> bool {
        matches!(
            self.follow_health_guard().error(),
            Some(Error::ResourceLimit {
                name: Error::FOLLOW_GAP,
                ..
            })
        )
    }
    /// Record a transient follow failure. A permanently failed follower's
    /// error is never downgraded or overwritten — `record_permanent_failure`
    /// owns that record — so the same failure never lands twice.
    pub(crate) fn record_failure(&self, error: Error) {
        self.follow_health_guard().record_transient(error);
    }
    /// Record the failed-apply-and-failed-reload state: one locked write
    /// carries both the error and the no-retry fact.
    pub(crate) fn record_permanent_failure(&self, error: Error) {
        self.follow_health_guard().fail_permanently(error);
    }
    /// Clear a transient failure after a clean poll. A permanent failure
    /// cannot be cleared: its own variant decides, never a caller-side flag.
    pub(crate) fn clear_transient_failure(&self) {
        self.follow_health_guard().clear_transient();
    }

    /// Health gate for readers: the stored follow error, then the WAL
    /// poison flag. A poisoned `follow_health` lock — a thread panicked
    /// while holding it, plausibly while recording the very error it was
    /// writing — is itself an error, never a silent clean bill of health:
    /// the lock is poison-mapped like the sibling guards, not skipped.
    pub(crate) fn check_healthy(&self) -> Result<()> {
        let health = self.follow_health.lock()?;
        if let Some(err) = health.error() {
            return Err(err.clone());
        }
        self.check_poisoned()
    }
    pub(crate) fn check_poisoned(&self) -> Result<()> {
        if self.poisoned.load(Ordering::Acquire) {
            Err(Error::storage(format!(
                "uncertain WAL outcome after safe LSN {} / timestamp {}; close and reopen",
                self.safe_lsn.load(Ordering::Acquire),
                self.safe_ts.load(Ordering::Acquire)
            )))
        } else {
            Ok(())
        }
    }

    /// Live graph + index logical bytes across every namespace; the memory
    /// limit covers them all (ADR-0042, ADR-0116 §10).
    pub(crate) fn total_used_bytes(&self) -> u64 {
        self.graph
            .used_bytes()
            .saturating_add(self.system.used_bytes())
    }

    /// Called under the exclusive publication lock, before releasing the WAL.
    pub(crate) fn poison(&self, cause: &Error) -> Error {
        self.poisoned.store(true, Ordering::Release);
        self.persist_on_drop.store(false, Ordering::Release);
        signal(&self.timer);
        Error::storage(format!(
            "uncertain WAL outcome after safe LSN {} / timestamp {} ({cause}); close and reopen",
            self.safe_lsn.load(Ordering::Acquire),
            self.safe_ts.load(Ordering::Acquire)
        ))
    }
}

#[cfg(test)]
pub(crate) struct ReadPause {
    pub(crate) entered: std::sync::Barrier,
    pub(crate) release: std::sync::Barrier,
}

impl Shared {
    /// Signal every background worker, then join the checkpoint and
    /// durability timers and the one-shot dump threads in `background`.
    /// The follow thread is only ever signaled through this, never joined
    /// here: a paused follow test gate waits on its own gate, and joining
    /// it from `Database::drop` would stall until that gate opens
    /// (unchanged from before ADR-0440; `Database::promote` is the one
    /// caller that does join it, outside of `drop`).
    pub(crate) fn stop_background(&self) {
        self.signal_background();
        stop_and_join(&self.timer);
        stop_and_join(&self.durability_timer);
        let handles = std::mem::take(
            &mut *self
                .background
                .lock()
                .unwrap_or_else(|err| err.into_inner()),
        );
        for handle in handles {
            let _ = handle.join();
        }
    }

    /// Signal every background worker's stop flag without joining any of
    /// them (ADR-0440). The one call both `stop_background` (which then
    /// joins the two timers) and `Drop for Shared` (which joins nothing —
    /// by the time that runs, `stop_background` already has, on every path
    /// that reaches it through `Database::drop`) share, so the three-way
    /// signal lives once instead of twice.
    pub(crate) fn signal_background(&self) {
        signal(&self.timer);
        signal(&self.durability_timer);
        signal(&self.follow_ctl);
    }

    pub(crate) fn track_background(&self, handle: Option<JoinHandle<()>>) {
        self.background
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .extend(handle);
    }
}

#[cfg(test)]
mod tests;
