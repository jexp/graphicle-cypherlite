//! Checkpoint / crash-recovery properties for the WAL + snapshot store
//! (ADR-0268, node `06-checkpoint-crash-recovery`).
//!
//! Three properties, each drawn from the shared [`proptest_support`]
//! `RedoOp` alphabet and each exercised against both the in-process
//! [`MemoryVfs`] store and a filesystem [`NativeVfs`] store under a tempdir:
//!
//! 1. [`checkpoint_reload_equivalence`] — the graph recovered from a
//!    checkpoint equals the in-memory graph that was checkpointed.
//! 2. [`fsync_prefix_crash_recovery`] — truncating the WAL to a real
//!    fsync/commit boundary and reopening yields the state of the committed
//!    prefix (the crash-cut points are the byte offsets recorded *after* each
//!    `append_commit` barrier, never arbitrary offsets).
//! 3. [`wal_checkpoint_joint_replay`] — recovery from a checkpointed LSN plus
//!    the surviving WAL equals a full replay from genesis.
//!
//! State equality is compared through the engine's own canonical tagged value
//! encoding ([`Value::to_json_tagged`]) via the shared
//! [`proptest_support::state_json`] fingerprint, so that values the
//! WAL/checkpoint codecs legitimately normalise (e.g. `NaN`) compare by
//! representation.
//!
//! The generated op sequence is first reduced by the shared
//! [`proptest_support::ModelGraph`] applicability filter (the single filter,
//! also used by the WAL suite) to the ops that actually apply on top of the
//! sequence so far (a `CreateRel` needs a fresh id and both endpoints,
//! `DeleteNode` detaches incident relationships, …). The raw alphabet can name
//! ids it never created; the engine's redo preflight rejects those, so the
//! reduction keeps the recovered states well-formed without restricting the
//! value alphabet.
//!
//! The two checkpoint-bearing properties (1 and 3) then pass the reduced
//! sequence through the shared
//! [`proptest_support::checkpoint_persistable_ops`] representability rule: a
//! checkpoint persists property values through the store's snapshot codec, and
//! the v2 JSON codec (cypherlite-model/src/serde_value.rs) rejects non-finite
//! floats at encode — "rejecting values that cannot round-trip", ADR-0087's
//! fail-closed contract — so under the JSON codec (`--no-default-features`,
//! ADR-0105) `NaN`/`±Infinity` are rewritten to finite stand-ins and `Uuid`s
//! to their canonical strings (the form the codec persists) before
//! persistence, while under Parquet (default features) the rule is the
//! identity and the full float alphabet is checkpointed. Property 2 never
//! checkpoints (it truncates the WAL), so it keeps the full alphabet in every
//! build.
//!
//! # Status
//!
//! All three properties run as ordinary passing tests. They were
//! ignored while LABS-134 (serde_json without `float_roundtrip` loses 1
//! ULP on f64 read) was open; that bug is now fixed — see the comment above
//! the `proptest!` block below for the canonical statement.

mod proptest_support;

use std::sync::Arc;

use cypherlite_executor::{Graph, RedoOp};
use cypherlite_storage::{
    checkpoint_with_lsn, memory_store, open_store, parse_uri, wal_dir, NativeVfs, StoreTarget, Vfs,
    VfsOpen, Wal,
};
use proptest::prelude::*;

/// Apply `seq[..upto]` to a fresh graph, one commit per op with `commit_ts`
/// advancing `1..=upto` — the in-memory reference the recovery is checked
/// against.
fn apply_prefix(seq: &[RedoOp], upto: usize) -> Graph {
    let graph = Graph::default();
    for (i, op) in seq[..upto].iter().enumerate() {
        graph
            .apply_redo(vec![op.clone()], i as u64 + 1)
            .expect("reference apply");
    }
    graph
}

/// The VFS that holds the WAL plus the (single) segment path within it.
///
/// A `:memory:` store shares one [`MemoryVfs`] for the snapshot and the WAL,
/// where the segment lives under the `wal/` prefix. A filesystem store keeps
/// its WAL in a sibling directory ([`wal_dir`]), opened directly by a
/// lease-free [`NativeVfs`] here.
fn wal_file_parts(store: &StoreTarget) -> (Arc<dyn Vfs>, String) {
    match wal_dir(store) {
        Some(dir) => (
            Arc::new(NativeVfs::new(dir)),
            proptest_support::WAL_SEGMENT_FILE.to_string(),
        ),
        None => (store.vfs(), proptest_support::wal_segment_path()),
    }
}

