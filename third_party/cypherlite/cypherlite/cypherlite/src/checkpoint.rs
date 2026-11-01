//! Checkpoint machinery: pin the WAL, dump a snapshot, recycle segments
//! (ADR-0020), the background timer (ADR-0041), and persist-on-drop.

use std::sync::atomic::Ordering;
use std::sync::{Arc, MutexGuard, RwLockWriteGuard, Weak};
use std::time::Duration;

use cypherlite_storage::{checkpoint_at, Wal};

use crate::commit::format_commit_timing;
use crate::{DurabilityMode, Error, Result, Shared, ThreadStop};

#[derive(Default)]
pub(crate) struct CheckpointState {
    busy: bool,
    blocked: bool,
    last_error: Option<Error>,
}

/// Excludes readers/writers, closes checkpoint admission, then waits for a
/// previously admitted dump AND its recycle to finish. No WAL lock is held.
pub(crate) struct Publication<'a> {
    shared: &'a Shared,
    _write: RwLockWriteGuard<'a, ()>,
}

pub(crate) fn begin_publication(shared: &Shared) -> Result<Publication<'_>> {
    let write = shared.publication.write()?;
    shared.check_healthy()?;
    admit_publication(shared, write)
}

/// Publication lock for a follow reload. The reload thread may already have
/// recorded a pin-stall failure in the follow health state; that must not
/// abort the swap that clears it, so this gate checks WAL poisoning only.
pub(crate) fn begin_reload_swap(shared: &Shared) -> Result<Publication<'_>> {
    let write = shared.publication.write()?;
    shared.check_poisoned()?;
    admit_publication(shared, write)
}

/// Close checkpoint admission and wait for an admitted dump and its recycle
/// to finish. The caller holds the publication write guard and has already
/// run its health gate.
fn admit_publication<'a>(
    shared: &'a Shared,
    write: RwLockWriteGuard<'a, ()>,
) -> Result<Publication<'a>> {
    // `Publication`'s Drop owns the admission release, so build it before
    // the last_error check and return it through both arms: on the error
    // path the state guard unlocks first, then the publication clears
    // `blocked` and notifies.
    let publication = Publication {
        shared,
        _write: write,
    };
    let mut state = shared.checkpoint_busy.lock()?;
    state.blocked = true;
    while state.busy {
        state = shared.checkpoint_cv.wait(state)?;
    }
    match state.last_error.take() {
        Some(error) => Err(error),
        None => Ok(publication),
    }
}

impl Drop for Publication<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.checkpoint_busy.lock() {
            state.blocked = false;
            self.shared.checkpoint_cv.notify_all();
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // `Database::drop` already called `stop_background`, which joined
        // the timers; this signal is only for a `Shared` that never went
        // through that path (a construction failure before the handle
        // existed). Signaling again here is harmless either way.
        self.signal_background();
        if self.timing_on.load(Ordering::Relaxed) && !self.timing_dumped.load(Ordering::Relaxed) {
            if let Ok(buf) = self.timing.lock() {
                let s = format_commit_timing(&buf);
                if !s.is_empty() {
                    eprint!("{s}");
                }
            }
        }
        let persist =
            self.persist_on_drop.load(Ordering::Acquire) && !self.poisoned.load(Ordering::Acquire);
        if persist {
            let _ = run_checkpoint(self, Trigger::Automatic);
        }
    }
}

/// Who asked for a checkpoint. Only an explicit `Database::checkpoint()`
/// dumps a non-durable store (ADR-0105).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Explicit,
    Automatic,
}

/// Run one checkpoint to completion, retrying admission if a concurrent
/// checkpoint or a blocked writer currently holds it. Shared by the timed
/// walker, `Database::checkpoint()`, and persist-on-drop.
fn run_checkpoint(shared: &Shared, trigger: Trigger) -> Result<()> {
    loop {
        wait_checkpoint_idle(shared)?;
        if try_begin_checkpoint(shared) {
            let result = pin_rotate_and_checkpoint(shared, trigger);
            end_checkpoint(shared, &result);
            return result;
        }
    }
}

