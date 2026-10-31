//! Publish the cut a read-only open loaded into a fresh store (ADR-0156,
//! ADR-0282): the shared machinery behind clone.

use std::cell::Cell;
use std::path::Path;

use cypherlite_storage::{
    checkpoint_at, init_empty_wal, parse_uri_creating, SnapshotCodecKind, DESTINATION_EXISTS,
};

use crate::{Database, Error, Result};

thread_local! {
    static FAIL_AFTER_DEST: Cell<bool> = const { Cell::new(false) };
    static PLANT_DEST: Cell<Option<PlantShape>> = const { Cell::new(None) };
}

/// The shape of a planted race-window destination (ADR-0161): one ternary
/// mode (no plant / plant shaped like the destination / plant always a
/// directory), carried as data so no incoherent both-set state exists.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub enum PlantShape {
    /// Shaped like the destination: a sentinel file at a `.cl` bundle
    /// destination, a directory holding one sentinel file at a native
    /// directory destination.
    MatchDestination,
    /// Always a directory holding one sentinel file — even at a `.cl`
    /// bundle destination, where a directory raced into the ADR-0161 window
    /// makes the exclusive create fail (`IsADirectory` on Unix,
    /// `AccessDenied` on Windows) rather than `AlreadyExists`.
    Directory,
}

/// Test hook: the next clone publish on this thread fails
/// after the destination is created, then clears the hook.
#[doc(hidden)]
pub fn set_fail_after_dest(fail: bool) {
    FAIL_AFTER_DEST.with(|slot| slot.set(fail));
}

fn take_fail_after_dest() -> bool {
    FAIL_AFTER_DEST.with(|slot| slot.replace(false))
}

/// Test hook: the next clone publish on this thread plants
/// a stand-in destination between the existence pre-check and the exclusive
/// create — the ADR-0161 race window — then clears the hook. `Some(shape)`
/// arms the plant, `None` disarms it. [`PlantShape::MatchDestination`]
/// shapes the plant like the destination: a sentinel file (`.cl`
/// destination) or a directory holding one sentinel file (native directory
/// destination). [`PlantShape::Directory`] is always a directory holding one
/// sentinel file — even at a `.cl` bundle destination, where a directory
/// raced into the ADR-0161 window makes the exclusive create fail
/// (`IsADirectory` on Unix, `AccessDenied` on Windows) rather than
/// `AlreadyExists`.
#[doc(hidden)]
pub fn set_plant_destination(shape: Option<PlantShape>) {
    PLANT_DEST.with(|slot| slot.set(shape));
}

fn take_plant_destination() -> Option<PlantShape> {
    PLANT_DEST.with(|slot| slot.replace(None))
}

/// The bytes a planted race-window destination carries; tests assert they
/// survive the refused publish byte-identical.
#[doc(hidden)]
pub const PLANTED_SENTINEL: &[u8] = b"planted race-window destination";

fn plant_destination(dest: &Path, shape: PlantShape) {
    let bundle = match shape {
        PlantShape::MatchDestination => cypherlite_storage::is_bundle_path(dest),
        PlantShape::Directory => false,
    };
    if bundle {
        std::fs::write(dest, PLANTED_SENTINEL).expect("plant bundle destination");
    } else {
        std::fs::create_dir(dest).expect("plant directory destination");
        std::fs::write(dest.join("sentinel"), PLANTED_SENTINEL).expect("plant sentinel");
    }
}

