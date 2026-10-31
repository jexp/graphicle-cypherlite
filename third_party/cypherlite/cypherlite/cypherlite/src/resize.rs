//! Promote a WAL follower to writer, and demote a writer back to follower
//! (ADR-0440, engine ADR-0440): CypherLite owns the in-process role change
//! so the loaded graph and the open WAL/checkpoint machinery survive the
//! swap. memorydb-cloud owns the decision to call either and the pod
//! lifecycle around it; this module knows nothing about pods, StatefulSets,
//! or capacity.
//!
//! Split out of `lib.rs` per the crate's per-concern file convention
//! (ADR-0371's split precedent): the three `start_*` helpers below are the
//! one gate the open path (`open.rs`), `Database::promote`, and
//! `Database::demote` all share for deciding whether a timer or the follow
//! thread should be running right now, so that decision lives once.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

use cypherlite_core::Error;
#[cfg(not(target_arch = "wasm32"))]
use cypherlite_storage::take_writer_lease;

use crate::checkpoint::{
    begin_publication, checkpoint_now, spawn_checkpoint_timer, spawn_durability_timer,
};
use crate::follow;
use crate::shared::stop_and_join;
use crate::{Database, DurabilityMode, Result, Shared, ThreadStop};

/// How long [`Database::demote`] waits for the group-commit queue to empty
/// before giving up and handing the writer back to service rather than
/// wedging the handoff forever. `ensure_writable` has already refused every
/// new admission by the time the wait starts, so the queue can only shrink;
/// this bound exists only so a caller that never finishes a commit (a stuck
/// test gate, a bug) fails loudly instead of hanging the whole handoff.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const DRAIN_POLL: Duration = Duration::from_millis(5);

/// A worker restart after a role change is best-effort. The role has
/// already moved, so a spawn failure is logged and does not change the
/// `Result` the caller reports.
fn best_effort_worker(what: &str, result: Result<()>) {
    if let Err(err) = result {
        eprintln!("cypherlite: {what} failed: {err}");
    }
}

/// Start the checkpoint timer if this handle is writable right now, its
/// resolved interval is nonzero, and the WAL is not `:memory:` — and one is
/// not already installed. Used by the open path for a bootstrap `Writable`
/// open and by [`Database::promote`] turning a follower into the writer;
/// both want exactly this gate, so it is written once.
pub(crate) fn start_checkpoint_timer(shared: &Arc<Shared>) -> Result<()> {
    if !shared.writable.load(Ordering::Acquire)
        || shared.resolved.checkpoint_interval == Duration::ZERO
        || shared.store.wal_is_memory()
    {
        return Ok(());
    }
    let mut slot = shared.timer.lock()?;
    if slot.is_some() {
        return Ok(());
    }
    let stop = Arc::new(ThreadStop::new());
    if let Some(handle) = spawn_checkpoint_timer(
        Arc::downgrade(shared),
        Arc::clone(&stop),
        shared.resolved.checkpoint_interval,
    )? {
        *slot = Some((stop, handle));
    }
    Ok(())
}

/// Start the 1s WAL-sync timer under the same writability gate as
/// [`start_checkpoint_timer`], when `DurabilityMode::EverySecond` is the
/// resolved policy.
pub(crate) fn start_durability_timer(shared: &Arc<Shared>) -> Result<()> {
    if !shared.writable.load(Ordering::Acquire)
        || shared.resolved.durability_mode != DurabilityMode::EverySecond
        || shared.store.wal_is_memory()
    {
        return Ok(());
    }
    let mut slot = shared.durability_timer.lock()?;
    if slot.is_some() {
        return Ok(());
    }
    let stop = Arc::new(ThreadStop::new());
    if let Some(handle) = spawn_durability_timer(
        Arc::downgrade(shared),
        Arc::clone(&stop),
        Duration::from_secs(1),
    )? {
        *slot = Some((stop, handle));
    }
    Ok(())
}

/// Start the follow thread when this handle should tail the WAL and is not
/// the writer. True at open for a follower, and true again after
/// [`Database::demote`] for either a follower or a writable open. A frozen
/// `ReadOnly` open never follows.
pub(crate) fn start_follow_thread(shared: &Arc<Shared>) -> Result<()> {
    if shared.writable.load(Ordering::Acquire) {
        return Ok(());
    }
    if shared.resolved.read_only && !shared.resolved.opened_as_follow {
        return Ok(());
    }
    let mut slot = shared.follow_ctl.lock()?;
    if slot.is_some() {
        return Ok(());
    }
    let stop = Arc::new(ThreadStop::new());
    let handle = follow::spawn(Arc::downgrade(shared), Arc::clone(&stop))?;
    *slot = Some((stop, handle));
    Ok(())
}