pub(crate) fn wait_checkpoint_idle(shared: &Shared) -> Result<()> {
    let mut busy = shared.checkpoint_busy.lock()?;
    while busy.busy || busy.blocked {
        shared.check_healthy()?;
        busy = shared.checkpoint_cv.wait(busy)?;
    }
    shared.check_healthy()?;
    Ok(())
}

pub(crate) fn try_begin_checkpoint(shared: &Shared) -> bool {
    let Ok(mut busy) = shared.checkpoint_busy.lock() else {
        return false;
    };
    if busy.busy || busy.blocked || shared.poisoned.load(Ordering::Acquire) {
        return false;
    }
    busy.busy = true;
    true
}

fn end_checkpoint(shared: &Shared, result: &Result<()>) {
    if let Ok(mut busy) = shared.checkpoint_busy.lock() {
        busy.busy = false;
        // Only a checkpoint that fails while a writer is blocked on its
        // admission drain is surfaced to that writer. A standalone
        // `Database::checkpoint()` failure is returned to its own caller and
        // must not fence later writes (ADR-0094).
        busy.last_error = if busy.blocked {
            result.as_ref().err().cloned()
        } else {
            None
        };
        shared.checkpoint_cv.notify_all();
    }
}

pub(crate) fn checkpoint_now(shared: &Shared) -> Result<()> {
    // A failed background durability barrier fences explicit checkpoints on
    // the current handle until reopen (ADR-0077, ADR-0094).
    check_durability_error(shared)?;
    run_checkpoint(shared, Trigger::Explicit)
}

/// Rotate the open segment, pin LSN/`commit_ts` from WAL metadata, dump
/// (if [`dumps`]) and recycle. Must not take the WAL mutex across dump
/// (ADR-0020 / ADR-0038).
fn pin_rotate_and_checkpoint(shared: &Shared, trigger: Trigger) -> Result<()> {
    shared.check_healthy()?;
    let (lsn, pin_ts) = {
        let mut wal = shared.wal.lock()?;
        rotate_for_mode(&mut wal, shared.resolved.durability_mode)?;
        wal.checkpoint_pin()
    };
    checkpoint_pinned(shared, trigger, lsn, pin_ts)
}

/// Automatic checkpoints of a store whose snapshot VFS is not durable
/// (`:memory:`) write no snapshot; they only recycle the WAL (ADR-0105).
fn dumps(shared: &Shared, trigger: Trigger) -> bool {
    trigger == Trigger::Explicit || shared.store.vfs().durable()
}

fn checkpoint_pinned(shared: &Shared, trigger: Trigger, lsn: u64, pin_ts: u64) -> Result<()> {
    // Clamp the dump horizon to the graph's commit ts — the one seam, so
    // every pin producer inherits the floor. The WAL pin is 0 when no
    // commit was appended since the last recycle (e.g. a read-only session
    // right after a snapshot load), while snapshot-loaded rows are born at
    // xmin 1 — dumping at 0 would write an empty snapshot. After a rotate
    // every committed record is in a closed segment, so `graph.commit_ts()`
    // can only exceed the pin by a snapshot-synthesized ts that has no WAL
    // records the cut at `lsn` could lose.
    let snapshot_ts = pin_ts.max(shared.graph.commit_ts());
    if dumps(shared, trigger) {
        shared.check_healthy()?;
        // ADR-0120 §5/§6: the checkpoint publishes the store's current
        // ontology document (v4 manifest + object, or the JSON inline
        // field at value_encoding 3); `None` keeps the previous format.
        // The commit pipeline updates the state under the publication
        // boundary, so a checkpoint always observes the last committed
        // document.
        checkpoint_at(
            &shared.store,
            &shared.graph,
            lsn,
            snapshot_ts,
            shared.ontology.get().as_deref(),
        )?;
    }
    recycle_and_gc(shared, lsn)
}

fn recycle_and_gc(shared: &Shared, lsn: u64) -> Result<()> {
    let cut = {
        let cdc = shared.cdc.lock()?;
        cdc.recycle_cut(lsn)
    };
    {
        let mut wal = shared.wal.lock()?;
        wal.recycle(cut)?;
    }
    shared.graph.gc_versions();
    Ok(())
}

