//! The open/replay cluster (ADR-0014): load the snapshot, open the WAL at
//! its checkpoint LSN, stream the validated records after it, and fold the
//! store-level ontology state the walk passes (ADR-0120). The write side —
//! checkpoint publication — stays with the facade in `lib.rs`.

use cypherlite_core::{Error, Result};
use cypherlite_executor::{Graph, OntologyOp};

use crate::snapshot::{self, LoadedSnapshot};
use crate::wal::Wal;
use crate::{codec, version, SnapshotCodecKind, StoreTarget};

/// The codec vocabulary's one behavioral surface on the open path: which
/// loader reads the snapshot, what the codec does at the open tail — the
/// dirty-WAL guard plus the load's deferred open-time action — and
/// whether it sweeps orphaned store objects after open. One fn table per
/// codec ([`codec_ops`]), so a codec's open behavior is spelled once
/// instead of one arm bolted onto every dispatch the open walk passes;
/// a fourth codec lands in that table alone.
struct CodecOpenOps {
    load: fn(&StoreTarget) -> Result<LoadedSnapshot>,
    finish_open: fn(&StoreTarget, u64, u64, Option<snapshot::DeferredOpenAction>) -> Result<()>,
    sweep_after_open: fn(&StoreTarget) -> Result<()>,
}

/// The classic codecs' open-tail default: no dirty-WAL window (their
/// durable form cannot change underneath them) and no deferred action —
/// every replay is admitted, nothing is deferred.
fn admit_every_replay(
    _target: &StoreTarget,
    _wal_lsn: u64,
    _checkpoint_lsn: u64,
    _deferred: Option<snapshot::DeferredOpenAction>,
) -> Result<()> {
    Ok(())
}

/// The file-per-checkpoint codecs' sweep default: the checkpoint image is
/// rewritten wholesale, so no object is orphaned and there is nothing to
/// retire.
fn nothing_to_sweep(_target: &StoreTarget) -> Result<()> {
    Ok(())
}

/// The one codec → open-ops mapping: Parquet overrides the sweep (it
/// retires objects its newest manifest orphaned, ADR-0089), markdown
/// overrides the open tail (the dirty-WAL guard and the deferred
/// re-baseline, ADR-0406 §5), and every codec names its own loader.
fn codec_ops(codec: SnapshotCodecKind) -> CodecOpenOps {
    match codec {
        SnapshotCodecKind::Parquet => CodecOpenOps {
            load: snapshot::load_with_checkpoint_lsn,
            finish_open: admit_every_replay,
            sweep_after_open: snapshot::sweep_orphans,
        },
        SnapshotCodecKind::Json => CodecOpenOps {
            load: codec::json::load_with_checkpoint_lsn,
            finish_open: admit_every_replay,
            sweep_after_open: nothing_to_sweep,
        },
        SnapshotCodecKind::Markdown => CodecOpenOps {
            load: codec::markdown::load_with_checkpoint_lsn,
            finish_open: codec::markdown::finish_open,
            sweep_after_open: nothing_to_sweep,
        },
    }
}

pub fn load(target: &StoreTarget) -> Result<Graph> {
    Ok(load_with_checkpoint_lsn(target)?.graph)
}

/// Load the snapshot and its checkpoint LSN from one manifest read
/// (0 when no manifest exists yet), plus the version marker the codec
/// loader decoded (ADR-0127 Decision 2: detection costs no extra I/O) and
/// the ontology document the snapshot carries (ADR-0120; `None` when the
/// store holds none).
fn load_with_checkpoint_lsn(target: &StoreTarget) -> Result<LoadedSnapshot> {
    (codec_ops(target.codec).load)(target)
}

/// The codec-owned open tail, through the codec's one ops table
/// ([`codec_ops`]): prove the loaded base safe for replay (the dirty-WAL
/// guard, ADR-0406 §5 — with WAL records past the checkpoint LSN, a
/// codec whose durable form can change underneath it refuses the open
/// instead of corrupting the base), then run the load's deferred
/// open-time action — the markdown re-baseline over externally-edited
/// vaults, the only deferred action today. The classic codecs have no
/// such window, admit every replay, and defer nothing. The read-only
/// open calls this with `None` — the guard alone, no deferred action;
/// the writable open passes the action the load deferred.
fn finish_open(
    target: &StoreTarget,
    wal: &Wal,
    checkpoint_lsn: u64,
    deferred: Option<snapshot::DeferredOpenAction>,
) -> Result<()> {
    (codec_ops(target.codec).finish_open)(target, wal.lsn, checkpoint_lsn, deferred)
}

/// The codec-owned post-open sweep, through the codec's one ops table
/// ([`codec_ops`]): the Parquet codec retires store objects its newest
/// manifest orphaned (ADR-0089); the file-per-checkpoint codecs rewrite
/// their image wholesale and have nothing to sweep.
fn sweep_after_open(target: &StoreTarget) -> Result<()> {
    (codec_ops(target.codec).sweep_after_open)(target)
}