/// Poll the group-commit queue until it is both leaderless and empty, or
/// `DRAIN_TIMEOUT` elapses. Called only after `draining` is set, so no new
/// commit can join the queue while this waits.
fn wait_for_drain(shared: &Shared) -> Result<bool> {
    let started = Instant::now();
    loop {
        {
            let gc = shared.group.lock()?;
            if gc.is_quiescent() {
                return Ok(true);
            }
        }
        if started.elapsed() > DRAIN_TIMEOUT {
            return Ok(false);
        }
        std::thread::sleep(DRAIN_POLL);
    }
}

/// The lease-then-catch-up body of [`Database::promote`], split out so the
/// public method's only job is the join-then-rollback-on-failure shape.
fn promote_inner(shared: &Arc<Shared>) -> Result<()> {
    if shared.needs_reopen() {
        let err = shared.check_healthy();
        // `promote` already joined the follow thread. The WAL was not
        // swapped, so the thread has to resume.
        best_effort_worker("follow thread restart", start_follow_thread(shared));
        return err;
    }
    let root = match shared.store.filesystem_dir() {
        Some(root) => root,
        None => {
            best_effort_worker("follow thread restart", start_follow_thread(shared));
            return Err(Error::semantic(
                "promote is only supported for native directory databases",
            ));
        }
    };
    // The writer lease is a native file lock. wasm cannot open a filesystem
    // database, so a store that still reports a directory adopts the WAL
    // without one.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let wal_dir = cypherlite_storage::wal_dir(&shared.store).unwrap_or(root);
        let lease = match take_writer_lease(root, wal_dir) {
            Ok(lease) => lease,
            Err(err) => {
                best_effort_worker("follow thread restart", start_follow_thread(shared));
                return Err(err);
            }
        };
        if let Err(err) = adopt_writer(shared) {
            drop(lease);
            best_effort_worker("follow thread restart", start_follow_thread(shared));
            return Err(err);
        }
        *shared.writer_lease.lock()? = Some(lease);
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = root;
        if let Err(err) = adopt_writer(shared) {
            best_effort_worker("follow thread restart", start_follow_thread(shared));
            return Err(err);
        }
    }
    shared.writable.store(true, Ordering::Release);
    shared.persist_on_drop.store(true, Ordering::Release);
    start_checkpoint_timer(shared)?;
    start_durability_timer(shared)?;
    Ok(())
}

/// Apply the surviving WAL tail onto the loaded graph, then swap in a
/// writable WAL scanned from disk. The graph stays. A checkpoint gap
/// follows [`crate::FollowGapPolicy`] and does not load a second graph
/// when the policy is `Yield` (ADR-0440, ADR-0441).
///
/// Called only with the writer lease already held, so no other writer can
/// append during the scan, and a torn-tail trim is as safe as it is for a
/// bootstrap open.
fn adopt_writer(shared: &Shared) -> Result<()> {
    follow::catch_up_to_tip(shared)?;
    if shared.needs_reopen() {
        return shared.check_healthy();
    }
    let applied = {
        let wal = shared.wal.lock()?;
        wal.lsn
    };
    let mut wal = cypherlite_storage::open_writer_wal(&shared.store)?;
    if wal.lsn != applied {
        return Err(Error::storage(format!(
            "promote WAL LSN {} does not match the applied LSN {applied}",
            wal.lsn
        )));
    }
    wal.set_ontology_held(shared.ontology.get().is_some());
    let mut slot = shared.wal.lock()?;
    *slot = wal;
    Ok(())
}

/// The drain-checkpoint-release body of [`Database::demote`]. On `Err`,
/// the writer lease is still held and `writable` is still `true`: the
/// timers this handle needs as the writer are restarted before returning,
/// so a failed demote leaves the writer exactly as it was.
fn demote_inner(shared: &Arc<Shared>) -> Result<()> {
    if !wait_for_drain(shared)? {
        return Err(Error::internal(
            "demote timed out waiting for in-flight commits to finish",
        ));
    }
    checkpoint_now(shared)?;
    stop_and_join(&shared.timer);
    stop_and_join(&shared.durability_timer);
    // Hold the publication write lock across the WAL swap and the lease
    // release. `commit_replicated` and CDC slot mutations re-check
    // `ensure_writable` under that lock, so they either finish first or
    // observe the follower.
    let publication = match begin_publication(shared) {
        Ok(publication) => publication,
        Err(err) => {
            best_effort_worker("checkpoint timer restart", start_checkpoint_timer(shared));
            best_effort_worker("durability timer restart", start_durability_timer(shared));
            return Err(err);
        }
    };
    // The graph already matches the checkpoint just written. The follow
    // thread only needs a read-only WAL: `poll_follow` refuses the append
    // handle. Swapping that WAL in while this process still holds the
    // writer lease means no other writer can append during the scan.
    if let Err(err) = adopt_follower_wal(shared) {
        drop(publication);
        best_effort_worker("checkpoint timer restart", start_checkpoint_timer(shared));
        best_effort_worker("durability timer restart", start_durability_timer(shared));
        return Err(err);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        *shared.writer_lease.lock()? = None;
    }
    shared.persist_on_drop.store(false, Ordering::Release);
    shared.writable.store(false, Ordering::Release);
    drop(publication);
    Ok(())
}

