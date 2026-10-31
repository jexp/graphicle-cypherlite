//! Tail a native WAL and reload when a checkpoint leaves a gap (ADR-0137).

use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::Duration;

use cypherlite_storage::{published_checkpoint_lsn, WalCatchUp};

use crate::checkpoint::{self, begin_reload_swap};
#[cfg(test)]
use crate::Database;
use crate::{Result, Shared, ThreadStop};

#[cfg(test)]
#[derive(Default)]
pub(crate) struct FollowTest {
    pub paused: bool,
    pub parked: bool,
    pub reloads: u64,
    /// Fail the 1-based apply in the next batch, then clear.
    pub fail_apply_on: u32,
    /// The next `reload` returns an error, then clears.
    pub fail_reload: bool,
    /// The next poll returns an error before it reads the WAL, then clears.
    pub fail_poll: bool,
}

#[cfg(test)]
impl FollowTest {
    /// Consume the armed poll failure (fire once, then clear).
    pub(crate) fn take_fail_poll(&mut self) -> bool {
        std::mem::take(&mut self.fail_poll)
    }

    /// Consume the armed reload failure (fire once, then clear).
    pub(crate) fn take_fail_reload(&mut self) -> bool {
        std::mem::take(&mut self.fail_reload)
    }

    /// Whether this batch's 1-based `index` is the armed apply failure,
    /// consuming it (fire once, then clear).
    pub(crate) fn take_fail_apply(&mut self, index: usize) -> bool {
        if self.fail_apply_on > 0 && index as u32 + 1 == self.fail_apply_on {
            self.fail_apply_on = 0;
            return true;
        }
        false
    }
}

const POLL: Duration = Duration::from_millis(50);
const PIN_WAIT: Duration = Duration::from_millis(1);
#[cfg(test)]
const PIN_STALL_AFTER: u32 = 2;
#[cfg(not(test))]
const PIN_STALL_AFTER: u32 = 5_000;

/// The follower's test gate under its poison-recovery lock — the one
/// acquire every consumer shares (the follower's poll/apply/reload hooks
/// and the Database test controls below).
#[cfg(test)]
fn follow_test_gate(shared: &Shared) -> std::sync::MutexGuard<'_, FollowTest> {
    shared
        .follow_test
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

/// Spawn the follow thread and return its handle. Before ADR-0440 this
/// detached the handle immediately — a paused follower waits on its own
/// gate, and joining it from `Database::drop` would stall until that gate
/// opens — and that is still true: `Database::drop` reaches this thread
/// only through [`crate::shared::Shared::signal_background`], which
/// signals without joining. `Database::promote` is the one caller that
/// does join: it needs the thread fully stopped, not just signaled,
/// before it takes the writer lease.
pub(crate) fn spawn(weak: Weak<Shared>, stop: Arc<ThreadStop>) -> Result<JoinHandle<()>> {
    crate::spawn_interval_worker("cypherlite-follow", weak, stop, POLL, follow_tick)?
        .ok_or_else(|| cypherlite_core::Error::internal("follow thread did not spawn"))
}

fn follow_tick(shared: &Shared) {
    park_while_paused(shared);
    if shared.follow_failed() {
        return;
    }
    match catch_up(shared) {
        Ok(_) => shared.clear_transient_failure(),
        Err(err) => shared.record_failure(err),
    }
}

/// Apply every surviving WAL frame onto the loaded graph. `Ok` means the
/// poll is idle: this handle has the tip. A gap or a torn apply follows
/// [`FollowGapPolicy`](crate::FollowGapPolicy) and may return `Err`.
///
/// The caller holds the writer lease, so no new commit appears between
/// polls.
pub(crate) fn catch_up_to_tip(shared: &Shared) -> Result<()> {
    loop {
        if catch_up(shared)? {
            return Ok(());
        }
    }
}

fn park_while_paused(shared: &Shared) {
    #[cfg(test)]
    {
        let mut gate = follow_test_gate(shared);
        if !gate.paused {
            gate.parked = false;
            return;
        }
        gate.parked = true;
        shared.follow_test_cv.notify_all();
        while gate.paused {
            gate = match shared.follow_test_cv.wait(gate) {
                Ok(gate) => gate,
                Err(err) => err.into_inner(),
            };
        }
        gate.parked = false;
    }
    #[cfg(not(test))]
    {
        let _ = shared;
    }
}

