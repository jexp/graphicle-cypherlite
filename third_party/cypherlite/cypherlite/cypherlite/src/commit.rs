//! Optimistic write commits: private-overlay execution and the group-commit
//! WAL append (independent waiters share one `sync_all`, ADR-0019). Timing
//! histograms live in [`commit_timing`].

use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLockReadGuard};
use web_time::Instant;

use cypherlite_executor::{
    execute, result_rows_bytes, ExecGraph, ExecRequest, Graph, QueryResult as ExecResult, RedoOp,
    SnapshotPin, WriteScratch,
};
use cypherlite_storage::{Wal, WalRecord};

use crate::checkpoint::{begin_publication, lock_wal_checked, Publication};
use crate::{CancelToken, DurabilityMode, Error, Result, Shared};
use cypherlite_core::tokens::WorkspaceCharge;

mod bulk_batch;

use bulk_batch::{BulkBatchPlan, GroupBatchCommit};

use crate::commit_timing::TimingSample;
pub(crate) use crate::commit_timing::{
    commit_timing_env, elapsed_nanos, format_commit_timing, CommitTimingBuf,
};

#[derive(Default)]
pub(crate) struct GroupCommit {
    queue: Vec<CommitSlot>,
    has_leader: bool,
}

impl GroupCommit {
    /// No leader flushing a batch and nothing waiting to join one —
    /// the drain target [`Database::demote`] (ADR-0440) polls for once
    /// `ensure_writable` has refused every new admission, so this can
    /// only become true and stay true.
    pub(crate) fn is_quiescent(&self) -> bool {
        !self.has_leader && self.queue.is_empty()
    }
}

struct CommitSlot {
    snapshot_ts: u64,
    txn_id: u64,
    ops: Vec<RedoOp>,
    parse_plan_ns: u64,
    execute_ns: u64,
    enqueued: Instant,
    waiter: Arc<CommitWaiter>,
}

struct CommitWaiter {
    /// The slot's outcome: on success the commit timestamp the group
    /// assigned it, so the caller rebases onto *its own* commit rather
    /// than re-reading a live clock a foreign commit may have advanced.
    result: Mutex<Option<Result<u64>>>,
    cv: Condvar,
}

/// A record accepted into a commit batch: the redo to append/apply, the
/// commit timestamp assigned at admission, and the timing fields recorded by
/// the group path. `apply_redo` consumes `ops`; the rest stay readable on the
/// error path.
pub(crate) struct Accepted {
    ops: Vec<RedoOp>,
    commit_ts: u64,
    /// Recorded by the fused preflight/append loop in [`commit_preflighted`].
    encode_write_ns: u64,
    wal_lock_ns: u64,
    parse_plan_ns: u64,
    execute_ns: u64,
}

impl Accepted {
    /// A single replicated record: no group-commit timing fields.
    pub(crate) fn replicated(ops: Vec<RedoOp>, commit_ts: u64) -> Self {
        Self {
            ops,
            commit_ts,
            encode_write_ns: 0,
            wal_lock_ns: 0,
            parse_plan_ns: 0,
            execute_ns: 0,
        }
    }
}

/// Timing measured by one [`commit_preflighted`] group when `timing_on` is
/// set: one sample per accepted record plus whether the group ran the Full
/// durability barrier (its single `sync_all`, ADR-0019). [`flush_group`]
/// records both under one timing lock; the replicated path drops it.
pub(crate) struct GroupTiming {
    samples: Vec<TimingSample>,
    synced: bool,
}

/// Auto-commit write entry point. ADR-0209: eligible global bulk writes
/// whose estimated touched-entity count reaches the threshold run in
/// bounded batches, one WAL commit per batch; everything else takes the
/// single commit path. The statement's execution facts ride the
/// `ExecRequest` (plan, params, query text, principal, PROFILE sink) —
/// the same bundle `commit_optimistic` executes.
pub(crate) fn commit_write(
    shared: &Arc<Shared>,
    request: &ExecRequest<'_>,
    parse_plan_ns: u64,
    cancel: &CancelToken,
) -> Result<ExecResult> {
    match BulkBatchPlan::prepare(shared, request.plan)? {
        // ADR-0209 bulk batches run their own clause loop (the batched
        // commit pipeline): neither of the request's per-statement facts
        // applies — the bulk loop records no PROFILE metrics, and its
        // redo ops carry no principal (they are already validated
        // statement output).
        Some(plan) => plan.run(shared, request.params, parse_plan_ns, cancel),
        None => commit_optimistic(shared, request, parse_plan_ns, cancel),
    }
}