/// Publish the cut a read-only open of `source_db` loaded into a new store
/// at `dest` (`.cl` file or native directory). The destination checkpoint is
/// written in `to_snapshot_format`, falling back to the source's codec when
/// `None` (ADR-0163); a requested codec this build does not include fails
/// inside `parse_uri_creating` before the destination exists. The
/// destination must not exist; a failure after it is created removes it.
/// `verb` ("clone") names the operation in error messages.
pub(crate) fn publish_into(
    dest: &str,
    source_db: &Database,
    verb: &'static str,
    to_snapshot_format: Option<SnapshotCodecKind>,
) -> Result<()> {
    let dest_path = uri_path(dest);
    reject_destination(dest_path, verb)?;
    if let Some(shape) = take_plant_destination() {
        plant_destination(dest_path, shape);
    }
    let codec = to_snapshot_format.unwrap_or_else(|| source_db.inner.store.snapshot_codec());
    // Create-exclusive (ADR-0161): a destination that appears between the
    // pre-check and the create is refused, never adopted.
    let dest_store = match parse_uri_creating(dest, None, Some(codec)) {
        Ok(store) => store,
        // The exclusive create made nothing, so there is nothing to remove:
        // the pre-existing destination must survive untouched.
        Err(Error::Storage(msg)) if msg.starts_with(DESTINATION_EXISTS) => {
            return Err(Error::storage(format!("{verb} {msg}")));
        }
        Err(err) => return remove_created(dest_path, err),
    };
    // Bundle stores have no `filesystem_dir`, so the cleanup target is the
    // `.cl` file path; a native directory destination cleans up the store
    // root.
    let dest_root = dest_store
        .filesystem_dir()
        .map_or_else(|| dest_path.to_path_buf(), Path::to_path_buf);
    if take_fail_after_dest() {
        return fail_after_create(
            dest_store,
            &dest_root,
            Error::storage(format!("{verb} failed after creating the destination")),
        );
    }
    if let Err(err) = publish_cut(source_db, &dest_store) {
        return fail_after_create(dest_store, &dest_root, err);
    }
    Ok(())
}

/// The shared epilogue of every post-create failure exit: drop the
/// destination store (releasing its flock and handles) before removal so
/// cleanup also succeeds on Windows (ADR-0161), then remove whatever the
/// operation created.
fn fail_after_create(
    dest_store: cypherlite_storage::StoreTarget,
    dest_root: &Path,
    err: Error,
) -> Result<()> {
    drop(dest_store);
    remove_created(dest_root, err)
}

/// Checkpoint the cut a read-only open of `source` loaded into `dest` and
/// start `dest` on an empty WAL (ADR-0156, ADR-0159). `checkpoint_at`
/// dispatches on `dest`'s codec, so the snapshot lands in the codec `dest`
/// was created with (ADR-0163) — the requested override or, absent one, the
/// source's codec.
pub(crate) fn publish_cut(source: &Database, dest: &cypherlite_storage::StoreTarget) -> Result<()> {
    let _guard = source.inner.read_guard()?;
    let lsn = source.inner.wal.lock()?.lsn;
    let snapshot_ts = source.inner.graph.commit_ts();
    // ADR-0120 §7: the clone carries the source's current ontology
    // document; the destination checkpoint publishes it in `dest`'s codec.
    checkpoint_at(
        dest,
        &source.inner.graph,
        lsn,
        snapshot_ts,
        source.inner.ontology.get().as_deref(),
    )?;
    init_empty_wal(dest, lsn)?;
    Ok(())
}

/// The destination did not exist before this call — enforced by the
/// exclusive create (ADR-0161): callers reach this only after a successful
/// `parse_uri_creating`, and the destination store handle is dropped first
/// so removal also succeeds on Windows. Remove whatever the operation
/// created, file (`.cl`) or directory (native). A missing path means
/// creation never happened, so the original error stands. Any other removal
/// failure is part of the returned error.
pub(crate) fn remove_created(path: &Path, err: Error) -> Result<()> {
    let removal = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match removal {
        Ok(()) => Err(err),
        Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => Err(err),
        Err(cleanup) => Err(Error::storage(format!(
            "{err} (removing {} failed: {cleanup})",
            path.display()
        ))),
    }
}

/// The destination must not exist (the ADR-0156 contract, every direction).
/// `verb` names the operation in error messages. One `symlink_metadata`
/// stat: an existing entry (file, directory, or symlink — dangling included)
/// is refused, a missing path only needs a final component, and any other
/// stat error propagates instead of being mistaken for "missing".
pub(crate) fn reject_destination(dest: &Path, verb: &str) -> Result<()> {
    match std::fs::symlink_metadata(dest) {
        Ok(_) => Err(Error::storage(format!("{verb} destination already exists"))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if dest.file_name().is_none() {
                return Err(Error::storage(format!(
                    "{verb} destination is not a valid path"
                )));
            }
            Ok(())
        }
        Err(err) => Err(Error::storage(format!("{verb} destination: {err}"))),
    }
}

pub(crate) fn uri_path(uri: &str) -> &Path {
    Path::new(uri.strip_prefix("file://").unwrap_or(uri))
}
