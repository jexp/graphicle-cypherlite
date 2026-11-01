//! `Shared` guard machinery tests: the poison-explicit health gate and the
//! reload swap's WAL-lock-first ordering. The follow end-to-end suite
//! (`crate::tests::follow`) drives the healthy reload path; these pin the
//! poisoned-lock failure paths directly.

use super::*;
use crate::checkpoint::begin_reload_swap;
use crate::test_opts::{count, writable};
use crate::Database;
use cypherlite_storage::open_store_readonly;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Lock `mutex` and panic while the guard is still alive, so the guard's
/// drop during unwind poisons the mutex. The caught panic and the poisoned
/// flag are asserted: a silent no-op must fail the test, not pass it.
fn poison<T>(mutex: &Mutex<T>) {
    poison_with(mutex, |_| ());
}

/// The general form: run `while_holding` on the locked value, then panic
/// with the guard alive, poisoning the mutex. Every poisoned-lock fixture
/// goes through here, so the scaffolding has one spelling.
fn poison_with<T>(mutex: &Mutex<T>, while_holding: impl FnOnce(&mut T)) {
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        let mut guard = mutex.lock().unwrap();
        while_holding(&mut guard);
        panic!("poison the mutex");
    }));
    assert!(panicked.is_err(), "the poisoning closure must panic");
    assert!(mutex.is_poisoned(), "the mutex must be poisoned");
}

/// The settled health state's error text, read through a poisoned guard like
/// the writers themselves, so every read-back assertion spells the lock once.
fn stored_follow_error_text(shared: &Shared) -> Option<String> {
    shared
        .follow_health
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .error()
        .map(ToString::to_string)
}

/// A poisoned `follow_health` lock must not read as a clean bill of health:
/// a thread that panicked while holding it — plausibly while recording the
/// very error it was writing — fences reads instead of passing them. The
/// pre-fix `if let Ok` skipped the slot entirely and reported healthy.
#[test]
fn check_healthy_fails_closed_on_a_poisoned_follow_health_lock() {
    let db = Database::open(":memory:").unwrap();
    let shared = db.shared();
    poison(&shared.follow_health);
    let err = shared.check_healthy().unwrap_err();
    assert!(matches!(err, Error::Internal(_)), "{err}");
}

/// The panicked-while-recording case: the error was stored before the
/// panic, and the lock is poisoned with it inside — the gate must still
/// fail closed rather than read that stored error's absence or presence
/// through a silent skip.
#[test]
fn check_healthy_fails_closed_when_poisoned_while_recording_the_error() {
    let db = Database::open(":memory:").unwrap();
    let shared = db.shared();
    poison_with(&shared.follow_health, |slot| {
        *slot = FollowHealth::Transient(Error::storage("injected follow error"));
    });
    let err = shared.check_healthy().unwrap_err();
    assert!(matches!(err, Error::Internal(_)), "{err}");
}

/// A permanently failed follower owns its stored error: neither a clear nor
/// a later transient record may overwrite the failure variant.
#[test]
fn failed_health_refuses_clear_and_transient_overwrites() {
    let db = Database::open(":memory:").unwrap();
    let shared = db.shared();
    shared.record_failure(Error::storage("transient"));
    shared.record_permanent_failure(Error::storage("permanent follow failure"));
    shared.clear_transient_failure();
    shared.record_failure(Error::storage("late transient"));
    let stored = stored_follow_error_text(&shared);
    assert!(
        stored
            .as_deref()
            .is_some_and(|text| text.contains("permanent follow failure")),
        "{stored:?}"
    );
    assert!(shared.follow_failed());
}

/// The recorder itself, not just `check_healthy`, carries the poisoned-lock
/// policy: a panic while recording must not make the next failure vanish.
#[test]
fn record_failure_records_through_a_poisoned_lock() {
    let db = Database::open(":memory:").unwrap();
    let shared = db.shared();
    poison(&shared.follow_health);
    shared.record_failure(Error::storage("recorded after poison"));
    let stored = stored_follow_error_text(&shared);
    assert!(
        stored
            .as_deref()
            .is_some_and(|text| text.contains("recorded after poison")),
        "{stored:?}"
    );
}

/// A poisoned WAL lock must abort the reload swap before any mutation. The
/// pre-fix order stored the counters, published the safe horizon, and
/// replaced the graph BEFORE taking the WAL lock, so a failed lock left a
/// half-swapped store: a stale graph over the old WAL. The lock is now
/// taken first and poison-mapped, and every swap step runs under it.
#[test]
fn install_reloaded_with_a_poisoned_wal_lock_mutates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let db = Database::open_with(uri, writable()).unwrap();
    db.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    db.checkpoint().unwrap();
    let shared = db.shared();
    // The pair a reload would install, loaded beside the live store.
    let (graph, wal) = open_store_readonly(&shared.store).unwrap();
    // These two commits land only in the live graph and WAL: an install
    // that had swapped any of them in would lose them.
    db.connect().unwrap().execute("CREATE (:Live)").unwrap();
    db.connect().unwrap().execute("CREATE (:More)").unwrap();
    let next_txn_id = shared.next_txn_id.load(Ordering::Relaxed);
    let next_node_id = shared.next_node_id.load(Ordering::Relaxed);
    let next_rel_id = shared.next_rel_id.load(Ordering::Relaxed);
    let safe_lsn = shared.safe_lsn.load(Ordering::Acquire);
    let safe_ts = shared.safe_ts.load(Ordering::Acquire);
    let live_lsn = shared.wal.lock().unwrap().lsn;

    poison(&shared.wal);

    // The reload caller's shape: the publication write lock is held across
    // the swap attempt (follow.rs `reload`).
    let publication = begin_reload_swap(&shared).unwrap();
    let err = shared
        .install_reloaded(&publication, graph, wal, None)
        .unwrap_err();
    drop(publication);

    assert!(matches!(err, Error::Internal(_)), "{err}");
    assert_eq!(shared.next_txn_id.load(Ordering::Relaxed), next_txn_id);
    assert_eq!(shared.next_node_id.load(Ordering::Relaxed), next_node_id);
    assert_eq!(shared.next_rel_id.load(Ordering::Relaxed), next_rel_id);
    assert_eq!(shared.safe_lsn.load(Ordering::Acquire), safe_lsn);
    assert_eq!(shared.safe_ts.load(Ordering::Acquire), safe_ts);
    // The live graph kept both post-load commits — the stale loaded pair
    // was not installed. Query paths take no WAL lock, so the poisoned
    // WAL does not fence this read.
    assert_eq!(count(&db, "Live"), 1);
    assert_eq!(count(&db, "More"), 1);
    assert_eq!(count(&db, "Seed"), 1);
    // The WAL itself kept its LSN: the loaded WAL was not assigned.
    let wal = shared.wal.lock().unwrap_or_else(|err| err.into_inner());
    assert_eq!(wal.lsn, live_lsn);
}
