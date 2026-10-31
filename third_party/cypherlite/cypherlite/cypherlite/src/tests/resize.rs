//! Follower/writer toggle (ADR-0440, engine ADR-0440): `Database::promote`
//! and `Database::demote`, and the draining admission gate they share with
//! every commit path.

use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Duration;

use crate::test_opts::{count, following, read_only, wait_count, writable};
use crate::{Database, Error, FollowGapPolicy, OpenOptions};

#[test]
fn promote_refuses_a_handle_not_opened_in_follow_mode() {
    let dir = tempfile::tempdir().unwrap();
    let uri = dir.path().join("db");
    let uri = uri.to_str().unwrap();

    let writer = Database::open_with(uri, writable()).unwrap();
    writer.promote().unwrap();
    assert!(
        !writer.is_read_only(),
        "promote on the writable opener is a no-op"
    );
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();

    let reader = Database::open_with(uri, read_only()).unwrap();
    let err = reader.promote().unwrap_err();
    assert!(
        matches!(err, Error::Semantic(_)),
        "a frozen read-only open can never become the writer: {err}"
    );
}

#[test]
fn a_writable_open_demotes_and_promotes_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    writer.demote().unwrap();
    assert!(writer.is_read_only());

    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Seed", 1);
    follower.promote().unwrap();
    assert!(!follower.is_read_only());
    let err = writer.promote().unwrap_err();
    assert!(matches!(err, Error::Locked(_)), "{err}");

    follower.demote().unwrap();
    writer.promote().unwrap();
    assert!(!writer.is_read_only());
    assert!(follower.is_read_only());
}

#[test]
fn promote_and_demote_are_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    drop(writer);

    let follower = Database::open_with(uri, following()).unwrap();
    assert!(follower.is_read_only());
    let reloads = follower.follow_reloads();
    follower.promote().unwrap();
    assert_eq!(
        follower.follow_reloads(),
        reloads,
        "a caught-up promote keeps the loaded graph"
    );
    assert!(!follower.is_read_only());
    // Already the writer: a second call is a no-op, not an error.
    follower.promote().unwrap();
    assert!(!follower.is_read_only());

    follower.demote().unwrap();
    assert!(follower.is_read_only());
    // Already a follower: a second call is a no-op, not an error.
    follower.demote().unwrap();
    assert!(follower.is_read_only());
}

#[test]
fn promoted_follower_accepts_writes_and_a_fully_demoted_follower_refuses_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    drop(writer);

    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Seed", 1);
    follower.promote().unwrap();
    follower
        .connect()
        .unwrap()
        .execute("CREATE (:FromPromoted)")
        .unwrap();
    assert_eq!(count(&follower, "FromPromoted"), 1);

    // No write races the drain in this single-threaded test, so by the
    // time `demote` returns it has already fully completed: a later write
    // sees the ordinary read-only-follower refusal, not the transient
    // draining one `ensure_writable_refuses_retryably_only_while_draining`
    // covers directly.
    follower.demote().unwrap();
    let err = follower
        .connect()
        .unwrap()
        .execute("CREATE (:AfterDemote)")
        .unwrap_err();
    assert!(
        matches!(err, Error::Semantic(_)),
        "a fully demoted handle is an ordinary read-only follower: {err}"
    );
}

fn release_lease_without_checkpoint(db: &Database) {
    db.shared().persist_on_drop.store(false, Ordering::Release);
    #[cfg(not(target_arch = "wasm32"))]
    {
        *db.shared().writer_lease.lock().unwrap() = None;
    }
}

#[test]
fn promote_applies_an_uncheckpointed_tail_without_reloading() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Seed", 1);
    follower.park_follow();
    writer.connect().unwrap().execute("CREATE (:Tail)").unwrap();
    release_lease_without_checkpoint(&writer);
    drop(writer);

    let reloads = follower.follow_reloads();
    follower.resume_follow();
    follower.promote().unwrap();
    assert_eq!(follower.follow_reloads(), reloads);
    assert_eq!(count(&follower, "Tail"), 1);
    follower
        .connect()
        .unwrap()
        .execute("CREATE (:FromPromoted)")
        .unwrap();
    assert_eq!(count(&follower, "FromPromoted"), 1);
}

#[test]
fn promote_does_not_wait_for_a_snapshot_pin() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    drop(writer);

    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Seed", 1);
    let mut pinned = follower.connect().unwrap();
    pinned.begin().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(follower.promote());
    });
    let promoted = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("promote waited for a snapshot pin");
    promoted.unwrap();
}

fn yielding() -> OpenOptions {
    OpenOptions {
        follow_gap: FollowGapPolicy::Yield,
        ..following()
    }
}

fn promote_across_a_recycled_wal(opts: OpenOptions) -> (tempfile::TempDir, Database, u64) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    let follower = Database::open_with(uri, opts).unwrap();
    wait_count(&follower, "Seed", 1);
    let reloads = follower.follow_reloads();
    follower.park_follow();
    writer
        .connect()
        .unwrap()
        .execute("CREATE (:Missed)")
        .unwrap();
    writer.checkpoint().unwrap();
    assert_eq!(count(&follower, "Missed"), 0);
    writer.demote().unwrap();
    drop(writer);
    follower.resume_follow();
    (dir, follower, reloads)
}

#[test]
fn promote_yields_across_a_checkpoint_gap_without_reloading() {
    let (_dir, follower, reloads) = promote_across_a_recycled_wal(yielding());
    let err = match follower.promote() {
        Err(err) => err,
        Ok(()) => panic!("promote loaded a second graph across a checkpoint gap"),
    };
    assert!(
        matches!(
            err,
            Error::ResourceLimit {
                name: Error::FOLLOW_GAP,
                retryable: true,
                ..
            }
        ),
        "{err}"
    );
    assert!(follower.needs_reopen());
    assert_eq!(follower.follow_reloads(), reloads);
    assert!(follower.is_read_only());
}