/// Truncate the WAL segment to `len` bytes — the physical crash-cut.
fn truncate_wal(store: &StoreTarget, len: u64) {
    let (vfs, path) = wal_file_parts(store);
    let mut file = vfs
        .open(&path, VfsOpen::read_write())
        .expect("open WAL segment");
    file.set_len(len).expect("truncate WAL segment");
    file.sync_all().expect("sync truncated WAL");
}

/// Run `check` against a fresh in-process store and a fresh filesystem store.
fn for_each_vfs(check: impl Fn(&StoreTarget)) {
    check(&memory_store());
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("db");
    let store = parse_uri(db_path.to_str().expect("tempdir path is valid UTF-8"))
        .expect("open native store");
    check(&store);
}

/// Open `store` and append every op of `seq` as its own commit (commit
/// timestamps `1..=n`) — the reference WAL every recovery property starts from.
///
/// Returns the opened [`Wal`] plus the segment's byte length after each commit:
/// `boundaries[k]` is the durable (fsynced) length once the first `k` commits
/// have landed, with `boundaries[0]` the length before any commit — the bare
/// magic header. That is the crash-cut boundary the fsync-prefix property
/// truncates to.
fn append_sequence(store: &StoreTarget, seq: &[RedoOp]) -> (Wal, Vec<u64>) {
    let (_graph, mut wal) = open_store(store).expect("open store");
    let (vfs, path) = wal_file_parts(store);
    let mut boundaries = vec![vfs.metadata(&path).expect("stat WAL").len];
    for (i, op) in seq.iter().enumerate() {
        wal.append_commit(i as u64 + 1, std::slice::from_ref(op))
            .expect("append commit");
        boundaries.push(vfs.metadata(&path).expect("stat WAL").len);
    }
    (wal, boundaries)
}

/// Reopen `store` and assert its recovered graph state equals `expected`,
/// naming `context` on divergence. The reopen-then-fingerprint tail is identical
/// across all three recovery checks, so it lives here; each check keeps only its
/// setup.
fn assert_recovers_to(store: &StoreTarget, expected: &Graph, context: &str) {
    let (recovered, _wal) = open_store(store).expect("reopen store");
    assert_eq!(
        proptest_support::state_json(&recovered),
        proptest_support::state_json(expected),
        "recovery diverged ({context})"
    );
}

/// Property 1: checkpoint then reopen yields the same state that was
/// checkpointed.
fn check_checkpoint_reload(store: &StoreTarget, seq: &[RedoOp]) {
    let (wal, _) = append_sequence(store, seq);
    let graph = apply_prefix(seq, seq.len());
    checkpoint_with_lsn(store, &graph, wal.lsn, None).expect("checkpoint");
    drop(wal);

    assert_recovers_to(store, &graph, &format!("checkpoint-reload for seq {seq:?}"));
}

/// Property 2: cut the WAL at the fsync boundary after `cut` commits and
/// recover; the recovered state must equal replaying the committed prefix.
fn check_fsync_prefix_recovery(store: &StoreTarget, seq: &[RedoOp], cut: usize) {
    let (wal, boundaries) = append_sequence(store, seq);
    drop(wal);

    // `boundaries[cut]` is the durable end after `cut` commits; index 0 is the
    // bare magic header, the cut before any commit — so `cut` indexes it
    // directly, no special case.
    truncate_wal(store, boundaries[cut]);

    let expected = apply_prefix(seq, cut);
    assert_recovers_to(
        store,
        &expected,
        &format!("fsync-prefix recovery at cut {cut} for seq {seq:?}"),
    );
}

/// Property 3: after a checkpoint at `cut` commits, recovery from the
/// checkpoint plus the surviving WAL equals a full replay from genesis.
fn check_joint_replay(store: &StoreTarget, seq: &[RedoOp], cut: usize) {
    let (wal, _) = append_sequence(store, seq);
    // One checkpoint at `cut`: the snapshot holds ops `0..cut` at LSN `cut`
    // (`apply_prefix` applies `0..cut`, so at `cut == 0` it is the empty graph
    // at LSN 0), and the WAL still holds every commit, so recovery must replay
    // it on top of the snapshot. Checkpointing *after* the loop rather than
    // mid-loop is the same state: `apply_prefix(seq, cut)` has `commit_ts ==
    // cut`, matching the claimed `checkpoint_lsn`.
    checkpoint_with_lsn(store, &apply_prefix(seq, cut), cut as u64, None).expect("checkpoint");
    drop(wal);

    let full = apply_prefix(seq, seq.len());
    assert_recovers_to(
        store,
        &full,
        &format!("WAL+checkpoint joint replay at cut {cut} for seq {seq:?}"),
    );
}

