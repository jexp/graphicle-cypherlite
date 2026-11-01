//! Reference model for WAL operation sequences (ADR-0268), driven by
//! `proptest-state-machine`.
//!
//! The model is a deterministic, in-memory prediction of the three canonical
//! observables of a real [`Wal`] — the current LSN, a digest of the records
//! still resident on disk, and the number of live segment files. A property
//! suite pairs [`WalReferenceModel`] (the pure state machine) with
//! [`WalStateMachineTest`] (the SUT harness over a real `Wal` on a
//! [`MemoryVfs`]); after every transition the harness compares
//! [`canonical_wal_state`] against [`WalModelState::canonical`]. The suite only
//! has to declare the test and pick a sequence length. The lower bound **must**
//! be at least 1: `proptest-state-machine` 0.8 underflows (`max_size - 1`) when
//! it samples a sequence length of 0, so a range that includes 0 panics — the
//! in-tree suite uses `1..=64`:
//!
//! ```ignore
//! mod proptest_support;
//! use proptest_state_machine::ReferenceStateMachine;
//!
//! proptest! {
//!     #![proptest_config(proptest_support::config())]
//!     #[test]
//!     fn wal_state_machine(ref_state in
//!         proptest_support::wal_model::WalReferenceModel::sequential_strategy(1..24_usize))
//!     {
//!         // ... test_sequential(...) as in the proptest-state-machine book
//!     }
//! }
//! ```
//!
//! # The five transitions
//!
//! | Transition | Real `Wal` API driven | Model effect |
//! | --- | --- | --- |
//! | `AppendCommit(ops)` | [`Wal::append_commit`] (append + sync) | `lsn += 1`, ops recorded |
//! | `Sync` | [`Wal::sync`] | none (durability only) |
//! | `Rotate` | [`Wal::rotate`] | `segments += 1` when the segment holds records |
//! | `Checkpoint` | [`Wal::checkpoint_pin`] + [`Wal::recycle`] | closes/recycles closed segments |
//! | `Reload` | [`Wal::open`] over the same VFS | recomputes the open-segment record count |
//!
//! **Reachable vs approximated.** `AppendCommit`, `Rotate`, and `Sync` are
//! driven verbatim. A full `checkpoint_at` (snapshot dump) needs a
//! `StoreTarget` + `Graph` and is out of scope for a `Wal`-only reference
//! model, so `Checkpoint` drives the **WAL-side effect** of a checkpoint — the
//! pin (`checkpoint_pin`) and the segment deletion (`recycle`) — which is the
//! only part that changes the canonical WAL state; the snapshot itself lives
//! outside the `Wal`. `Reload` reopens the WAL through the public
//! [`Wal::open`] on the *same* `MemoryVfs`, which is exactly what
//! `open_store`/`load_with_wal` do after locating the persisted checkpoint LSN;
//! the harness passes the LSN pinned by the last `Checkpoint`, so a reloaded
//! `Wal` sees the same "un-checkpointed records" boundary a real recovery
//! would.
//!
//! # Model state
//!
//! The canonical trio — [`WalModelState::lsn`], the committed-op digest, and
//! [`WalModelState::segments`] — is what the suite asserts on. The remaining
//! fields are bookkeeping the five transitions need to stay faithful; they are
//! private and never compared directly.
//!
//! Ops entering the model pass through [`codec_roundtrip`], the same
//! serialize-then-deserialize transform the WAL applies, so the digest is over
//! the *decoded* op form a real `Wal::replay` returns — not the freshly
//! generated one. Without it, `serde_json`'s non-exact float parse would make
//! the two sides digest different bytes for the same float-bearing commit.

use std::sync::Arc;

use cypherlite_executor::RedoOp;
use cypherlite_storage::{MemoryVfs, Vfs, Wal};
use proptest::prelude::*;
use proptest::strategy::BoxedStrategy;
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest};

use super::{encode_ops, redo_op_sequence, WAL_PREFIX};

/// One step in a modelled WAL run.
#[derive(Clone, Debug)]
pub enum WalTransition {
    /// Append and sync one commit of `ops`.
    AppendCommit(Vec<RedoOp>),
    /// Issue the durability barrier without appending.
    Sync,
    /// Close the open segment so a checkpoint can dump up to its LSN.
    Rotate,
    /// Pin the closed segments and recycle those at or below the pinned LSN.
    Checkpoint,
    /// Reopen the WAL over the same VFS at the last pinned checkpoint LSN.
    Reload,
}

/// Canonical, comparable snapshot of a WAL's observable state: the LSN, a
/// digest of the records still resident on disk, and the live segment count.
///
/// [`canonical_wal_state`] produces it from a real `Wal`; [`WalModelState::canonical`]
/// produces it from the model. Equality of the two is the state assertion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanonicalWalState {
    pub lsn: u64,
    pub op_digest: u64,
    pub segments: usize,
}