/// The guard-and-pin acquisition every optimistic write commit starts
/// with: under the publication read guard, check health and pin the
/// graph. The GraphCell swap protocol requires the pin to be taken while
/// the publication read guard is held, in ALL paths: the reload-swap path
/// (follow.rs) checks `snapshot_pinned()` and replaces the graph under
/// the WRITE lock, so a guardless `pin_now` could interleave between that
/// check and the replace and leave the statement's pin/reference on a
/// graph being overwritten in place through the UnsafeCell. The caller
/// chooses the guard's lifetime — drop it before execution only when
/// batch commits take the publication WRITE lock from inside `execute`
/// (ADR-0210); the pin outlives the guard either way (the read-path
/// pattern, ADR-0137).
fn acquire_guarded_pin(shared: &Arc<Shared>) -> Result<(RwLockReadGuard<'_, ()>, SnapshotPin<'_>)> {
    let read_guard = shared.read_guard()?;
    let pin = shared.graph.pin_now()?;
    Ok((read_guard, pin))
}

/// The guard-agnostic execute-on-fresh-scratch step: build a fresh
/// `WriteScratch` at `snapshot_ts` over the pinned graph and run `f` on
/// its write view. Returns the scratch's final snapshot timestamp — the
/// pin's own unless `f` rebased the scratch (ADR-0210 batch commits) —
/// then `f`'s output, the scratch's redo, and the reservation that keeps
/// that write workspace admitted through its WAL boundary. The caller holds
/// the guard/pin its lock protocol requires; this step touches neither.
fn run_on_scratch<T>(
    shared: &Arc<Shared>,
    snapshot_ts: u64,
    cancel: &CancelToken,
    ontology_document: Option<&str>,
    f: impl FnOnce(&ExecGraph<'_>) -> Result<T>,
) -> Result<(u64, T, Vec<RedoOp>, WorkspaceCharge)> {
    let scratch = RefCell::new(WriteScratch::new(
        snapshot_ts,
        Arc::clone(&shared.next_node_id),
        Arc::clone(&shared.next_rel_id),
    ));
    let out = {
        let view = ExecGraph::write(&shared.graph, &scratch)
            .with_cancel(Some(cancel))
            .with_ontology_document(ontology_document);
        f(&view)?
    };
    let mut scratch = scratch.into_inner();
    let final_ts = scratch.snapshot_ts();
    let (ops, workspace) = scratch.take_redo_with_workspace()?;
    Ok((final_ts, out, ops, workspace))
}

/// The guard-and-pin ceremony every optimistic write commit starts with:
/// acquire the guard and pin, then run `f` on the pinned graph at the
/// pin's snapshot timestamp.
fn with_pinned_graph<T>(
    shared: &Arc<Shared>,
    f: impl FnOnce(&Graph, u64) -> Result<T>,
) -> Result<T> {
    let (_read, pin) = acquire_guarded_pin(shared)?;
    let out = f(&shared.graph, pin.ts());
    drop(pin);
    out
}

/// Returns the raw executor result; the caller (`Connection::dispatch`)
/// stamps the resolved namespace, like it does for every sibling path.
fn commit_optimistic(
    shared: &Arc<Shared>,
    request: &ExecRequest<'_>,
    parse_plan_ns: u64,
    cancel: &CancelToken,
) -> Result<ExecResult> {
    let txn_id = shared.next_txn_id.fetch_add(1, Ordering::SeqCst);
    let timing = shared.timing_on.load(Ordering::Relaxed);
    // ADR-0210: a statement carrying `CALL { … } IN TRANSACTIONS` commits
    // each batch through the group-commit pipeline from inside execution,
    // which needs the publication WRITE lock — so it must not hold the
    // publication read guard across `execute` (same-thread read→write
    // would deadlock). The snapshot pin alone keeps the graph version
    // alive (a pin already outlives the read guard on the read path,
    // ADR-0137), and timestamped version retention keeps mid-execution
    // reads consistent while own batches apply at newer timestamps.
    let batched = cypherlite_planner::plan_contains_in_transactions(request.plan);
    // The base timestamp the statement's own final commit conflict-checks
    // against: the statement's snapshot normally, the last batch's commit
    // after ADR-0210 rebases.
    let (result, commit_base_ts, ops, execute_ns, write_workspace) = {
        let (read_guard, pin) = acquire_guarded_pin(shared)?;
        // The ontology document the statement reads (ADR-0121 §3), captured
        // with the pin under the publication guard: `db.ontology.get` /
        // `db.ontology.apply` see this admission-time document even if a
        // concurrent store-level ontology commit lands mid-statement. A
        // shared handle — the capture is an `Arc` pointer clone.
        let ontology_document = shared.ontology.get();
        // The two lock protocols share one linear flow; `batched` decides
        // only the guard's lifetime and the batch-commit sink:
        // - batched: drop the guard BEFORE `execute` — batch commits take
        //   the publication WRITE lock from inside `execute`, where a held
        //   read guard would deadlock (the pin outlives the guard, the
        //   read-path pattern, ADR-0137);
        // - unbatched: hold the guard across `execute` (it stays in scope
        //   to the end of the block) — `validate_constraints`'s
        //   live-theater fallback relies on it (overlay/catalog.rs).
        if batched {
            drop(read_guard);
        }
        let sink = batched.then(|| GroupBatchCommit {
            shared: Arc::clone(shared),
        });
        let (commit_base_ts, (result, execute_ns), ops, workspace) = run_on_scratch(
            shared,
            pin.ts(),
            cancel,
            ontology_document.as_deref(),
            |view| {
                let t_exec = timing.then(Instant::now);
                // The request's own facts ride through; the batch-commit
                // sink is this path's one addition.
                let mut request = *request;
                request.batch_commit = sink
                    .as_ref()
                    .map(|s| s as &dyn cypherlite_executor::BatchCommit);
                let result = execute(view, &request)?;
                Ok((result, elapsed_nanos(t_exec)))
            },
        )?;
        drop(pin);
        (result, commit_base_ts, ops, execute_ns, workspace)
    };
    // The result remains resident after the write commits, until the
    // surface has consumed it. Reserve it before the WAL boundary: once a
    // WAL record is accepted, this request must report its committed write,
    // never turn into a retryable token-limit error.
    let result_reservation = WorkspaceCharge::retain_with(|| result_rows_bytes(&result.rows))?;
    if ops.is_empty() {
        shared.check_healthy()?;
        drop(write_workspace);
        result_reservation.transfer_to_ledger();
        return Ok(result);
    }
    try_wal_commit(
        shared,
        commit_base_ts,
        txn_id,
        ops,
        parse_plan_ns,
        execute_ns,
    )?;
    drop(write_workspace);
    result_reservation.transfer_to_ledger();
    shared.check_healthy()?;
    Ok(result)
}

/// WAL append is the serialize point. Independent waiters share one fsync
/// (ADR-0019). Conflict is returned to the client (ADR-0017). On success
/// returns the commit timestamp this record was assigned at admission —
/// the value the per-batch sink (`GroupBatchCommit`) must rebase onto,
/// not a re-read of the live commit clock.
pub(super) fn try_wal_commit(
    shared: &Arc<Shared>,
    snapshot_ts: u64,
    txn_id: u64,
    ops: Vec<RedoOp>,
    parse_plan_ns: u64,
    execute_ns: u64,
) -> Result<u64> {
    shared.check_healthy()?;
    crate::ensure_writable(shared)?;
    {
        let cdc = shared.cdc.lock()?;
        if cdc.apply_only() {
            return Err(Error::semantic(
                "database is a CDC replica; writes are not accepted",
            ));
        }
    }
    // This reservation is intentionally taken before the request joins the
    // group-commit queue and held until its waiter completes.  The group
    // leader materializes one WAL payload per accepted slot; all callers
    // retain their own charge while those buffers coexist. The final
    // `Vec<RedoOp>` was reserved before its allocation when the scratch was
    // converted. Consequently a later write is refused at admission instead
    // of evicting a write that has already crossed the WAL boundary.
    let _wal_workspace = WorkspaceCharge::retain(wal_payload_workspace_bytes(&ops)?)?;
    let waiter = CommitWaiter::new();
    let slot = CommitSlot {
        snapshot_ts,
        txn_id,
        ops,
        parse_plan_ns,
        execute_ns,
        enqueued: Instant::now(),
        waiter: Arc::clone(&waiter),
    };
    let is_leader = {
        let mut gc = shared.group.lock()?;
        shared.check_healthy()?;
        // Authoritative admission. `ensure_writable` above can pass, then
        // `demote` can set `draining` and observe an empty queue, before this
        // slot is pushed. Re-checking under the group lock means the write
        // either joins the queue the drain waits on or is refused retryably.
        crate::ensure_writable(shared)?;
        gc.queue.push(slot);
        if gc.has_leader {
            false
        } else {
            gc.has_leader = true;
            true
        }
    };
    if is_leader {
        loop {
            let batch = {
                let mut gc = shared.group.lock()?;
                if gc.queue.is_empty() {
                    gc.has_leader = false;
                    break;
                }
                std::mem::take(&mut gc.queue)
            };
            if let Err(e) = flush_group(shared, batch) {
                let rest = {
                    let mut gc = shared.group.lock()?;
                    gc.has_leader = false;
                    std::mem::take(&mut gc.queue)
                };
                for slot in rest {
                    slot.waiter.complete(Err(e.clone()));
                }
                break;
            }
        }
    }
    let ts = waiter.wait()?;
    shared.check_healthy()?;
    Ok(ts)
}

/// Deterministic payload allocation made after a write leaves its scratch
/// overlay and before WAL/apply completes. The estimate deliberately uses
/// maximum-width LSN/timestamp fields, so it is a small safe upper bound
/// before the group leader assigns the real values.
fn wal_payload_workspace_bytes(ops: &[RedoOp]) -> Result<u64> {
    cypherlite_storage::commit_payload_reservation_bytes(ops)
}

impl CommitWaiter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            result: Mutex::new(None),
            cv: Condvar::new(),
        })
    }

    fn complete(&self, r: Result<u64>) {
        if let Ok(mut g) = self.result.lock() {
            *g = Some(r);
            self.cv.notify_one();
        }
    }

    fn wait(&self) -> Result<u64> {
        let mut g = self.result.lock()?;
        while g.is_none() {
            g = self.cv.wait(g)?;
        }
        g.take()
            .expect("commit waiter result set after condvar wake")
    }
}

