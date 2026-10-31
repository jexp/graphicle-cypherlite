//! Pure WAL property suite (ADR-0268).
//!
//! Four invariants of the write-ahead log itself, with no checkpoint or
//! snapshot in play:
//!
//! 1. [`replay_is_deterministic`] — the same persisted records, replayed
//!    twice, produce identical graph state.
//! 2. [`replay_is_idempotent`] — replay is a pure read: reading the same WAL
//!    twice (including across a close/reopen) yields identical records.
//! 3. [`lsn_is_monotonic`] — `append_commit` never regresses or skips an LSN.
//! 4. [`torn_tail_prefix_survives`] — a torn/garbage tail is dropped without
//!    error and the committed prefix survives.
//!
//! [`reencoded_records_are_lossless`] is a codec round-trip diagnostic; it is
//! not one of the four contract properties. It was ignored while LABS-134
//! (serde_json without `float_roundtrip` loses 1 ULP on f64 read) was open;
//! that bug is now fixed and the property runs as an ordinary passing test.
//!
//! Every case draws from the shared [`proptest_support`] `RedoOp` strategies
//! so the suite and the strategy harness stay in step. Sequence seeding is
//! `proptest`'s: a failing case is reproducible via `PROPTEST_RNG_SEED`.
//!
//! [`reencoded_records_are_lossless`]: fn.reencoded_records_are_lossless.html

mod proptest_support;

use std::sync::Arc;

use cypherlite_executor::{Graph, RedoOp};
use cypherlite_storage::{read_to_vec, MemoryVfs, Vfs, VfsOpen, Wal, WalRecord};
use proptest::prelude::*;

use proptest_support::{encode_ops, wal_segment_path, WAL_PREFIX};

/// A sequence of commits: each outer element is one commit's [`RedoOp`] list.
/// Bounded so a single case stays cheap while still spanning several LSNs.
fn commit_sequence() -> impl Strategy<Value = Vec<Vec<RedoOp>>> {
    prop::collection::vec(proptest_support::redo_op_sequence(), 0..=5)
}

/// A garbage suffix that is guaranteed to be an *incomplete* frame at EOF
/// (either a partial 8-byte frame header or a header whose declared payload
/// length runs past the file end). This models a crash mid-write, the only
/// shape recovery is specified to tolerate; arbitrary bytes could legitimately
/// decode to a `len == 0` / oversized frame and hard-fail, which is correct
/// engine behavior, not a property violation.
fn torn_tail() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 1..7),
        (any::<u32>(), prop::collection::vec(any::<u8>(), 0..24)).prop_map(|(crc, payload)| {
            let mut bytes = Vec::with_capacity(8 + payload.len());
            bytes.extend_from_slice(&crc.to_le_bytes());
            // Always claim one byte more than is actually present, so recovery
            // sees an incomplete payload at EOF regardless of sequence length.
            bytes.extend_from_slice(&(payload.len() as u32 + 1).to_le_bytes());
            bytes.extend_from_slice(&payload);
            bytes
        }),
    ]
}

/// Append each commit with a strictly increasing commit timestamp, mirroring
/// how the engine stamps real commits.
fn append_all(wal: &mut Wal, commits: &[Vec<RedoOp>]) {
    for (index, ops) in commits.iter().enumerate() {
        wal.append_commit((index + 1) as u64, ops)
            .expect("WAL append");
    }
}

/// Reduce each generated commit to the ops the engine applies, feeding the
/// commits through one shared [`proptest_support::ModelGraph`] so the reduction
/// sees state across commits. The WAL itself stores what it is given; only the
/// live graph preflight rejects inapplicable ops (ADR-0015). This keeps the
/// replay properties measuring graph *state*, not the applicability checker.
fn apply_commits(commits: &[Vec<RedoOp>]) -> Vec<Vec<RedoOp>> {
    let mut model = proptest_support::ModelGraph::new();
    commits.iter().map(|ops| model.apply_commit(ops)).collect()
}

/// Replay records into a fresh graph, returning a canonical fingerprint of the
/// resulting state (see [`proptest_support::state_json`]).
///
/// The typed [`cypherlite_core::Error`] from `apply_redo` is preserved rather
/// than flattened to a string: it derives `PartialEq`/`Eq`/`Debug`, so
/// `prop_assert_eq!` compares two runs' results directly and a divergence
/// reports the engine's own error, not a re-rendered message.
fn materialize(records: &[WalRecord]) -> Result<serde_json::Value, cypherlite_core::Error> {
    let graph = Graph::default();
    for record in records {
        graph.apply_redo(record.ops.clone(), record.commit_ts)?;
    }
    Ok(proptest_support::state_json(&graph))
}

