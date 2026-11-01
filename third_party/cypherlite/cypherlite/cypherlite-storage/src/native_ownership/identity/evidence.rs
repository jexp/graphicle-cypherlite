//! Directory-evidence classification: which legacy bytes prove a root or a
//! WAL, and which directory entries make a directory non-empty.
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::Path;

use crate::codec::SnapshotCodecKind;
use crate::vfs::native::{native_entry_kind, NativeEntryKind};
use crate::vfs::NativeVfs;
use cypherlite_core::file_identity::nofollow;
use cypherlite_core::{Error, Result};

use super::super::LOCK_MARKER;
use super::is_same_dir;

/// Whether an entry exists by lstat: `NotFound` means absent, every other
/// error propagates — the one typed spelling of the existence idiom.
pub(crate) fn lstat_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn empty_except_lock(path: &Path) -> Result<bool> {
    for entry in fs::read_dir(path)? {
        if entry?.file_name() != LOCK_MARKER {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Lock `path`'s marker (unless a lease on it is already held) and read it.
///
/// Missing markers read as an empty byte string. A marker entry that exists
/// but is not a plain regular file is refused; it must never be silently
/// treated as absent. Read failures propagate: admission stays fail-closed.
pub(super) fn lock_and_read_marker(path: &Path, locked: Option<&File>) -> Result<Vec<u8>> {
    let marker = path.join(LOCK_MARKER);
    if let Some(file) = locked {
        return NativeVfs::read_locked_marker(file, &marker);
    }
    match fs::symlink_metadata(&marker) {
        Ok(meta) if matches!(native_entry_kind(&meta), NativeEntryKind::File) => {
            NativeVfs::read_locked_marker(
                &*NativeVfs::lock_existing_regular_file(&marker)?,
                &marker,
            )
        }
        Ok(_) => Err(Error::storage(format!(
            "invalid lock marker {}",
            marker.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn root_specific_evidence(path: &Path) -> Result<bool> {
    // The per-codec root-evidence registry, consulted through the codec
    // dispatch surface: each codec's checkpoint image is the file whose
    // presence proves that codec's root (manifest.json / graph.json /
    // .cypherlite/manifest.json). COORDINATION NOTE (stellar F-14-3):
    // codec/* is another change's file in this round, so the registry is
    // iterated through the existing `checkpoint_image()` mapping rather
    // than a dedicated `SnapshotCodecKind::root_evidence_files()` beside
    // `wal_dir()` in codec/kind.rs; if a codec's admission evidence ever
    // diverges from its checkpoint image, promote this to that method and
    // iterate it here.
    for name in [
        SnapshotCodecKind::Parquet.checkpoint_image(),
        SnapshotCodecKind::Json.checkpoint_image(),
        SnapshotCodecKind::Markdown.checkpoint_image(),
    ] {
        match fs::symlink_metadata(path.join(name)) {
            Ok(meta) if matches!(native_entry_kind(&meta), NativeEntryKind::File) => {
                return Ok(true)
            }
            Ok(_) => {
                return Err(Error::storage(format!(
                    "invalid root evidence at {}",
                    path.join(name).display()
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(false)
}

pub(super) fn root_evidence(path: &Path) -> Result<bool> {
    if root_specific_evidence(path)? {
        return Ok(true);
    }
    let direct = path.join(SnapshotCodecKind::Json.wal_dir());
    match fs::symlink_metadata(&direct) {
        Ok(meta) if matches!(native_entry_kind(&meta), NativeEntryKind::Dir) => {
            let marker = direct.join(LOCK_MARKER);
            match fs::symlink_metadata(&marker) {
                Ok(meta) if matches!(native_entry_kind(&meta), NativeEntryKind::File) => {
                    // A typed child alone does not establish this root.
                    // Check segment evidence first so a live unrelated
                    // child without WAL data remains an ambiguous root,
                    // rather than reporting contention on that child.
                    if !wal_evidence(&direct)? {
                        return Ok(false);
                    }
                    Ok(lock_and_read_marker(&direct, None)? == super::WAL_ROLE)
                }
                Ok(_) => Err(Error::storage(format!(
                    "invalid lock marker {}",
                    marker.display()
                ))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(e.into()),
            }
        }
        Ok(_) => Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub(super) fn wal_evidence(path: &Path) -> Result<bool> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Admission and the WAL scanner share the scanner's one
        // segment-name classifier, so they cannot disagree about what a
        // WAL segment looks like.
        if crate::wal::parse_seg_name(&name).is_none() {
            continue;
        }
        let mut opts = OpenOptions::new();
        opts.read(true);
        nofollow(&mut opts);
        let mut file = NativeVfs::open_checked_existing(&entry.path(), &opts)?;
        let mut magic = [0; 8];
        file.read_exact(&mut magic)?;
        // Admission and the WAL scanner share the registry's one
        // readable-magic classification (ADR-0127 Decision 5), so they
        // cannot disagree about what a WAL segment looks like.
        if crate::version::wal::is_readable(&magic) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `wal` is the root's physical direct WAL child: the direct
/// `wal/` slot exists as a plain directory and `wal` names it (possibly
/// through an alias spelling).
pub(crate) fn direct_wal(root: &Path, wal: &Path) -> Result<bool> {
    let direct = root.join(SnapshotCodecKind::Json.wal_dir());
    match fs::symlink_metadata(&direct) {
        Ok(meta) if matches!(native_entry_kind(&meta), NativeEntryKind::Dir) => (),
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    match fs::metadata(wal) {
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    // The direct child must itself be plain, but a caller's WAL spelling
    // may be an alias to that physical directory.
    is_same_dir(&direct, wal)
}