/// The reduced op sequence: draw from the shared alphabet, then keep only the
/// ops the shared applicability filter accepts on top of the run so far. All
/// three properties start here, so the model preamble lives in one place.
fn reduced_sequence() -> impl Strategy<Value = Vec<RedoOp>> {
    proptest_support::redo_op_sequence().prop_map(|ops| {
        let mut model = proptest_support::ModelGraph::new();
        model.apply_commit(&ops)
    })
}

/// The reduced op sequence for the two checkpoint-bearing properties, further
/// adapted by the shared [`proptest_support::checkpoint_persistable_ops`]
/// representability rule: a checkpoint persists property values through the
/// active snapshot codec, and the v2 JSON codec rejects non-finite floats at
/// encode (ADR-0087, fail-closed), so under `--no-default-features` (JSON)
/// `NaN`/`±Infinity` are rewritten to finite stand-ins and `Uuid`s to their
/// canonical strings before persistence.
/// Under Parquet (default features) the adapter is the identity and the full
/// float alphabet is checkpointed. The fsync-prefix property below never
/// checkpoints — it truncates the WAL — so it keeps the unadapted sequence
/// and the full alphabet in every build.
fn checkpointable_sequence() -> impl Strategy<Value = Vec<RedoOp>> {
    reduced_sequence().prop_map(proptest_support::checkpoint_persistable_ops)
}

/// `(sequence, cut)` with the cut drawn *relative to* the generated sequence,
/// so it is always a valid index in `0..=seq.len()` (0 = before any commit).
fn with_cut(
    seqs: impl Strategy<Value = Vec<RedoOp>>,
) -> impl Strategy<Value = (Vec<RedoOp>, usize)> {
    seqs.prop_flat_map(|seq| {
        let cut = 0..=seq.len();
        (Just(seq), cut)
    })
}

/// [`with_cut`] over the reduced sequence — the fsync-prefix property's
/// input.
fn reduced_sequence_with_cut() -> impl Strategy<Value = (Vec<RedoOp>, usize)> {
    with_cut(reduced_sequence())
}

/// [`with_cut`] over the checkpointable sequence — the joint-replay
/// property's input.
fn checkpointable_sequence_with_cut() -> impl Strategy<Value = (Vec<RedoOp>, usize)> {
    with_cut(checkpointable_sequence())
}

// All three properties below were IGNORED while a genuine engine bug tracked
// as LABS-134 was open: the WAL and snapshot codecs decode property / redo
// JSON with `serde_json`, whose *default* float parser is not correctly
// rounded. For a non-trivial share of `f64` values it returned a value one ULP
// away from the original (`"-5.17145877622159e+39".parse::<f64>()` reproduces
// the written bits; `serde_json::from_str::<f64>` returned the adjacent
// value), so `create_node` → checkpoint → reopen, and `append_commit` →
// replay, silently corrupted a property by one ULP.
//
// The fix shipped in this change: the workspace dependency enables
// `serde_json`'s `float_roundtrip` feature (`Cargo.toml`:
// `serde_json = { version = "1", features = ["float_roundtrip"] }`), so the
// parser is correctly rounded and these properties pass unchanged. Do NOT
// weaken the comparison — a one-ULP silent corruption of a persisted value is
// exactly what these properties exist to catch.
proptest! {
    #![proptest_config(proptest_support::config())]

    /// Checkpoint-reload equivalence: the graph recovered from a checkpoint
    /// equals the in-memory graph that was checkpointed (Memory and Native
    /// VFS). Persists the checkpointable sequence (see
    /// [`checkpointable_sequence`]).
    #[test]
    fn checkpoint_reload_equivalence(seq in checkpointable_sequence()) {
        for_each_vfs(|store| check_checkpoint_reload(store, &seq));
    }

    /// fsync-prefix (crash-cut) recovery: truncating the WAL to the fsync
    /// boundary after `cut` commits and reopening yields the committed prefix
    /// (Memory and Native VFS).
    #[test]
    fn fsync_prefix_crash_recovery((seq, cut) in reduced_sequence_with_cut()) {
        for_each_vfs(|store| check_fsync_prefix_recovery(store, &seq, cut));
    }

    /// WAL+checkpoint joint replay: recovery from the checkpointed LSN plus the
    /// remaining WAL equals a full replay from genesis (Memory and Native VFS).
    /// Persists the checkpointable sequence (see [`checkpointable_sequence`]).
    #[test]
    fn wal_checkpoint_joint_replay((seq, cut) in checkpointable_sequence_with_cut()) {
        for_each_vfs(|store| check_joint_replay(store, &seq, cut));
    }
}
