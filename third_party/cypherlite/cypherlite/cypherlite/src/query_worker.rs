//! The per-connection query worker (ADR-0259): one parked 64 MiB thread
//! per connection that every statement runs on, so a deep plan cannot
//! overflow the caller's stack. A panicking statement is contained to the
//! worker, which catches it and keeps serving the connection's later
//! statements. Extracted from `connection.rs` to keep that module under
//! the repository's file-size budget.
//!
//! The module also owns the waiting caller's [`Heartbeat`] (ADR-0312): the
//! type lives beside the wait loop that arms it and is re-exported at the
//! crate root as `cypherlite::Heartbeat`. On wasm32 there is no worker
//! thread — only the type compiles there.

use std::time::Duration;

use crate::{Error, Result};

#[cfg(not(target_arch = "wasm32"))]
use std::panic::{self, AssertUnwindSafe};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
#[cfg(not(target_arch = "wasm32"))]
use std::thread::JoinHandle;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

/// A callback the calling thread runs while a statement (or a COMMIT)
/// executes on the connection's query worker (ADR-0312, ADR-0259).
///
/// The first tick fires one `interval` after the wait starts and then once
/// per further `interval` until the work finishes; work that finishes
/// sooner never ticks and never arms a timer. `tick` runs on the calling
/// thread, so it may use state the caller owns (the Bolt server writes a
/// keep-alive NOOP to its socket).
///
/// The fields are private: [`Heartbeat::every`] is the only constructor,
/// so a zero interval — which would tick without ever waiting — cannot be
/// built.
// The fields are read by the native wait loop below; wasm32 compiles only
// the type, so there is no reader to warn about there.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub struct Heartbeat<'a> {
    interval: Duration,
    tick: &'a mut dyn FnMut(),
}

impl<'a> Heartbeat<'a> {
    /// A heartbeat that ticks `tick` once per elapsed `interval`.
    ///
    /// # Errors
    ///
    /// `interval` must be non-zero: a zero interval would tick back to
    /// back for the whole statement (a hot keep-alive loop), so it is
    /// rejected here, at the boundary, instead of hot-spinning later.
    ///
    /// ```
    /// use std::time::Duration;
    /// use cypherlite::Heartbeat;
    ///
    /// let mut tick = || {};
    /// // The constructor enforces the non-zero contract.
    /// assert!(Heartbeat::every(Duration::ZERO, &mut tick).is_err());
    /// let heartbeat = Heartbeat::every(Duration::from_secs(60), &mut tick).unwrap();
    /// drop(heartbeat);
    /// ```
    pub fn every(interval: Duration, tick: &'a mut dyn FnMut()) -> Result<Self> {
        if interval.is_zero() {
            return Err(Error::semantic("heartbeat interval must be non-zero"));
        }
        Ok(Self { interval, tick })
    }
}

/// Stack size of the connection query worker: 64 MiB (67 108 864 bytes),
/// matching the size the test suite and the ADR-0257 limit tests already
/// use (ADR-0259).
#[cfg(not(target_arch = "wasm32"))]
const QUERY_WORKER_STACK_BYTES: usize = 64 * 1024 * 1024;

/// One parked 64 MiB thread and the slot a caller uses to hand it a job.
///
/// The job borrows the caller's `Connection`, so it cannot be `'static`.
/// The caller publishes the closure's address and blocks until the worker
/// signals completion. `Connection` is `!Sync` and the caller holds
/// `&mut Connection` for the whole call, so nothing else can publish a job
/// or drop the connection while the pointer is live. The worker does not
/// touch the pointer after that signal.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct QueryWorker {
    pub(crate) slot: Arc<JobSlot>,
    handle: Option<JoinHandle<()>>,
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct JobSlot {
    state: Mutex<SlotState>,
    work: Condvar,
    done: Condvar,
}