/// Structural equality of two record runs.
///
/// `WalRecord` is not `PartialEq`, and the derived `PartialEq` on the carried
/// [`cypherlite_core::Value`]s treats two identical `Float(NaN)`s as unequal,
/// so decoded records are compared through their JSON encoding instead. Both
/// sides go through the shared [`encode_ops`] — the same encoder every WAL
/// write uses — so a serialize failure is a harness bug that panics there,
/// never a record difference to reconcile.
fn records_equal(left: &[WalRecord], right: &[WalRecord]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(l, r)| {
            l.lsn == r.lsn && l.commit_ts == r.commit_ts && encode_ops(&l.ops) == encode_ops(&r.ops)
        })
}

proptest! {
    #![proptest_config(proptest_support::config())]

    /// Replaying the same persisted bytes twice yields identical graph state,
    /// including across a close/reopen of the WAL.
    #[test]
    fn replay_is_deterministic(commits in commit_sequence()) {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let mut wal = Wal::open(Arc::clone(&vfs), WAL_PREFIX, 0).expect("open WAL");
        append_all(&mut wal, &apply_commits(&commits));
        let first = wal.replay(0).expect("first replay");
        drop(wal);

        let reopened = Wal::open(Arc::clone(&vfs), WAL_PREFIX, 0).expect("reopen WAL");
        let second = reopened.replay(0).expect("second replay");
        prop_assert_eq!(materialize(&first), materialize(&second));
    }

    /// Replay is a pure read: two reads of the same WAL agree, and a read
    /// after close/reopen agrees with the first.
    #[test]
    fn replay_is_idempotent(commits in commit_sequence()) {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let mut wal = Wal::open(Arc::clone(&vfs), WAL_PREFIX, 0).expect("open WAL");
        append_all(&mut wal, &apply_commits(&commits));

        let first = wal.replay(0).expect("first replay");
        let again = wal.replay(0).expect("second replay");
        prop_assert!(records_equal(&first, &again));
        drop(wal);

        let reopened = Wal::open(Arc::clone(&vfs), WAL_PREFIX, 0).expect("reopen WAL");
        let after_reopen = reopened.replay(0).expect("replay after reopen");
        prop_assert!(records_equal(&first, &after_reopen));
    }

    /// `append_commit` assigns exactly the next LSN, every time, for any op
    /// sequence.
    #[test]
    fn lsn_is_monotonic(commits in commit_sequence()) {
        let mut wal = Wal::memory();
        let mut previous = 0u64;
        for (index, ops) in commits.iter().enumerate() {
            let lsn = wal.append_commit((index + 1) as u64, ops).expect("append");
            prop_assert!(lsn > previous, "LSN regressed: {lsn} after {previous}");
            prop_assert_eq!(lsn, previous + 1);
            previous = lsn;
        }
        prop_assert_eq!(wal.lsn, previous);
    }

    /// A torn tail at EOF is trimmed without error; the committed prefix is
    /// replayed intact and the on-disk bytes are the prefix alone.
    #[test]
    fn torn_tail_prefix_survives(
        commits in commit_sequence(),
        tail in torn_tail(),
    ) {
        let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
        let mut wal = Wal::open(Arc::clone(&vfs), WAL_PREFIX, 0).expect("open WAL");
        append_all(&mut wal, &commits);
        let before = wal.replay(0).expect("replay before corruption");
        let path = wal_segment_path();
        let prefix = read_to_vec(&*vfs, &path).expect("read committed prefix");
        {
            let mut file = vfs
                .open(&path, VfsOpen::append_create())
                .expect("open segment for tail");
            file.write_all(&tail).expect("write torn tail");
        }
        drop(wal);

        let reopened = Wal::open(Arc::clone(&vfs), WAL_PREFIX, 0).expect("reopen with torn tail");
        let after = reopened.replay(0).expect("replay after torn tail");
        prop_assert!(records_equal(&before, &after));
        prop_assert_eq!(
            read_to_vec(&*vfs, &path).expect("read trimmed segment"),
            prefix
        );
    }

    /// Re-persisting replayed records and replaying again must reproduce them
    /// exactly.
    ///
    /// Historically ignored as LABS-134: the workspace built `serde_json`
    /// without the `float_roundtrip` feature, so its float parser was not
    /// round-trip exact and a serialized `f64` (a float property, or a point
    /// coordinate) could be parsed back one ULP away from the value written.
    /// The workspace now enables `float_roundtrip`, the parser is correctly
    /// rounded, and this property passes as an ordinary test.
    #[test]
    fn reencoded_records_are_lossless(commits in commit_sequence()) {
        let mut wal = Wal::memory();
        append_all(&mut wal, &commits);
        let first = wal.replay(0).expect("first replay");

        let mut repersisted = Wal::memory();
        append_all(
            &mut repersisted,
            &first.iter().map(|record| record.ops.clone()).collect::<Vec<_>>(),
        );
        let round_tripped = repersisted.replay(0).expect("replay repersisted");
        prop_assert!(records_equal(&first, &round_tripped));
    }
}