/// Owns the waiters of one group flush. `Drop` fails every waiter that has
/// not been completed yet — with the stored error when one was recorded, and
/// with an internal error otherwise — so every early return has exactly one
/// fail-all path and success only ever comes from the explicit
/// `complete_ok()`.
struct Waiters {
    pending: Vec<(usize, Arc<CommitWaiter>)>,
    error: Option<Error>,
}

impl Waiters {
    fn new(batch: &[CommitSlot]) -> Self {
        Self {
            pending: batch
                .iter()
                .enumerate()
                .map(|(i, slot)| (i, Arc::clone(&slot.waiter)))
                .collect(),
            error: None,
        }
    }

    /// Complete a pre-acceptance rejection (conflict / capacity) immediately
    /// and drop it from the pending set so `Drop` never completes it twice.
    fn reject(&mut self, index: usize, error: Error) {
        self.pending.retain(|(i, waiter)| {
            if *i == index {
                waiter.complete(Err(error.clone()));
                false
            } else {
                true
            }
        });
    }

    fn fail(&mut self, error: Error) {
        self.error = Some(error);
    }

    /// Complete every still-pending (i.e. accepted) waiter with the
    /// commit timestamp its slot was assigned at admission. `assigned`
    /// holds one `(batch index, commit_ts)` pair per accepted slot; a
    /// pending waiter with no pair was never accepted, which the caller's
    /// control flow excludes — fail it loudly rather than acknowledge.
    fn complete_ok(&mut self, assigned: &[(usize, u64)]) {
        for (i, waiter) in self.pending.drain(..) {
            match assigned.iter().find(|(j, _)| *j == i) {
                Some((_, ts)) => waiter.complete(Ok(*ts)),
                None => waiter.complete(Err(Error::internal(
                    "committed slot missing its assigned commit timestamp",
                ))),
            }
        }
    }
}