/// Open a store: load snapshot, then stream validated WAL records (ADR-0014).
///
/// The snapshot is loaded and validated *before* [`Wal::open`] may repair a
/// torn tail: a store written by a newer CypherLite (unsupported snapshot
/// `format_version`, JSON `value_encoding`, or read-only marker) fails open
/// with every WAL segment byte untouched (ADR-0114 Decision 2).
///
/// A supported-older store is migrated to the current format before the
/// first query is served (ADR-0127: migrate-on-open, hands-off,
/// forward-only); see [`open_store_with_report`].
pub fn open_store(target: &StoreTarget) -> Result<(Graph, Wal)> {
    let (graph, wal, _, _) = open_store_with_report(target)?;
    Ok((graph, wal))
}

/// [`open_store`] plus the store's current ontology document (ADR-0120):
/// the snapshot's document folded with the WAL-pending ontology ops the
/// replay walked (the last op wins), so a set that no checkpoint covered
/// yet survives the reopen. `None` when the store holds no ontology.
#[doc(hidden)]
pub fn open_store_with_ontology(target: &StoreTarget) -> Result<(Graph, Wal, Option<String>)> {
    let (graph, wal, ontology, _) = open_store_with_report(target)?;
    Ok((graph, wal, ontology))
}

/// [`open_store`] for a read-only handle (ADR-0136).
///
/// Skips migrate-on-open, orphan sweep, and torn-tail trim. Retries when the
/// manifest changes during the load.
pub fn open_store_readonly(target: &StoreTarget) -> Result<(Graph, Wal)> {
    let (graph, wal, _) = open_store_readonly_with_ontology(target)?;
    Ok((graph, wal))
}