/// `job` is the caller's stack closure, erased to a thin pointer plus the
/// function that invokes it. It is taken under `state` and called only by
/// the worker, which the caller is waiting on.
#[cfg(not(target_arch = "wasm32"))]
struct SlotState {
    job: Option<ErasedJob>,
    finished: bool,
    panic: Option<Box<dyn std::any::Any + Send>>,
    shutdown: bool,
}

/// A non-`'static` `FnMut` handed to the worker. The data pointer is the
/// caller's stack closure; `invoke` is a monomorphized trampoline.
#[cfg(not(target_arch = "wasm32"))]
struct ErasedJob {
    ptr: *mut (),
    invoke: unsafe fn(*mut ()),
}

// Safety: the pointer is published and taken under `state`'s mutex, and the
// worker calls it only while the caller is blocked in
// `JobSlot::run_with_heartbeat`.
#[cfg(not(target_arch = "wasm32"))]
unsafe impl Send for ErasedJob {}

#[cfg(not(target_arch = "wasm32"))]
impl ErasedJob {
    fn new<F: FnMut()>(job: &mut F) -> Self {
        unsafe fn invoke<F: FnMut()>(ptr: *mut ()) {
            unsafe { (*ptr.cast::<F>())() }
        }
        Self {
            ptr: job as *mut F as *mut (),
            invoke: invoke::<F>,
        }
    }