impl Drop for Waiters {
    fn drop(&mut self) {
        for (_, waiter) in self.pending.drain(..) {
            match &self.error {
                Some(error) => waiter.complete(Err(error.clone())),
                // Success is only ever signalled by `complete_ok`: a guard
                // dropped without a recorded outcome must not acknowledge
                // the write.
                None => waiter.complete(Err(Error::internal(
                    "commit group dropped without a result",
                ))),
            }
        }
    }
}

/// One group flush's successful outcome: the `(batch index, commit_ts)`
/// pair of every accepted slot (each waiter's success value) plus the
/// group's timing, when measured.
struct FlushOutcome {
    assigned: Vec<(usize, u64)>,
    timing: Option<GroupTiming>,
}

/// Apply one replicated record through the shared commit core — the replica
/// write path's whole choreography, every step delegated to its named
/// canonical helper (`ensure_writable`, `begin_publication`,
/// `ensure_no_cdc_source_slot`, `admit_redo_capacity`, `lock_wal_checked`)
/// beside the group-commit leader, so the entry is self-fencing like
/// `try_wal_commit`.
pub(crate) fn commit_replicated(shared: &Arc<Shared>, record: &WalRecord) -> Result<u64> {
    crate::ensure_writable(shared)?;
    let publication = begin_publication(shared)?;
    // Authoritative admission. This path never joins the group-commit
    // queue, so `demote` cannot see it in `wait_for_drain`. The publication
    // write lock is the same one `demote` holds across releasing the writer
    // lease, so a commit that passed the check above either finishes before
    // that release or is refused here.
    crate::ensure_writable(shared)?;
    crate::shared::ensure_no_cdc_source_slot(shared)?;
    // The canonical capacity admission and durability-fenced WAL acquire:
    // this path spells neither policy itself.
    shared.admit_redo_capacity(&record.ops, 0)?;
    let wal = lock_wal_checked(shared)?;
    // The replicated path records no group-commit timing: it does not ask
    // the core to measure (`measure: false`), so it pays for no samples it
    // would drop.
    let committed = commit_preflighted(
        shared,
        wal,
        vec![Accepted::replicated(record.ops.clone(), record.commit_ts)],
        publication,
        false,
    )?;
    shared.check_healthy()?;
    Ok(committed.last_lsn)
}