/// `Ok(true)` when the poll is idle (or a `Reload` gap has installed its
/// snapshot). `Ok(false)` when a batch was applied and another poll may
/// still find frames.
fn catch_up(shared: &Shared) -> Result<bool> {
    #[cfg(test)]
    {
        let mut gate = follow_test_gate(shared);
        if gate.take_fail_poll() {
            return Err(cypherlite_core::Error::storage(
                "injected follow poll failure",
            ));
        }
    }
    let checkpoint_lsn = published_checkpoint_lsn(&shared.store)?;
    let caught = {
        let mut wal = shared.wal.lock()?;
        wal.poll_follow(checkpoint_lsn)?
    };
    match caught {
        WalCatchUp::Idle => Ok(true),
        WalCatchUp::Records { records, cursor } => {
            // A transient error from the previous poll is still stored. This
            // apply is what clears it, so it must not be refused by it.
            let read = shared.follow_apply_guard()?;
            for (index, record) in records.into_iter().enumerate() {
                if injected_apply_failure(shared, index) {
                    drop(read);
                    recover_partial_apply(shared)?;
                    return Ok(true);
                }
                if shared
                    .graph
                    .apply_redo(record.ops, record.commit_ts)
                    .is_err()
                {
                    drop(read);
                    recover_partial_apply(shared)?;
                    return Ok(true);
                }
            }
            drop(read);
            let mut wal = shared.wal.lock()?;
            wal.commit_follow_cursor(cursor);
            let lsn = wal.lsn;
            drop(wal);
            shared.publish_safe_horizon(lsn, shared.graph.commit_ts());
            Ok(false)
        }
        WalCatchUp::Gap => {
            recover_gap(shared)?;
            Ok(true)
        }
    }
}

/// A gap reloads beside the live graph, or latches `follow-gap` when the
/// host asked to reopen this engine itself (ADR-0441).
fn recover_gap(shared: &Shared) -> Result<()> {
    if yields(shared) {
        yield_for_reopen(shared)
    } else {
        reload(shared)
    }
}

fn yields(shared: &Shared) -> bool {
    shared.resolved.follow_gap == crate::FollowGapPolicy::Yield
}

/// Stop this follower without loading a second graph. New queries see a
/// retryable `follow-gap` limit, and [`crate::Database::needs_reopen`]
/// becomes true. The follow loop does not try again.
fn yield_for_reopen(shared: &Shared) -> Result<()> {
    let err = cypherlite_core::Error::resource_limit(cypherlite_core::Error::FOLLOW_GAP, 0, true);
    shared.record_permanent_failure(err.clone());
    Err(err)
}

fn injected_apply_failure(shared: &Shared, index: usize) -> bool {
    #[cfg(test)]
    {
        let mut gate = follow_test_gate(shared);
        if gate.take_fail_apply(index) {
            return true;
        }
    }
    #[cfg(not(test))]
    {
        let _ = (shared, index);
    }
    false
}

/// The in-memory graph already contains a prefix of this batch. Replace it
/// from disk, unless this follower yields: then the host drops the engine
/// instead of loading a second graph. If a reload fails, stop the follower
/// for the life of the process.
fn recover_partial_apply(shared: &Shared) -> Result<()> {
    if yields(shared) {
        return yield_for_reopen(shared);
    }
    match reload(shared) {
        Ok(()) => Ok(()),
        Err(err) => {
            shared.record_permanent_failure(err.clone());
            Err(err)
        }
    }
}

