//! WAL state-machine property suite (ADR-0268).
//!
//! Drives `proptest-state-machine`'s [`prop_state_machine!`] macro over the
//! reference model in [`proptest_support::wal_model`]: every case is a random
//! interleaving of the five WAL transitions — [`AppendCommit`], [`Rotate`],
//! [`Sync`], [`Checkpoint`], [`Reload`] — applied both to a real `Wal` on a
//! `MemoryVfs` and to the pure [`WalReferenceModel`]. After every transition
//! the harness asserts the real WAL's canonical state (LSN, resident-record
//! digest, live segment count) equals the model's prediction, so any divergence
//! is reported with the transition sequence that produced it.
//!
//! The macro expands to a `proptest!` case whose input is the model's
//! `sequential_strategy`, and `StateMachineTest::test_sequential` calls
//! `check_invariants` after each transition — that is what makes the model/real
//! agreement a per-transition assertion rather than an end-of-run check.
//!
//! Case count comes from [`proptest_support::config`] (256, the pinned
//! default); the length range `1..=MAX_TRANSITIONS` allows up to 64 sequential
//! transitions per case. The lower bound is 1 because `proptest-state-machine`
//! 0.8 panics with a `max_size - 1` underflow when the sequential size range
//! can sample 0, so a 0 lower bound would panic; see the note in
//! [`proptest_support::wal_model`].
//!
//! [`AppendCommit`]: proptest_support::wal_model::WalTransition::AppendCommit
//! [`Rotate`]: proptest_support::wal_model::WalTransition::Rotate
//! [`Sync`]: proptest_support::wal_model::WalTransition::Sync
//! [`Checkpoint`]: proptest_support::wal_model::WalTransition::Checkpoint
//! [`Reload`]: proptest_support::wal_model::WalTransition::Reload
//! [`WalReferenceModel`]: proptest_support::wal_model::WalReferenceModel

mod proptest_support;

use proptest_support::wal_model::WalStateMachineTest;

/// Upper bound on the number of transitions in a single case. Long enough to
/// hit multi-rotation, checkpoint, and reload sequences; short enough to keep
/// a case cheap under the 256-case budget.
const MAX_TRANSITIONS: usize = 64;

proptest_state_machine::prop_state_machine! {
    #![proptest_config(proptest_support::config())]

    #[test]
    fn wal_state_machine(sequential 1..=MAX_TRANSITIONS => WalStateMachineTest);
}