fn flush_group(shared: &Arc<Shared>, batch: Vec<CommitSlot>) -> Result<()> {
    let mut waiters = Waiters::new(&batch);
    // Acquire the publication and WAL guards and run the commit core inside
    // one closure, so the guards release exactly when the group's outcome is
    // known; the single terminal `match` is the only place that completes or
    // fails every waiter.
    let result = (|| -> Result<FlushOutcome> {
        let publication = begin_publication(shared)?;
        let wal = lock_wal_checked(shared)?;
        let batch_start = Instant::now();
        // The batch index travels with the accepted record, so the
        // completion list below derives from this one source instead of a
        // parallel vector.
        let mut accepted: Vec<(usize, Accepted)> = Vec::new();
        let mut reserved = 0u64;

        for (index, slot) in batch.into_iter().enumerate() {
            let wal_lock_ns = batch_start
                .saturating_duration_since(slot.enqueued)
                .as_nanos() as u64;
            // Previously accepted slots' ops stand in for the old `unapplied`
            // shadow list (ADR-0063: no second deep clone of the redo ops).
            let conflict = shared
                .graph
                .write_set_conflicts(slot.snapshot_ts, &slot.ops)
                || accepted
                    .iter()
                    .any(|(_, a)| Graph::redo_conflicts_with_later(&slot.ops, &a.ops));
            if conflict {
                waiters.reject(index, Error::conflict());
                continue;
            }
            match shared.admit_redo_capacity(&slot.ops, reserved) {
                Ok(next) => reserved = next,
                Err(e) => {
                    waiters.reject(index, e);
                    continue;
                }
            }
            let commit_ts = {
                let last = accepted
                    .last()
                    .map(|(_, a)| a.commit_ts)
                    .unwrap_or_else(|| shared.graph.commit_ts());
                last.checked_add(1).map(|next| next.max(slot.txn_id))
            };
            // Timestamp overflow rejects the slot here, like the conflict
            // and capacity arms above — never deferred to a sentinel the
            // group-wide preflight must remember to catch.
            let Some(commit_ts) = commit_ts else {
                waiters.reject(index, Error::storage("commit timestamp overflow"));
                continue;
            };
            accepted.push((
                index,
                Accepted {
                    ops: slot.ops,
                    commit_ts,
                    encode_write_ns: 0,
                    wal_lock_ns,
                    parse_plan_ns: slot.parse_plan_ns,
                    execute_ns: slot.execute_ns,
                },
            ));
        }

        // Strip the batch indices the records traveled with — the commit
        // core is shared with the replicated path, which never sees them —
        // deriving each accepted waiter's success value from the same
        // records, just before the move.
        let (assigned, records): (Vec<(usize, u64)>, Vec<Accepted>) = accepted
            .into_iter()
            .map(|(index, a)| ((index, a.commit_ts), a))
            .unzip();

        if records.is_empty() {
            return Ok(FlushOutcome {
                assigned,
                timing: None,
            });
        }

        commit_preflighted(
            shared,
            wal,
            records,
            publication,
            shared.timing_on.load(Ordering::Relaxed),
        )
        .map(|committed| FlushOutcome {
            assigned,
            timing: committed.timing,
        })
    })();
    match result {
        Ok(outcome) => {
            // One flat record under a single lock: the group's samples plus
            // its one Full-durability-barrier count (ADR-0019).
            if let (Some(t), Ok(mut buf)) = (outcome.timing, shared.timing.lock()) {
                buf.sync_calls += u64::from(t.synced);
                buf.samples.extend(t.samples);
            }
            waiters.complete_ok(&outcome.assigned);
            Ok(())
        }
        Err(e) => {
            waiters.fail(e.clone());
            Err(e)
        }
    }
}

