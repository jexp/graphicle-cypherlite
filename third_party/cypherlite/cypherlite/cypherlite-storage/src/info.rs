//! Cold store-metadata read (ADR-0165): answer "what is in this store?"
//! without opening it — no `Wal::open`, no shard or Parquet load, no WAL
//! replay, and no lock acquisition, so a pre-open diagnostic can peek beside
//! a live writer.

use std::path::Path;
use std::sync::Arc;

use cypherlite_core::{Error, Result};

use crate::vfs::{is_bundle_path, BundleVfs, NativeVfs};
use crate::{
    codec, filesystem_uri_path, is_memory_uri, reject_remote_scheme, wal, Manifest,
    SnapshotCodecKind, StoreTarget,
};

/// What physical container a store path holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreLayout {
    /// A native filesystem directory.
    NativeDir,
    /// A `.cl` single-file bundle (ADR-0074) with its superblock `VERSION`.
    Bundle { superblock_version: u32 },
}

/// The snapshot metadata a cold read recovers without opening the store.
// The ADR-0165 contract is `Parquet(Manifest)` by value; one cold read
// returns one StoreInfo, so the enum size is not on a hot path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum StoreSnapshot {
    /// Parquet-codec store: the full selected manifest.
    Parquet(Manifest),
    /// JSON-codec store: `manifest.json` holds only the codec envelope
    /// (ADR-0050); counts and the catalog live in `graph.json`, which a
    /// cold info read never parses.
    Json { checkpoint_lsn: u64, commit_ts: u64 },
    /// No snapshot yet: a fresh store before its first checkpoint has no
    /// `manifest.json`. This is a state to report, not an error — the typed
    /// signal callers match on instead of an error-message substring.
    None,
}

/// Cold-read store metadata: the selected snapshot metadata plus on-disk
/// facts, gathered without opening the store.
#[derive(Debug, Clone)]
pub struct StoreInfo {
    pub layout: StoreLayout,
    pub snapshot: StoreSnapshot,
    /// Recursive directory sum, or the bundle host-file length.
    pub total_bytes: u64,
    /// WAL segments past the checkpoint. Checkpoint recycles every segment
    /// it covers, so each remaining segment may hold commits newer than
    /// `checkpoint_lsn` — a staleness hint, not an exact count. A store
    /// opened with a `wal_dir` override keeps its segments elsewhere; only
    /// the default `wal/` location is counted.
    pub wal_segments: u32,
}

/// Read store metadata cold (ADR-0165), for native directories and `.cl`
/// bundles alike. Never opens the store: no `Wal::open`, no shard load, no
/// lock acquisition. A fresh store before its first checkpoint is not a
/// failure: it reports [`StoreSnapshot::None`]. Failures are typed
/// [`Error::Storage`] values, never panics: missing database, corrupt
/// `manifest.json`, and an unreadable `format_version` rejected by the
/// ADR-0127 manifest gate.
///
/// Beside a live writer the bundle path is platform-split: Unix locks are
/// advisory, so the lock-free peek reads beside the writer, but the
/// writer's mandatory whole-file `LockFileEx` lock on Windows forbids even
/// a lock-free read from any other handle, so a writer-held `.cl` bundle
/// fails there with the same typed [`Error::Locked`] a locking open
/// returns (ADR-0136). A native directory reads beside its writer on every
/// platform: the writer locks only its marker files, never `manifest.json`
/// or the shards.
///
/// # Example
/// ```
/// use cypherlite_storage::{checkpoint, parse_uri, store_info, StoreLayout};
///
/// let dir = tempfile::tempdir().unwrap();
/// let db = dir.path().join("db");
/// let target = parse_uri(db.to_str().unwrap()).unwrap();
/// checkpoint(&target, &cypherlite_executor::Graph::default(), None).unwrap();
///
/// // The live writer still holds its locks; the cold read works beside it.
/// let info = store_info(&db).unwrap();
/// assert_eq!(info.layout, StoreLayout::NativeDir);
/// assert!(info.total_bytes > 0);
/// ```
pub fn store_info(path: &Path) -> Result<StoreInfo> {
    if is_bundle_path(path) {
        bundle_info(path)
    } else {
        native_info(path)
    }
}