/// Reload the loaded graph and WAL from disk, read-only (ADR-0137). Used
/// by the follow tick after a gap. [`Database::demote`] does not call this:
/// the graph it just checkpointed is already current, and the follow thread
/// only needs the WAL reopened read-only.
pub(crate) fn reload(shared: &Shared) -> Result<()> {
    #[cfg(test)]
    {
        let mut gate = follow_test_gate(shared);
        if gate.take_fail_reload() {
            return Err(cypherlite_core::Error::storage(
                "injected follow reload failure",
            ));
        }
    }
    // The replacement is private until the swap. Queries keep reading the
    // installed graph for the whole disk load.
    let (graph, wal, ontology) = crate::open::open_store_readonly_screened(&shared.store)?;
    crate::check_loaded_graph(&graph)?;
    // An open statement or explicit transaction holds a pin after it has
    // dropped `read_guard`. Wait until those pins are gone, and hold the
    // publication write lock across the swap so a new pin cannot start.
    let publication =
        wait_for_unpinned_publication(shared, "follow reload is waiting for a snapshot pin")?;
    // `install_reloaded` carries the services bundle, catalog, and the
    // checkpoint's ontology document; see its doc.
    shared.install_reloaded(&publication, graph, wal, ontology)?;
    // Count before the write unlock. A query that then observes the new
    // graph happens-after this increment.
    #[cfg(test)]
    {
        let mut gate = follow_test_gate(shared);
        gate.reloads = gate.reloads.saturating_add(1);
    }
    drop(publication);
    Ok(())
}

/// Wait until no reader holds a snapshot pin, then return the publication
/// write guard already held across that check. Shared by this module's own
/// `reload` (back to a read-only follower) and `Database::promote`'s
/// writable reload (ADR-0440): both swap the loaded graph and WAL under
/// exactly this guard, so the pin-wait loop lives once. `stall_message`
/// names what is waiting, for the health record raised after
/// `PIN_STALL_AFTER` waits — the two callers wait for different things, so
/// the message is the caller's to choose.
pub(crate) fn wait_for_unpinned_publication<'a>(
    shared: &'a Shared,
    stall_message: &'static str,
) -> Result<checkpoint::Publication<'a>> {
    let mut waits = 0u32;
    loop {
        let publication = begin_reload_swap(shared)?;
        if !shared.graph.snapshot_pinned() {
            return Ok(publication);
        }
        drop(publication);
        waits = waits.saturating_add(1);
        if waits == PIN_STALL_AFTER {
            shared.record_failure(cypherlite_core::Error::storage(stall_message));
        }
        std::thread::sleep(PIN_WAIT);
    }
}

/// The test controls of the follower (the gate the impl above parks on):
/// one poison-recovery acquire of the gate for every control — the
/// follow end-to-end suite (src/tests/follow.rs) drives the follower
/// through them.
#[cfg(test)]
impl Database {
    /// Park the follower and wait until it is parked — one operation, so a
    /// test cannot pause without waiting (the pair was mandatory anyway).
    pub(crate) fn park_follow(&self) {
        {
            let mut gate = follow_test_gate(&self.inner);
            gate.paused = true;
        }
        self.inner.follow_test_cv.notify_all();
        self.wait_follow_parked();
    }

    /// Resume the parked follower.
    pub(crate) fn resume_follow(&self) {
        {
            let mut gate = follow_test_gate(&self.inner);
            gate.paused = false;
        }
        self.inner.follow_test_cv.notify_all();
    }

    fn wait_follow_parked(&self) {
        let mut gate = follow_test_gate(&self.inner);
        let started = std::time::Instant::now();
        while !gate.parked {
            let (next, timeout) = self
                .inner
                .follow_test_cv
                .wait_timeout(gate, std::time::Duration::from_secs(2))
                .unwrap_or_else(|e| e.into_inner());
            gate = next;
            if timeout.timed_out() || started.elapsed() > std::time::Duration::from_secs(2) {
                break;
            }
        }
        assert!(gate.parked, "follower did not park");
    }

    pub(crate) fn fail_follow_apply_on(&self, n: u32) {
        follow_test_gate(&self.inner).fail_apply_on = n;
    }

    pub(crate) fn fail_next_follow_reload(&self) {
        follow_test_gate(&self.inner).fail_reload = true;
    }

    pub(crate) fn fail_next_follow_poll(&self) {
        follow_test_gate(&self.inner).fail_poll = true;
    }

    pub(crate) fn follow_reloads(&self) -> u64 {
        follow_test_gate(&self.inner).reloads
    }
}