/// Stable 64-bit digest (FNV-1a) over the JSON encoding of `ops`.
///
/// The encoding is the same tagged JSON the WAL itself writes (see the
/// `TaggedRef` encoder in `cypherlite-core`), so equal op sequences always
/// digest equally and the digest is independent of process, run, and Rust
/// toolchain. Call it on ops that have already been through [`codec_roundtrip`]
/// so the model and a real `Wal` digest the same bytes.
pub fn op_digest(ops: &[RedoOp]) -> u64 {
    let bytes = encode_ops(ops);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Mirror the WAL's JSON codec: serialize then deserialize a slice of ops —
/// the exact transform `Wal::append_record` and `scan_seg` apply. A real `Wal`
/// hands `replay` the *decoded* form, and `serde_json`'s float parse is not
/// round-trip exact (a finite `f64` can shift by an ULP), so the model must
/// store the decoded form too; otherwise the two digest different bytes for
/// the same logical op and the state assertion would fail for any commit
/// carrying a float property.
fn codec_roundtrip(ops: &[RedoOp]) -> Vec<RedoOp> {
    let encoded = encode_ops(ops);
    serde_json::from_slice(&encoded).expect("WAL-encoded ops deserialize")
}

/// The live segment-file count of `prefix` under `vfs`: one file per segment.
pub fn segment_count(vfs: &dyn Vfs, prefix: &str) -> usize {
    vfs.list_dir(prefix)
        .expect("WAL directory is listable")
        .iter()
        .filter(|name| name.ends_with(".log"))
        .count()
}

/// Canonical snapshot of a real `Wal`: its LSN, the digest of every redo op the
/// WAL still holds on disk (`replay(0)`), and its live segment count.
pub fn canonical_wal_state(wal: &Wal, vfs: &dyn Vfs, prefix: &str) -> CanonicalWalState {
    let ops: Vec<RedoOp> = wal
        .replay(0)
        .expect("WAL replays without error")
        .into_iter()
        .flat_map(|record| record.ops)
        .collect();
    CanonicalWalState {
        lsn: wal.lsn,
        op_digest: op_digest(&ops),
        segments: segment_count(vfs, prefix),
    }
}

/// Reference-model state. [`WalModelState::lsn`] and
/// [`WalModelState::segments`] are the public observables the suite asserts on
/// (the third of the canonical trio — the op digest — derives from `commits`);
/// the private fields mirror the `Wal` internals the five transitions must
/// predict (record grouping per commit, the open-segment record count, the
/// last rotation LSN, the recycled LSN boundary, and the pinned checkpoint
/// LSN).
#[derive(Clone, Debug)]
pub struct WalModelState {
    /// Highest committed LSN (0 before any append).
    pub lsn: u64,
    /// Live segment files (1 until a rotation with records creates a second).
    pub segments: usize,
    /// Every commit as `(lsn, ops)` in append order — the LSN-tagged source
    /// `ops_on_disk` uses to recompute which records survive a recycle.
    commits: Vec<(u64, Vec<RedoOp>)>,
    /// Records in the open segment, mirroring `Wal::commits`.
    open_commits: u64,
    /// LSN of the most recent rotation (0 before any).
    last_rotate_lsn: u64,
    /// Highest LSN whose records a checkpoint has recycled off disk.
    recycled_upto: u64,
    /// LSN pinned by the most recent checkpoint; the value `Reload` reopens at.
    checkpoint_lsn: u64,
}

impl WalModelState {
    /// The empty log: LSN 0, no ops, one segment.
    pub fn empty() -> Self {
        Self {
            lsn: 0,
            segments: 1,
            commits: Vec::new(),
            open_commits: 0,
            last_rotate_lsn: 0,
            recycled_upto: 0,
            checkpoint_lsn: 0,
        }
    }

    /// The redo ops still resident on disk: every op committed at an LSN above
    /// the recycled boundary. Matches what `Wal::replay(0)` returns.
    fn ops_on_disk(&self) -> Vec<RedoOp> {
        self.commits
            .iter()
            .filter(|(lsn, _)| *lsn > self.recycled_upto)
            .flat_map(|(_, ops)| ops.iter().cloned())
            .collect()
    }

    /// The canonical snapshot this state predicts.
    pub fn canonical(&self) -> CanonicalWalState {
        CanonicalWalState {
            lsn: self.lsn,
            op_digest: op_digest(&self.ops_on_disk()),
            segments: self.segments,
        }
    }
}

/// The reference state machine for WAL operation sequences.
pub struct WalReferenceModel;

impl ReferenceStateMachine for WalReferenceModel {
    type State = WalModelState;
    type Transition = WalTransition;

    fn init_state() -> BoxedStrategy<Self::State> {
        Just(WalModelState::empty()).boxed()
    }

    // `sequential_strategy` is deliberately *not* overridden: the trait default
    // is faithful once the size range excludes 0. (A `0` lower bound panics a
    // `proptest-state-machine` 0.8 underflow — `max_size - 1` on an empty
    // sequence — so a caller that must accept 0 should clamp to `1..` before
    // building the range, as the module example does.)

    fn transitions(_state: &Self::State) -> BoxedStrategy<Self::Transition> {
        prop_oneof![
            4 => redo_op_sequence().prop_map(WalTransition::AppendCommit),
            1 => Just(WalTransition::Rotate),
            1 => Just(WalTransition::Sync),
            1 => Just(WalTransition::Checkpoint),
            1 => Just(WalTransition::Reload),
        ]
        .boxed()
    }

    fn apply(mut state: Self::State, transition: &Self::Transition) -> Self::State {
        match transition {
            WalTransition::AppendCommit(ops) => {
                // Store the ops as the WAL codec would return them, so the
                // canonical digest compares like with like across a `Reload`.
                let ops = codec_roundtrip(ops);
                state.lsn += 1;
                state.commits.push((state.lsn, ops));
                state.open_commits += 1;
            }
            WalTransition::Sync => {}
            WalTransition::Rotate => {
                // `Wal::rotate` is a no-op while the open segment holds no
                // records, and otherwise closes it and opens a fresh one.
                if state.open_commits > 0 {
                    state.segments += 1;
                    state.open_commits = 0;
                    state.last_rotate_lsn = state.lsn;
                }
            }
            WalTransition::Checkpoint => {
                // `checkpoint_pin` reports the latest closed segment's LSN, or
                // the whole log when nothing is closed. `recycle` then drops
                // every closed segment (all of them are at or below the pin),
                // leaving only the open segment on disk. Pin and recycle are
                // coupled: both key off `segments > 1`, so they share a branch.
                if state.segments > 1 {
                    state.checkpoint_lsn = state.last_rotate_lsn;
                    state.recycled_upto = state.last_rotate_lsn;
                    state.segments = 1;
                } else {
                    state.checkpoint_lsn = state.lsn;
                }
            }
            WalTransition::Reload => {
                // `Wal::open` recomputes the open segment's record count as the
                // records above the checkpoint LSN; appends and rotations do
                // not touch the file layout.
                state.open_commits = state
                    .commits
                    .iter()
                    .filter(|(lsn, _)| *lsn > state.last_rotate_lsn && *lsn > state.checkpoint_lsn)
                    .count() as u64;
            }
        }
        state
    }
}

/// The system under test: a real `Wal` on a private [`MemoryVfs`], plus the
/// LSN the next `Reload` reopens at.
pub struct WalSut {
    vfs: Arc<dyn Vfs>,
    prefix: String,
    checkpoint_lsn: u64,
    wal: Wal,
}

/// Open a fresh memory-backed WAL for a model run.
pub fn init_wal_sut() -> WalSut {
    let vfs: Arc<dyn Vfs> = Arc::new(MemoryVfs::new());
    let wal = Wal::open(vfs.clone(), WAL_PREFIX, 0).expect("open fresh memory WAL");
    WalSut {
        vfs,
        prefix: WAL_PREFIX.to_string(),
        checkpoint_lsn: 0,
        wal,
    }
}

/// The state-machine test wiring a [`WalReferenceModel`] to a real `Wal`.
pub struct WalStateMachineTest;

impl StateMachineTest for WalStateMachineTest {
    type SystemUnderTest = WalSut;
    type Reference = WalReferenceModel;

    fn init_test(_ref_state: &WalModelState) -> Self::SystemUnderTest {
        init_wal_sut()
    }

    fn apply(
        mut sut: Self::SystemUnderTest,
        ref_state: &WalModelState,
        transition: WalTransition,
    ) -> Self::SystemUnderTest {
        match transition {
            WalTransition::AppendCommit(ops) => {
                // The post-transition LSN is the strictly-increasing commit
                // timestamp the WAL requires; it is > 0 and < u64::MAX by
                // construction.
                sut.wal
                    .append_commit(ref_state.lsn, &ops)
                    .expect("append_commit on a memory WAL never fails");
            }
            WalTransition::Sync => {
                sut.wal.sync().expect("sync on a memory WAL never fails");
            }
            WalTransition::Rotate => {
                sut.wal
                    .rotate()
                    .expect("rotate on a memory WAL never fails");
            }
            WalTransition::Checkpoint => {
                let (pin_lsn, _ts) = sut.wal.checkpoint_pin();
                sut.wal.recycle(pin_lsn).expect("recycle never fails");
                sut.checkpoint_lsn = pin_lsn;
            }
            WalTransition::Reload => {
                sut.wal = Wal::open(sut.vfs.clone(), &sut.prefix, sut.checkpoint_lsn)
                    .expect("reopen WAL over the same VFS");
            }
        }
        sut
    }

    fn check_invariants(sut: &Self::SystemUnderTest, ref_state: &WalModelState) {
        let actual = canonical_wal_state(&sut.wal, &*sut.vfs, &sut.prefix);
        let expected = ref_state.canonical();
        assert_eq!(
            actual, expected,
            "WAL canonical state diverged from the reference model"
        );
        assert_eq!(
            sut.wal.has_closed_segments(),
            ref_state.segments > 1,
            "closed-segment flag disagrees with the model's segment count"
        );
    }
}

// Sanity tests removed: this module is included by every storage integration
// binary, so a `#[cfg(test)] mod` here compiles once per binary and Rust
// integration-test targets cannot be gated per-target. The float / Rotate /
// Checkpoint / Reload paths they covered are exercised by the
// `wal_state_machine_proptest` suite's 256-case `prop_state_machine!` run
// (ADR-0268).