/// [`store_info`] from a store URI instead of a path, with the same scheme
/// classification `parse_uri_inner` owns: a `file://` prefix is stripped, a
/// remote scheme (`s3://`, …) is rejected with [`Error::Storage`], and
/// `:memory:` fails with a typed [`Error::Storage`] — an in-memory store has
/// no on-disk layout to describe.
///
/// # Example
/// ```
/// use cypherlite_storage::{checkpoint, parse_uri, store_info_uri, StoreLayout};
///
/// let dir = tempfile::tempdir().unwrap();
/// let db = dir.path().join("db");
/// let target = parse_uri(db.to_str().unwrap()).unwrap();
/// checkpoint(&target, &cypherlite_executor::Graph::default(), None).unwrap();
///
/// let info = store_info_uri(&format!("file://{}", db.display())).unwrap();
/// assert_eq!(info.layout, StoreLayout::NativeDir);
/// assert!(store_info_uri(":memory:").is_err());
/// assert!(store_info_uri("s3://bucket/db").is_err());
/// ```
pub fn store_info_uri(uri: &str) -> Result<StoreInfo> {
    if is_memory_uri(uri) {
        return Err(Error::storage(
            "store info is not available for a :memory: store (no on-disk store)",
        ));
    }
    reject_remote_scheme(uri)?;
    store_info(Path::new(filesystem_uri_path(uri)))
}

fn native_info(path: &Path) -> Result<StoreInfo> {
    let meta = std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::storage(format!("database not found: {}", path.display()))
        } else {
            Error::from(e)
        }
    })?;
    if !meta.is_dir() {
        return Err(Error::storage(format!(
            "{} is not a database directory",
            path.display()
        )));
    }
    let total_bytes = dir_bytes(path)?;
    let target = StoreTarget::from_vfs(Arc::new(NativeVfs::new(path)));
    collect(&target, StoreLayout::NativeDir, total_bytes)
}

fn bundle_info(path: &Path) -> Result<StoreInfo> {
    // resolve_bundle_path (HostAdmit::Shared) inside open_peek is the one
    // classification taxonomy for a missing or non-file bundle path; its
    // errors already carry the path. The host file is statted once, inside
    // the peek load. On Windows a writer-held bundle answers the typed
    // `Error::Locked` of open_peek's lock probe — the mandatory whole-file
    // lock admits no reader but its owner.
    let (vfs, superblock_version, host_len) = BundleVfs::open_peek(path)?;
    let target = StoreTarget::from_vfs(Arc::new(vfs));
    collect(
        &target,
        StoreLayout::Bundle { superblock_version },
        host_len,
    )
}

/// The shared tail of a cold read: snapshot metadata, WAL segment count and
/// `StoreInfo` assembly, identical for both layouts. Callers keep only their
/// layout-specific setup (path checks, byte total, target construction).
fn collect(target: &StoreTarget, layout: StoreLayout, total_bytes: u64) -> Result<StoreInfo> {
    let snapshot = read_snapshot(target)?;
    let (wal_vfs, wal_prefix) = target.wal_vfs();
    let wal_segments = wal::segment_count(&*wal_vfs, wal_prefix)?;
    Ok(StoreInfo {
        layout,
        snapshot,
        total_bytes,
        wal_segments,
    })
}

/// The snapshot half of a cold read: the codec envelope decides whether
/// `manifest.json` is a full Parquet manifest or a JSON-codec envelope
/// (ADR-0165 Decision 5). No envelope at all is the fresh-store state:
/// [`StoreSnapshot::None`], not an error.
fn read_snapshot(target: &StoreTarget) -> Result<StoreSnapshot> {
    let Some(envelope) = codec::read_envelope(target)? else {
        return Ok(StoreSnapshot::None);
    };
    match envelope.codec {
        SnapshotCodecKind::Json => Ok(StoreSnapshot::Json {
            checkpoint_lsn: envelope.checkpoint_lsn,
            commit_ts: envelope.commit_ts,
        }),
        SnapshotCodecKind::Parquet => parquet_snapshot(target),
    }
}

/// The real manifest reader, version gate included (ADR-0127) — the cold
/// read must accept and reject exactly what open does.
#[cfg(parquet_codec)]
fn parquet_snapshot(target: &StoreTarget) -> Result<StoreSnapshot> {
    let Some((manifest, _marker)) = crate::snapshot::native::read_manifest(target)? else {
        // The envelope read above just saw manifest.json; a concurrent reset
        // removed it in between. Report, never panic.
        return Err(Error::storage("manifest.json disappeared during read"));
    };
    Ok(StoreSnapshot::Parquet(manifest))
}

/// A Parquet manifest in a build without the codec fails closed, exactly
/// like open does (ADR-0105).
#[cfg(not(parquet_codec))]
fn parquet_snapshot(_: &StoreTarget) -> Result<StoreSnapshot> {
    Err(codec::parquet_unavailable())
}

/// Recursive directory byte sum. Symlinks and special entries are neither
/// followed nor counted.
fn dir_bytes(root: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.is_dir() {
                pending.push(path);
            } else if meta.is_file() {
                total = total
                    .checked_add(meta.len())
                    .ok_or_else(|| Error::storage("store size overflow"))?;
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests;