#[test]
fn promote_reload_policy_still_reloads_across_a_checkpoint_gap() {
    let (_dir, follower, reloads) = promote_across_a_recycled_wal(following());
    follower.promote().unwrap();
    assert!(follower.follow_reloads() > reloads);
    assert_eq!(count(&follower, "Missed"), 1);
    assert!(!follower.is_read_only());
    follower
        .connect()
        .unwrap()
        .execute("CREATE (:FromPromoted)")
        .unwrap();
    assert_eq!(count(&follower, "FromPromoted"), 1);
}

#[test]
fn demote_keeps_the_loaded_graph_and_still_tails_the_wal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    drop(writer);

    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Seed", 1);
    let reloads = follower.follow_reloads();
    follower.promote().unwrap();
    follower
        .connect()
        .unwrap()
        .execute("CREATE (:BeforeDemote)")
        .unwrap();
    follower.demote().unwrap();

    assert_eq!(follower.follow_reloads(), reloads);
    assert_eq!(count(&follower, "BeforeDemote"), 1);

    let next = Database::open_with(uri, writable()).unwrap();
    next.connect()
        .unwrap()
        .execute("CREATE (:AfterDemote)")
        .unwrap();
    wait_count(&follower, "AfterDemote", 1);
    assert_eq!(follower.follow_reloads(), reloads);
}

#[test]
fn demote_checkpoints_before_releasing_the_lease() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    drop(writer);

    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Seed", 1);
    follower.promote().unwrap();
    follower
        .connect()
        .unwrap()
        .execute("CREATE (:Checkpointed)")
        .unwrap();
    follower.demote().unwrap();

    // A brand-new follower loads from the checkpoint `demote` wrote, not
    // from a tailed WAL this process still owns — proof the checkpoint
    // actually landed before the lease was released.
    let fresh = Database::open_with(uri, following()).unwrap();
    assert_eq!(count(&fresh, "Checkpointed"), 1);
}

#[test]
fn two_handoffs_hand_the_writer_role_back_and_forth() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let bootstrap = Database::open_with(uri, writable()).unwrap();
    bootstrap
        .connect()
        .unwrap()
        .execute("CREATE (:Seed)")
        .unwrap();
    bootstrap.checkpoint().unwrap();
    drop(bootstrap);

    // name-0 promotes first, as the two-handoff sequence in the plan
    // requires the surviving writer to end on the lowest ordinal.
    let p0 = Database::open_with(uri, following()).unwrap();
    wait_count(&p0, "Seed", 1);
    p0.promote().unwrap();
    p0.connect().unwrap().execute("CREATE (:FromP0)").unwrap();

    // Handoff 1: name-1 catches up, p0 demotes, name-1 promotes.
    let p1 = Database::open_with(uri, following()).unwrap();
    p0.demote().unwrap();
    wait_count(&p1, "FromP0", 1);
    p1.promote().unwrap();
    p1.connect().unwrap().execute("CREATE (:FromP1)").unwrap();

    // Handoff 2: p0 (now a follower again) catches back up, p1 demotes,
    // p0 promotes — ending with the original handle as the writer again.
    wait_count(&p0, "FromP1", 1);
    p1.demote().unwrap();
    p0.promote().unwrap();
    p0.connect()
        .unwrap()
        .execute("CREATE (:FromP0Again)")
        .unwrap();

    assert_eq!(count(&p0, "FromP0Again"), 1);
    assert!(p1.is_read_only());
    assert!(!p0.is_read_only());
}

#[test]
fn a_second_promote_on_a_different_handle_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let bootstrap = Database::open_with(uri, writable()).unwrap();
    bootstrap
        .connect()
        .unwrap()
        .execute("CREATE (:Seed)")
        .unwrap();
    bootstrap.checkpoint().unwrap();
    drop(bootstrap);

    let a = Database::open_with(uri, following()).unwrap();
    wait_count(&a, "Seed", 1);
    a.promote().unwrap();

    let b = Database::open_with(uri, following()).unwrap();
    let err = b.promote().unwrap_err();
    assert!(
        matches!(err, Error::Locked(_)),
        "a second writer lease on the same store fails the same way a second \
         Writable open would: {err}"
    );
}

#[test]
fn ensure_writable_refuses_retryably_only_while_draining() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let uri = path.to_str().unwrap();
    let writer = Database::open_with(uri, writable()).unwrap();
    writer.connect().unwrap().execute("CREATE (:Seed)").unwrap();
    writer.checkpoint().unwrap();
    drop(writer);

    let follower = Database::open_with(uri, following()).unwrap();
    wait_count(&follower, "Seed", 1);
    let err = crate::ensure_writable(&follower.shared()).unwrap_err();
    assert_eq!(
        crate::binding_error_metadata(&err).0,
        "WriteOnReadOnlyAccessDatabase",
        "a follower write is the fixed 42N18 error"
    );
    follower.promote().unwrap();
    let shared = follower.shared();
    assert!(crate::ensure_writable(&shared).is_ok());

    shared.draining.store(true, Ordering::Release);
    let err = crate::ensure_writable(&shared).unwrap_err();
    assert!(matches!(
        err,
        Error::ResourceLimit {
            name: Error::WRITER_DRAINING,
            retryable: true,
            ..
        }
    ));
    // Draining refuses even though the handle is still the writer
    // underneath — demote has not released the lease yet, matching the
    // plan's "Ready until the drain finishes" contract for db-api.
    assert!(!follower.is_read_only());
    shared.draining.store(false, Ordering::Release);

    assert!(crate::ensure_writable(&shared).is_ok());
}