/// What one preflighted commit group produced: the timing samples (only
/// when `CYPHERLITE_COMMIT_TIMING` is on) and the last LSN the group
/// assigned — the pipeline is the one place that numbers the records, so a
/// caller that reports the LSN (the replicated path) reads it here instead
/// of pre-computing it.
pub(crate) struct Committed {
    pub(crate) timing: Option<GroupTiming>,
    pub(crate) last_lsn: u64,
}

/// Shared commit core for the group-commit leader and replicated apply:
/// preflight, append, the fault-injection hooks, the durability barrier, the
/// ordered `apply_redo`, and `finish_applied_commit`. Runs under the caller's
/// WAL mutex and publication guard; any uncertain physical step poisons the
/// shared state. Expects a non-empty group: `Committed::last_lsn` is the
/// last LSN the group assigned, so an empty group would report the previous
/// record's. Measures per-record timing whenever the caller passes
/// `measure: true` (the group-commit leader forwards `shared.timing_on`; the
/// replicated path passes `false` and pays for no samples it would drop) and
/// returns the samples (each carrying the group's single `sync_all` cost)
/// plus whether the group ran the Full durability barrier, for the caller to
/// record.
pub(crate) fn commit_preflighted(
    shared: &Arc<Shared>,
    mut wal: MutexGuard<'_, Wal>,
    mut accepted: Vec<Accepted>,
    publication: Publication<'_>,
    measure: bool,
) -> Result<Committed> {
    let mut proofs = Vec::with_capacity(accepted.len());
    for (i, a) in accepted.iter().enumerate() {
        let lsn = wal
            .lsn
            .checked_add(i as u64 + 1)
            .ok_or_else(|| Error::storage("WAL LSN overflow"))?;
        proofs.push(wal.preflight_record(lsn, a.commit_ts, &a.ops)?);
    }
    shared.graph.validate_redo_group_applicability(
        accepted.iter().map(|a| (a.ops.as_slice(), a.commit_ts)),
    )?;

    // ADR-0120 §2/§3: the group's store-level ontology outcome (the last
    // ontology op wins), scanned BEFORE the first append. Nothing is
    // derived from the document (ADR-0229 §3) and it was validated at set
    // time, before the redo was pushed, so this cannot fail; the 16 MiB
    // envelope below is the WAL preflight's own `RedoOp` validation.
    let ontology = crate::ontology::prepare_committed(accepted.iter().map(|a| a.ops.as_slice()));

    // Checkpoint admission was closed and any old worker fully drained before
    // the first frame. Keep the publication lock until every redo is applied.
    // Each proof owns its encoded payload, so its element can be borrowed
    // mutably and the write cost recorded directly in this single append
    // loop, avoiding a parallel per-record vector and a second pass.
    for (a, proof) in accepted.iter_mut().zip(proofs) {
        match wal.append_preflighted(proof) {
            Ok(written) => a.encode_write_ns = written.encode_write_ns,
            Err(cause) => return Err(shared.poison(&cause)),
        }
    }
    #[cfg(test)]
    if shared.fail_after_append.swap(false, Ordering::SeqCst) {
        return Err(shared.poison(&Error::io("injected post-append failure")));
    }

    // Full durability barriers once for the accepted group. Relaxed modes
    // acknowledge after apply: EverySecond's worker barriers the dirty WAL,
    // while Snapshot relies on a completed checkpoint. Rotation below adds no
    // second data-segment barrier: `sync` above already made the group's
    // records durable under the same WAL mutex.
    let synced = shared.resolved.durability_mode == DurabilityMode::Full;
    let sync_ns = if synced {
        match wal.sync() {
            Ok(ns) => ns,
            Err(e) => return Err(shared.poison(&e)),
        }
    } else {
        0
    };
    #[cfg(test)]
    if shared.fail_after_sync.swap(false, Ordering::SeqCst) {
        // Simulate an unexpected graph change after validation: the actual
        // apply_redo call below must fail and take the real poison branch.
        shared.graph.apply_redo(vec![], accepted[0].commit_ts)?;
    }
    let mut samples = measure.then(|| Vec::with_capacity(accepted.len()));
    for a in accepted.iter_mut() {
        let t_apply = measure.then(Instant::now);
        // `apply_redo` consumes the ops: the WAL already encoded them
        // (preflight encoded them, above), so apply moves them out instead
        // of cloning the property maps. `mem::take` leaves the entry in place
        // so timing can still read its other fields.
        let ops = std::mem::take(&mut a.ops);
        let commit_ts = a.commit_ts;
        if let Err(e) = shared.graph.apply_redo(ops, commit_ts) {
            return Err(shared.poison(&e));
        }
        let apply_ns = elapsed_nanos(t_apply);
        if let Some(samples) = samples.as_mut() {
            samples.push(TimingSample {
                parse_plan_ns: a.parse_plan_ns,
                execute_ns: a.execute_ns,
                wal_lock_ns: a.wal_lock_ns,
                encode_write_ns: a.encode_write_ns,
                sync_ns,
                apply_ns,
            });
        }
    }
    // The ontology commit is durable and applied: update the store's
    // document state and the segment-magic flag now, under the still-held
    // WAL mutex and publication boundary, so no checkpoint can publish or
    // recycle past this commit without observing it (ADR-0120 §2/§4).
    if let Some(committed) = ontology {
        crate::ontology::install_committed(shared, &mut wal, committed);
    }
    // Rotation does not add another data-segment commit barrier: `sync` above
    // already made the group's records durable under the same WAL mutex.
    // `finish_applied_commit` releases the checkpoint publication boundary and
    // schedules a mode-appropriate rotation/checkpoint. Waiters are completed
    // by the caller's RAII `Waiters`.
    let last_lsn = wal.lsn;
    shared.finish_applied_commit(wal, publication)?;
    Ok(Committed {
        timing: samples.map(|samples| GroupTiming { samples, synced }),
        last_lsn,
    })
}

#[cfg(test)]
mod tests;