pub(crate) fn spawn_checkpoint_timer(
    weak: Weak<Shared>,
    ctl: Arc<ThreadStop>,
    interval: Duration,
) -> Result<Option<std::thread::JoinHandle<()>>> {
    crate::spawn_interval_worker(
        "cypherlite-checkpoint-timer",
        weak,
        ctl,
        interval,
        timed_checkpoint,
    )
}

pub(crate) fn spawn_durability_timer(
    weak: Weak<Shared>,
    ctl: Arc<ThreadStop>,
    interval: Duration,
) -> Result<Option<std::thread::JoinHandle<()>>> {
    crate::spawn_interval_worker("cypherlite-wal-sync", weak, ctl, interval, durability_tick)
}

fn durability_tick(shared: &Shared) {
    let result = match shared.wal.lock() {
        Ok(mut wal) if wal.needs_sync() => wal.sync().map(|_| ()),
        Ok(_) => Ok(()),
        Err(err) => Err(Error::semantic(err.to_string())),
    };
    if let Err(err) = result {
        record_durability_error(shared, err);
    }
}

pub(crate) fn check_durability_error(shared: &Shared) -> Result<()> {
    let guard = shared.durability_error.lock()?;
    match guard.as_ref() {
        Some(e) => Err(e.clone()),
        None => Ok(()),
    }
}

/// Snapshot rotate skips the WAL barrier (checkpoint is the durable cut).
/// Full always barriers; EverySecond also barriers here so a checkpoint that
/// wins the race against the 1s worker still closes the durability window.
pub(crate) fn rotate_for_mode(wal: &mut Wal, mode: DurabilityMode) -> Result<u64> {
    match mode {
        DurabilityMode::Snapshot => wal.rotate_relaxed(),
        DurabilityMode::Full | DurabilityMode::EverySecond => wal.rotate(),
    }
}

pub(crate) fn lock_wal_checked(shared: &Shared) -> Result<MutexGuard<'_, Wal>> {
    check_durability_error(shared)?;
    let wal = shared.wal.lock()?;
    if let Err(e) = check_durability_error(shared) {
        drop(wal);
        return Err(e);
    }
    Ok(wal)
}

fn record_durability_error(shared: &Shared, error: Error) {
    if let Ok(mut guard) = shared.durability_error.lock() {
        if guard.is_none() {
            *guard = Some(error);
        }
    }
}

pub(crate) fn timed_checkpoint(shared: &Shared) {
    if !try_begin_checkpoint(shared) {
        return;
    }
    let dirty = match shared.wal.lock() {
        Ok(wal) => wal.has_uncheckpointed(),
        Err(_) => {
            end_checkpoint(shared, &Ok(()));
            return;
        }
    };
    if !dirty {
        end_checkpoint(shared, &Ok(()));
        return;
    }
    let result = pin_rotate_and_checkpoint(shared, Trigger::Automatic);
    end_checkpoint(shared, &result);
}

/// WAL-rotation checkpoint. A dump runs on a background thread where threads
/// exist. [`Database`]'s drop joins that thread before the writer lease can
/// be taken again. A recycle-only checkpoint, or a failed spawn, runs inline.
pub(crate) fn schedule_checkpoint(shared: &Arc<Shared>, lsn: u64, snapshot_ts: u64) {
    #[cfg(not(target_arch = "wasm32"))]
    if dumps(shared, Trigger::Automatic) {
        let shared_t = Arc::clone(shared);
        if let Ok(handle) = std::thread::Builder::new()
            .name("cypherlite-checkpoint".into())
            .spawn(move || {
                let result = checkpoint_pinned(&shared_t, Trigger::Automatic, lsn, snapshot_ts);
                end_checkpoint(&shared_t, &result);
            })
        {
            shared.track_background(Some(handle));
            return;
        }
    }
    let result = checkpoint_pinned(shared, Trigger::Automatic, lsn, snapshot_ts);
    end_checkpoint(shared, &result);
}

#[cfg(test)]
mod tests;