/// [`open_store_readonly`] plus the store's current ontology document
/// (ADR-0120), folded exactly as in [`open_store_with_ontology`].
#[doc(hidden)]
pub fn open_store_readonly_with_ontology(
    target: &StoreTarget,
) -> Result<(Graph, Wal, Option<String>)> {
    const ATTEMPTS: u32 = 8;
    let mut last_err = None;
    for attempt in 0..ATTEMPTS {
        let before = manifest_stamp(target)?;
        match open_store_readonly_once(target) {
            Ok((graph, wal, ontology)) => {
                let after = manifest_stamp(target)?;
                if before == after {
                    return Ok((graph, wal, ontology));
                }
            }
            Err(e) => last_err = Some(e),
        }
        if attempt + 1 < ATTEMPTS {
            #[cfg(not(target_arch = "wasm32"))]
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    Err(last_err
        .unwrap_or_else(|| Error::storage("read-only open could not observe a stable snapshot")))
}

/// Open the WAL read-only at the published checkpoint, without loading the
/// snapshot.
///
/// [`Wal::poll_follow`] refuses an append handle. A writer that has just
/// checkpointed already holds the graph that snapshot describes, so it can
/// swap in this WAL and keep the graph.
pub fn open_follower_wal(target: &StoreTarget) -> Result<Wal> {
    let checkpoint_lsn = crate::published_checkpoint_lsn(target)?;
    let (vfs, prefix) = target.wal_vfs();
    Wal::open_readonly(vfs, prefix, checkpoint_lsn)
}

/// Open the WAL for append at the published checkpoint, without loading
/// the snapshot.
///
/// [`Wal::open`] rescans `bytes` and `commits` and trims a torn tail. A
/// follower that has applied through the tip swaps this handle in and
/// keeps the loaded graph (ADR-0440).
pub fn open_writer_wal(target: &StoreTarget) -> Result<Wal> {
    let checkpoint_lsn = crate::published_checkpoint_lsn(target)?;
    let (vfs, prefix) = target.wal_vfs();
    Wal::open(vfs, prefix, checkpoint_lsn)
}

fn manifest_stamp(target: &StoreTarget) -> Result<Option<Vec<u8>>> {
    // The one Option-returning checkpoint-image read, stamped through
    // the codec's own `checkpoint_image` — the file that codec's writer
    // actually rewrites — so the before/after comparison observes the
    // image a concurrent checkpoint really touches (`None` when the
    // store has no image yet).
    snapshot::get_object_opt(target, target.codec.checkpoint_image())
}

fn open_store_readonly_once(target: &StoreTarget) -> Result<(Graph, Wal, Option<String>)> {
    let LoadedSnapshot {
        graph,
        checkpoint_lsn,
        ontology,
        ..
    } = load_with_checkpoint_lsn(target)?;
    let (wal_vfs, wal_prefix) = target.wal_vfs();
    let wal = Wal::open_readonly(wal_vfs, wal_prefix, checkpoint_lsn)?;
    // The read-only open tail: the one call with no deferred action —
    // for the markdown codec that is exactly the dirty-WAL guard
    // (`markdown::finish_open` runs its guard before any deferred
    // action); the classic codecs admit every replay.
    finish_open(target, &wal, checkpoint_lsn, None)?;
    let ontology = replay_after_ontology(&graph, &wal, checkpoint_lsn, ontology)?;
    Ok((graph, wal, ontology))
}

/// [`open_store`] plus the folded ontology document (ADR-0120; see
/// [`open_store_with_ontology`]) and the `pub(crate)` migration report
/// (ADR-0127 Decision 8). The crate has no logging facility, so the report
/// surfaces only through this internal channel; its `Display` impl is the
/// trace line a future logging facility would emit on open.
///
/// ADR-0127 Decisions 2–4: one open flow. The codec loaders report the
/// version marker they decoded, so detection costs no extra I/O; a legacy
/// marker on a durable VFS is then migrated on the handles already open —
/// checkpoint at the current format, rotate to a fresh segment with the
/// current magic, recycle the covered segments. There is no second load,
/// `Wal::open` or replay. Checkpoint publication follows the manifest-last
/// tmp/rename rules (ADR-0089/ADR-0092), so a crash mid-migration leaves
/// the old store intact and the next open re-migrates from scratch.
/// `:memory:` never migrates (Decision 7). A store with an unreadable
/// marker (newer binary, unknown magic) fails at the ADR-0114 gate inside
/// the load, before any byte mutates.
pub(crate) fn open_store_with_report(
    target: &StoreTarget,
) -> Result<(Graph, Wal, Option<String>, version::MigrationReport)> {
    let LoadedSnapshot {
        graph,
        checkpoint_lsn,
        ontology,
        marker,
        deferred,
    } = load_with_checkpoint_lsn(target)?;
    let (wal_vfs, wal_prefix) = target.wal_vfs();
    let mut wal = Wal::open(wal_vfs, wal_prefix, checkpoint_lsn)?;
    // The writable-open tail (ADR-0406 §5), codec-owned through the ops
    // table: the dirty-WAL guard first, then — no WAL deltas pending
    // over the loaded base — the load's deferred action runs.
    finish_open(target, &wal, checkpoint_lsn, deferred)?;
    let ontology = replay_after_ontology(&graph, &wal, checkpoint_lsn, ontology)?;
    let mut report = version::MigrationReport::default();
    if let version::SnapshotMarker::Legacy(subsystem, from) = marker {
        if target.is_fully_durable() {
            report.record(subsystem, from);
            // The migration checkpoint carries the loaded ontology document
            // so a content-mismatched marker (ADR-0120 §8) is normalized,
            // never dropped. It publishes at the graph's own commit
            // timestamp, like every checkpoint.
            crate::checkpoint_at(
                target,
                &graph,
                wal.lsn,
                graph.commit_ts(),
                ontology.as_deref(),
            )?;
            // Rotate to a fresh segment with the current magic and recycle
            // every segment the checkpoint covers. A fully checkpointed
            // tail leaves no commits on the open segment, so `rotate` is a
            // no-op and the open segment — whose magic `Wal::open` already
            // verified as readable — stays active.
            let cut = wal.rotate()?;
            wal.recycle(cut)?;
        }
    }
    sweep_after_open(target)?;
    Ok((graph, wal, ontology, report))
}

/// Materialize the live graph: snapshot + streamed WAL records after `checkpoint_lsn`.
pub fn load_with_wal(target: &StoreTarget, wal: &Wal) -> Result<Graph> {
    let LoadedSnapshot {
        graph,
        checkpoint_lsn,
        ..
    } = load_with_checkpoint_lsn(target)?;
    replay_after(&graph, wal, checkpoint_lsn)?;
    Ok(graph)
}

fn replay_after(graph: &Graph, wal: &Wal, checkpoint_lsn: u64) -> Result<()> {
    replay_after_ontology(graph, wal, checkpoint_lsn, None).map(|_| ())
}

/// [`replay_after`] plus the store-level ontology state (ADR-0120 §1/§3):
/// the snapshot's document folded with every ontology op the replay walks
/// (the last op wins — a set no checkpoint covered yet, or a drop after
/// one). The ops themselves are graph no-ops; the document lives with the
/// store layer.
fn replay_after_ontology(
    graph: &Graph,
    wal: &Wal,
    checkpoint_lsn: u64,
    snapshot: Option<String>,
) -> Result<Option<String>> {
    let mut ontology = snapshot;
    wal.for_each_after(checkpoint_lsn, |rec| {
        for op in &rec.ops {
            // The executor's typed classifier: a future ontology-affecting
            // redo variant forces a compile-time decision at its enum site,
            // not a silent miss in this fold.
            match op.ontology_op() {
                Some(OntologyOp::Set(document)) => ontology = Some(document.to_owned()),
                Some(OntologyOp::Drop) => ontology = None,
                None => {}
            }
        }
        graph.apply_replayed_redo(rec.ops, rec.commit_ts)
    })?;
    graph.rebuild_indexes();
    Ok(ontology)
}