    /// Safety: `ptr` must still be the closure passed to [`ErasedJob::new`],
    /// and this must not run after that closure is dropped.
    unsafe fn call(self) {
        unsafe { (self.invoke)(self.ptr) }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl QueryWorker {
    pub(crate) fn spawn() -> Result<Self> {
        let slot = Arc::new(JobSlot {
            state: Mutex::new(SlotState {
                job: None,
                finished: false,
                panic: None,
                shutdown: false,
            }),
            work: Condvar::new(),
            done: Condvar::new(),
        });
        let worker_slot = Arc::clone(&slot);
        let handle = std::thread::Builder::new()
            .name("cypherlite-query".into())
            .stack_size(QUERY_WORKER_STACK_BYTES)
            .spawn(move || worker_loop(worker_slot))
            .map_err(|e| {
                Error::internal(format!(
                    "failed to spawn query worker thread (64 MiB stack): {e}"
                ))
            })?;
        Ok(Self {
            slot,
            handle: Some(handle),
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for QueryWorker {
    fn drop(&mut self) {
        {
            let mut guard = lock_slot(&self.slot.state);
            guard.shutdown = true;
            self.slot.work.notify_one();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn lock_slot(state: &Mutex<SlotState>) -> std::sync::MutexGuard<'_, SlotState> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(not(target_arch = "wasm32"))]
fn worker_loop(slot: Arc<JobSlot>) {
    loop {
        let job = {
            let mut guard = lock_slot(&slot.state);
            while guard.job.is_none() && !guard.shutdown {
                guard = slot.work.wait(guard).unwrap_or_else(|e| e.into_inner());
            }
            let Some(job) = guard.job.take() else {
                break;
            };
            job
        };
        // Safety: the caller is blocked in `JobSlot::run_with_heartbeat`
        // and keeps the closure alive until `finished` is set below. No
        // other thread publishes into this slot.
        let panicked = panic::catch_unwind(AssertUnwindSafe(|| unsafe { job.call() }));
        let mut guard = lock_slot(&slot.state);
        if let Err(payload) = panicked {
            guard.panic = Some(payload);
        }
        guard.finished = true;
        slot.done.notify_one();
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl JobSlot {
    /// Run `f` on the worker and wait for it (ADR-0312): publish the erased
    /// job, wait for `finished` in [`JobSlot::wait`], then resume a tick
    /// panic or report a worker panic.
    ///
    /// A panic in the tick cannot unwind this frame while the worker still
    /// executes the erased job, whose raw pointer targets this frame's
    /// locals: `wait` stashes it and this frame keeps waiting until the job
    /// finishes, and only then is the panic resumed (ADR-0334). Without
    /// that, a panicking tick would unwind before `finished` is set and
    /// leave the worker dereferencing a dangling stack pointer.
    pub(crate) fn run_with_heartbeat<T: Send>(
        &self,
        f: impl FnOnce() -> T + Send,
        heartbeat: Option<Heartbeat<'_>>,
    ) -> Result<T> {
        let mut out: Option<T> = None;
        let mut f = Some(f);
        let mut job = || {
            let f = f.take().expect("query worker job runs once");
            out = Some(f());
        };
        let erased = ErasedJob::new(&mut job);
        let (mut guard, tick_panic) = {
            let mut guard = lock_slot(&self.state);
            guard.job = Some(erased);
            guard.finished = false;
            self.work.notify_one();
            self.wait(guard, heartbeat)
        };
        if let Some(payload) = tick_panic {
            // The worker has finished and no longer touches this frame.
            // Resume the caller's own panic now; a worker panic recorded
            // alongside it is superseded by it and must not outlive this
            // call as stale slot state.
            guard.panic = None;
            drop(guard);
            panic::resume_unwind(payload);
        }
        if let Some(payload) = guard.panic.take() {
            return Err(worker_panic_error(&*payload));
        }
        drop(guard);
        Ok(out.expect("query worker finished without a value"))
    }

    /// Wait for the published job to finish, ticking `heartbeat` — when
    /// armed — once per interval on this (the waiting) thread, with the
    /// slot unlocked (ADR-0312). Without one the wait is untimed, so a job
    /// that finishes first never pays for a timer.
    ///
    /// Returns the locked slot state and the payload of a panicking tick,
    /// if any. The payload is stashed, never raised here: this frame must
    /// not unwind while the worker may still call the erased job that
    /// points into it. A stashed panic — or a tick whose deadline cannot
    /// be represented — drops the timer with the tick, and the wait
    /// finishes untimed: a dead tick has no consumer left, so no timer
    /// survives it (ADR-0334).
    fn wait<'a>(
        &'a self,
        guard: MutexGuard<'a, SlotState>,
        heartbeat: Option<Heartbeat<'_>>,
    ) -> (
        MutexGuard<'a, SlotState>,
        Option<Box<dyn std::any::Any + Send>>,
    ) {
        let (mut guard, tick_panic) = match heartbeat {
            Some(beat) => self.wait_ticked(guard, beat),
            None => (guard, None),
        };
        while !guard.finished {
            guard = self.done.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
        (guard, tick_panic)
    }

    /// The ticked half of [`JobSlot::wait`]: wait for the published job to
    /// finish, ticking `beat` on this (the waiting) thread once per
    /// interval, with the slot unlocked (ADR-0312). Returns the locked
    /// slot state and the payload of a panicking tick, if any.
    ///
    /// The tick panic is stashed, never raised here: this frame must not
    /// unwind while the worker may still call the erased job that points
    /// into it. A panicking tick drops the timer with the tick and the
    /// wait finishes untimed — a dead tick has no consumer left, so no
    /// timer survives it (ADR-0334). The deadline arithmetic is fallible
    /// too: an interval `Instant` cannot represent from now yields no
    /// timer, because such a tick can never fire.
    fn wait_ticked<'a>(
        &'a self,
        mut guard: MutexGuard<'a, SlotState>,
        beat: Heartbeat<'_>,
    ) -> (
        MutexGuard<'a, SlotState>,
        Option<Box<dyn std::any::Any + Send>>,
    ) {
        let mut tick_panic: Option<Box<dyn std::any::Any + Send>> = None;
        let mut next_tick = next_deadline(&beat);
        while !guard.finished {
            // A deadline `Instant` cannot represent (only an interval
            // that large reaches this) can never arrive: every later
            // `now` overflows too, so the tick cannot fire. Drop the
            // timer and let the untimed wait finish the job, like the
            // dead-tick path. Computing the deadline fallibly keeps
            // this frame from ever unwinding while the worker may
            // still call the erased job (ADR-0334).
            let Some(deadline) = next_tick else {
                break;
            };
            let now = Instant::now();
            if now < deadline {
                guard = self
                    .done
                    .wait_timeout(guard, deadline - now)
                    .map(|(guard, _)| guard)
                    .unwrap_or_else(|e| e.into_inner().0);
                continue;
            }
            // The caller's closure stays alive across the tick: this
            // frame does not return until `finished` is set, and a
            // panic in the tick is stashed rather than raised while
            // the worker may still call it.
            drop(guard);
            tick_panic = panic::catch_unwind(AssertUnwindSafe(|| (beat.tick)())).err();
            guard = lock_slot(&self.state);
            if tick_panic.is_some() {
                // The tick is dead: stop re-arming the timer; the
                // untimed wait finishes the job.
                break;
            }
            next_tick = next_deadline(&beat);
        }
        (guard, tick_panic)
    }
}

/// Arm the next tick of `beat`: the instant it fires, or `None` when the
/// interval reaches past `Instant`'s range — such a deadline can never
/// arrive, so no timer is armed and the wait finishes untimed.
#[cfg(not(target_arch = "wasm32"))]
fn next_deadline(beat: &Heartbeat<'_>) -> Option<Instant> {
    Instant::now().checked_add(beat.interval)
}

/// Extract a human-readable message from a panic payload.
#[cfg(not(target_arch = "wasm32"))]
fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn worker_panic_error(payload: &(dyn std::any::Any + Send)) -> Error {
    Error::internal(format!(
        "query worker thread panicked: {}",
        panic_payload_message(payload)
    ))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::{Heartbeat, QueryWorker};

    /// ADR-0334 test scaffold: spawn a worker, count the heartbeat's
    /// fires, run `job` on the worker under a heartbeat every `interval`
    /// whose tick counts its fire and then runs `tick_body`, and contain a
    /// tick panic in the frame around the call. Returns the caught
    /// outcome, the tick counter and the still-live worker, so a caller
    /// can assert on the slot afterwards.
    fn run_under_tick<T: Send>(
        interval: Duration,
        tick_body: impl FnMut(),
        job: impl FnOnce(&Arc<AtomicUsize>) -> T + Send,
    ) -> (
        std::thread::Result<crate::Result<T>>,
        Arc<AtomicUsize>,
        QueryWorker,
    ) {
        let worker = QueryWorker::spawn().unwrap();
        let ticks = Arc::new(AtomicUsize::new(0));
        let mut tick_body = tick_body;
        let mut tick = {
            let ticks = Arc::clone(&ticks);
            move || {
                ticks.fetch_add(1, Ordering::SeqCst);
                tick_body();
            }
        };
        let heartbeat = Heartbeat::every(interval, &mut tick).unwrap();
        let job_ticks = Arc::clone(&ticks);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker
                .slot
                .run_with_heartbeat(move || job(&job_ticks), Some(heartbeat))
        }));
        (result, ticks, worker)
    }

    /// The panicking variant of [`run_under_tick`]: the tick dies on its
    /// first fire (ADR-0334).
    fn run_under_panicking_tick<T: Send>(
        job: impl FnOnce(&Arc<AtomicUsize>) -> T + Send,
    ) -> (
        std::thread::Result<crate::Result<T>>,
        Arc<AtomicUsize>,
        QueryWorker,
    ) {
        run_under_tick(
            Duration::from_millis(1),
            || panic!("injected tick panic"),
            job,
        )
    }

    /// The non-panicking variant of [`run_under_tick`]: the tick only
    /// counts its fires.
    fn run_under_live_tick<T: Send>(
        interval: Duration,
        job: impl FnOnce(&Arc<AtomicUsize>) -> T + Send,
    ) -> (
        std::thread::Result<crate::Result<T>>,
        Arc<AtomicUsize>,
        QueryWorker,
    ) {
        run_under_tick(interval, || {}, job)
    }

    /// ADR-0334: a panicking tick must not unwind the waiting frame while
    /// the worker can still call the erased job (which points into that
    /// frame). The panic resumes only after the job has finished.
    #[test]
    fn a_panicking_tick_waits_for_the_job_before_unwinding() {
        let done = Arc::new(AtomicBool::new(false));
        let job_done = Arc::clone(&done);
        let (result, _ticks, _worker) = run_under_panicking_tick(move |ticks| {
            // The job outlives the tick's panic: it only finishes once
            // the tick has fired, and the unwinding caller must keep
            // waiting for it.
            while ticks.load(Ordering::SeqCst) == 0 {
                std::thread::yield_now();
            }
            std::thread::sleep(Duration::from_millis(200));
            job_done.store(true, Ordering::SeqCst);
        });
        assert!(result.is_err(), "the tick panic must resume");
        assert!(
            done.load(Ordering::SeqCst),
            "the job must finish before the tick panic unwinds the caller"
        );
    }

    /// The worker still runs jobs after a tick panic was resumed, and the
    /// panic stashed by the aborted call is not returned to the next one.
    #[test]
    fn the_slot_stays_usable_after_a_tick_panic() {
        let (result, _ticks, worker) = run_under_panicking_tick(|ticks| {
            while ticks.load(Ordering::SeqCst) == 0 {
                std::thread::yield_now();
            }
        });
        assert!(result.is_err());
        assert_eq!(worker.slot.run_with_heartbeat(|| 7u32, None).unwrap(), 7);
    }

    /// ADR-0334: once a tick panics it stays dead for the rest of the job
    /// — the wait switches to the untimed loop, so the tick runs exactly
    /// once however many intervals the job outlives it, and the panic
    /// still resumes only after the job has finished. (A timer-free wait
    /// is not observable from outside; the structural half is the break
    /// to the untimed loop. This pins the visible half, which a
    /// re-armed tick would break.)
    #[test]
    fn a_tick_panic_leaves_the_tick_dead_for_the_rest_of_the_job() {
        let (result, ticks, _worker) = run_under_panicking_tick(|ticks| {
            // The job waits for the first tick, then outlives many
            // intervals after its panic.
            while ticks.load(Ordering::SeqCst) == 0 {
                std::thread::yield_now();
            }
            std::thread::sleep(Duration::from_millis(50));
        });
        assert!(result.is_err(), "the tick panic must resume");
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            1,
            "a dead tick must not be re-armed or re-run"
        );
    }

    /// ADR-0334: an interval `Instant` cannot represent from now — only a
    /// zero interval is rejected at construction — must not panic the
    /// waiting frame. The unchecked `Instant + Duration` did, unwinding
    /// the frame the worker's erased job points into. Such a deadline can
    /// never arrive, so the tick cannot fire: the wait finishes untimed
    /// when the job finishes.
    #[test]
    fn an_unrepresentable_tick_deadline_does_not_panic_the_wait() {
        let (result, ticks, _worker) = run_under_live_tick(Duration::MAX, |_| {
            // Hold the job open briefly so the untimed wait is the one
            // that observes the finish.
            std::thread::sleep(Duration::from_millis(20));
            7u32
        });
        assert_eq!(result.unwrap().unwrap(), 7);
        assert_eq!(
            ticks.load(Ordering::SeqCst),
            0,
            "a deadline beyond Instant's range can never fire"
        );
    }

    /// A zero interval is rejected at construction: it would tick without
    /// ever waiting.
    #[test]
    fn a_zero_heartbeat_interval_is_rejected() {
        let mut tick = || {};
        let err = Heartbeat::every(Duration::ZERO, &mut tick)
            .err()
            .expect("a zero interval is rejected");
        assert!(
            err.to_string().contains("non-zero"),
            "unexpected message: {err}"
        );
    }
}