/// Replace the append WAL with a read-only one positioned at the published
/// checkpoint. The loaded graph stays.
fn adopt_follower_wal(shared: &Shared) -> Result<()> {
    let wal = cypherlite_storage::open_follower_wal(&shared.store)?;
    let mut slot = shared.wal.lock()?;
    *slot = wal;
    Ok(())
}

impl Database {
    /// Take over as writer from a WAL follower (ADR-0440, engine ADR-0440).
    ///
    /// No-op if this handle is already the writer, whether it opened
    /// [`crate::OpenMode::Writable`] or was promoted before. A `ReadOnly`
    /// open can never become one. A demoted writer, including one that
    /// opened writable, takes the same writer lease a follower's promote
    /// takes.
    ///
    /// Joins the follow thread first — so a concurrent tick cannot race the
    /// catch-up below — then takes the writer lease: a second writer
    /// (another process, or another handle this one already promoted)
    /// fails exactly as a second `Writable` open would, with
    /// [`Error::Locked`]. Only with the lease held does it apply any
    /// surviving WAL tail onto the loaded graph and swap in a writable WAL
    /// rescanned from disk. The graph stays. A checkpoint gap follows
    /// [`crate::FollowGapPolicy`]: `Yield` latches `follow-gap` instead of
    /// loading a second graph. Then it flips the live writable fact and
    /// starts the checkpoint and durability timers. A failure before the
    /// append WAL is installed drops the lease and restarts the follow
    /// thread, leaving this handle a follower. A failure after that swap
    /// does not restart the thread: it would poll the append WAL. A handle
    /// that already needs reopen returns that error without taking the
    /// lease.
    pub fn promote(&self) -> Result<()> {
        let shared = &self.inner;
        // One role change at a time. Two promotes otherwise both pass the
        // writable check, and the loser's failure path restarts a follow
        // thread against the winner's append WAL.
        let _role = shared.role.lock()?;
        if shared.resolved.read_only && !shared.resolved.opened_as_follow {
            return Err(Error::semantic(
                "promote cannot make a read-only open the writer",
            ));
        }
        if shared.writable.load(Ordering::Acquire) {
            return Ok(());
        }
        stop_and_join(&shared.follow_ctl);
        promote_inner(shared)
    }

    /// Hand writer status back to a follower (ADR-0440, engine ADR-0440).
    ///
    /// Releases the one writer lease, whether [`Self::promote`] took it or
    /// a writable open did. No-op if this handle is already a follower.
    /// `:memory:` has no lease to hand off.
    ///
    /// Sets `draining` first: [`crate::shared::ensure_writable`] then
    /// refuses every new write with the retryable `writer-draining`
    /// [`Error::ResourceLimit`] (429 over CypherLite HTTP, the transient
    /// `MemoryPoolOutOfMemoryError` over Bolt) while a write already
    /// admitted into the group-commit queue finishes and is checkpointed,
    /// never cancelled. Waits for that queue to drain, checkpoints, stops
    /// the timers, reopens the WAL read-only at that checkpoint, and keeps
    /// the loaded graph. Then it releases the writer lease, still holding
    /// the publication write lock, and restarts the follow thread. A
    /// failure before that release leaves this handle the writer — the
    /// timers restart too — because the lease was never released and
    /// `writable` was never cleared. Restarting the follow thread after
    /// the release is best-effort: a spawn failure is logged and does not
    /// report the demotion as failed. `draining` clears whether this
    /// succeeds or fails.
    pub fn demote(&self) -> Result<()> {
        let shared = &self.inner;
        let _role = shared.role.lock()?;
        if !shared.writable.load(Ordering::Acquire) {
            return Ok(());
        }
        if shared.store.filesystem_dir().is_none() {
            return Err(Error::semantic(
                "demote is only supported for native directory databases",
            ));
        }
        shared.draining.store(true, Ordering::Release);
        let result = demote_inner(shared);
        shared.draining.store(false, Ordering::Release);
        result?;
        best_effort_worker("follow thread restart", start_follow_thread(shared));
        Ok(())
    }

    /// Whether this handle is mid-`demote`: new writes refuse retryably,
    /// but the handle is still Ready for ingress until the drain finishes
    /// (ADR-0440's db-api role surfaces this on `GET /info`).
    pub fn is_draining(&self) -> bool {
        self.inner.draining.load(Ordering::Acquire)
    }
}
