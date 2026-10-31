//! Shared physical path, nested-owner and role admission policy for Native open and reset.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::vfs::is_bundle_path;
use crate::vfs::native::plain_entry;
use crate::vfs::NativeVfs;
use cypherlite_core::file_identity;
use cypherlite_core::file_identity::nofollow;
use cypherlite_core::{Error, Result};

use super::LOCK_MARKER;

/// Stable role bytes for the root database directory lock marker (ADR-0090).
///
/// Exported so ownership admission, the WAL magic check and tests share one
/// byte string instead of private copies.
pub const ROOT_ROLE: &[u8] = b"CYPHERLITE-LOCK-v1 root\n";
/// Stable role bytes for the WAL directory lock marker (ADR-0090).
pub const WAL_ROLE: &[u8] = b"CYPHERLITE-LOCK-v1 wal\n";

impl DirectoryRole {
    pub(super) fn marker(self) -> &'static [u8] {
        match self {
            Self::Root => ROOT_ROLE,
            Self::Wal => WAL_ROLE,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum DirectoryRole {
    Root,
    Wal,
}

/// Admission inputs for role preflight and locked-role checks.
///
/// Named fields keep call sites readable; positional trailing bools made the
/// three independent switches easy to transpose. `newly_created` is consulted
/// only under a held lease ([`check_locked_role`]); a lease-less preflight
/// always sees a pre-existing marker.
#[derive(Clone, Copy)]
pub(super) struct RoleCheck {
    /// Reset is the explicit migration barrier that admits legacy evidence.
    pub reset: bool,
    /// The WAL is the physical direct child of this root.
    pub direct_wal: bool,
    /// The marker was created by this open, so a changed identity is refused.
    pub newly_created: bool,
}

pub(super) fn direct_wal(root: &Path, wal: &Path) -> Result<bool> {
    let direct = root.join("wal");
    match fs::symlink_metadata(&direct) {
        Ok(meta) if meta.is_dir() && plain_entry(&meta) => (),
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

/// Lock `path`'s marker (unless a lease on it is already held) and read it.
///
/// Missing markers read as an empty byte string. A marker entry that exists
/// but is not a plain regular file is refused; it must never be silently
/// treated as absent. Read failures propagate: admission stays fail-closed.
fn lock_and_read_marker(path: &Path, locked: Option<&File>) -> Result<Vec<u8>> {
    let marker = path.join(LOCK_MARKER);
    if let Some(file) = locked {
        return NativeVfs::read_locked_marker(file, &marker);
    }
    match fs::symlink_metadata(&marker) {
        Ok(meta) if meta.is_file() && plain_entry(&meta) => NativeVfs::read_locked_marker(
            &*NativeVfs::lock_existing_regular_file(&marker)?,
            &marker,
        ),
        Ok(_) => Err(Error::storage(format!(
            "invalid lock marker {}",
            marker.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

/// The operator has verified the *ownership* of this direct WAL offline.
/// Layout alone cannot distinguish it from a nested legacy root == WAL.
/// This only checks eligibility; ordinary open/reset never call it.
pub(super) fn preflight_manual_wal(path: &Path, locked: Option<&File>) -> Result<bool> {
    if root_specific_evidence(path)? || !wal_evidence(path)? {
        return Err(Error::storage(format!(
            "offline WAL adoption requires segment magic and no root evidence at {}",
            path.display()
        )));
    }
    let bytes = lock_and_read_marker(path, locked)?;
    if bytes.is_empty() {
        Ok(true)
    } else if bytes == WAL_ROLE {
        Ok(false)
    } else {
        Err(Error::storage(format!(
            "wrong or malformed lock role at {}",
            path.join(LOCK_MARKER).display()
        )))
    }
}

/// Inspect without creating a marker. Returns whether a typed role marker
/// was verified. Reset requires positive legacy evidence; ordinary open
/// may adopt only an otherwise empty, previously unused root.
pub(super) fn preflight_role(path: &Path, role: DirectoryRole, check: RoleCheck) -> Result<bool> {
    preflight_role_with_lease(path, role, check, None)
}

fn preflight_role_with_lease(
    path: &Path,
    role: DirectoryRole,
    check: RoleCheck,
    locked: Option<&File>,
) -> Result<bool> {
    let dir = match fs::metadata(path) {
        Ok(meta) if meta.is_dir() => true,
        Ok(_) => {
            return Err(Error::storage(format!(
                "{} is not a directory",
                path.display()
            )))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.into()),
    };
    if !dir {
        return Ok(false);
    }
    let marker = path.join(LOCK_MARKER);
    let bytes = lock_and_read_marker(path, locked)?;
    if !bytes.is_empty() {
        if bytes != role.marker() || (role == DirectoryRole::Wal && root_evidence(path)?) {
            return Err(Error::storage(format!(
                "wrong or malformed lock role at {}",
                marker.display()
            )));
        }
        return Ok(true);
    }
    // Preflight existing roles before creating either marker. The final
    // lease holder checks the marker again against concurrent changes.
    if role == DirectoryRole::Root {
        let root_evidence = if check.direct_wal {
            root_specific_evidence(path)?
        } else {
            root_evidence(path)?
        };
        // A same-directory segment cannot prove root identity, even
        // though it proves this directory contains WAL-format bytes.
        if root_evidence || (!check.reset && empty_except_lock(path)? && !wal_evidence(path)?) {
            return Ok(false);
        }
    }
    Err(Error::storage(format!(
        "ambiguous legacy storage role at {}; explicit offline migration required",
        path.display()
    )))
}

pub(super) fn empty_except_lock(path: &Path) -> Result<bool> {
    for entry in fs::read_dir(path)? {
        if entry?.file_name() != LOCK_MARKER {
            return Ok(false);
        }
    }
    Ok(true)
}

fn root_specific_evidence(path: &Path) -> Result<bool> {
    for name in ["manifest.json", "graph.json"] {
        match fs::symlink_metadata(path.join(name)) {
            Ok(meta) if meta.is_file() && plain_entry(&meta) => return Ok(true),
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

fn root_evidence(path: &Path) -> Result<bool> {
    if root_specific_evidence(path)? {
        return Ok(true);
    }
    let direct = path.join("wal");
    match fs::symlink_metadata(&direct) {
        Ok(meta) if meta.is_dir() && plain_entry(&meta) => {
            let marker = direct.join(LOCK_MARKER);
            match fs::symlink_metadata(&marker) {
                Ok(meta) if meta.is_file() && plain_entry(&meta) => {
                    // A typed child alone does not establish this root.
                    // Check segment evidence first so a live unrelated
                    // child without WAL data remains an ambiguous root,
                    // rather than reporting contention on that child.
                    if !wal_evidence(&direct)? {
                        return Ok(false);
                    }
                    Ok(lock_and_read_marker(&direct, None)? == WAL_ROLE)
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

fn wal_evidence(path: &Path) -> Result<bool> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.len() != 12
            || !name.ends_with(".log")
            || !name[..8].bytes().all(|b| b.is_ascii_digit())
        {
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

/// Read-only admission. Does not write the marker.
///
/// A root must be [`ROOT_ROLE`] or an empty legacy marker on a directory
/// that already has root evidence. A WAL must be [`WAL_ROLE`] or empty.
pub(super) fn accept_readonly_marker(path: &Path, file: &File, role: DirectoryRole) -> Result<()> {
    let marker = path.join(LOCK_MARKER);
    NativeVfs::check_identity(file, &marker)?;
    let bytes = NativeVfs::read_locked_marker(file, &marker)?;
    if bytes == role.marker() {
        return Ok(());
    }
    if bytes.is_empty() {
        if role == DirectoryRole::Wal {
            return Ok(());
        }
        if root_evidence(path)? {
            return Ok(());
        }
        return Err(Error::storage(format!(
            "read-only open requires a root database at {}",
            path.display()
        )));
    }
    Err(Error::storage(format!(
        "wrong or malformed lock role at {}",
        marker.display()
    )))
}

pub(super) fn check_locked_role(
    path: &Path,
    file: &File,
    role: DirectoryRole,
    check: RoleCheck,
) -> Result<bool> {
    let marker = path.join(LOCK_MARKER);
    NativeVfs::check_identity(file, &marker)?;
    let bytes = NativeVfs::read_locked_marker(file, &marker)?;
    if bytes == role.marker() {
        if role == DirectoryRole::Wal && root_evidence(path)? {
            return Err(Error::storage(format!(
                "WAL identity {} also contains root evidence",
                path.display()
            )));
        }
        return Ok(false);
    }
    if !bytes.is_empty() {
        return Err(Error::storage(format!(
            "wrong or malformed lock role at {}",
            marker.display()
        )));
    }
    if check.newly_created {
        if role == DirectoryRole::Wal && !empty_except_lock(path)? {
            return Err(Error::storage(format!(
                "new storage identity changed at {}",
                path.display()
            )));
        }
    } else {
        preflight_role_with_lease(path, role, check, Some(file))?;
    }
    Ok(true)
}

/// Role write for a marker this process holds `LOCK_SH` (ADR-0136).
pub(super) fn write_shared_lease_role(path: &Path, file: &File, role: DirectoryRole) -> Result<()> {
    NativeVfs::write_under_shared_lock(file, &path.join(LOCK_MARKER), role.marker())?;
    sync_marker_parent(path)
}

pub(super) fn write_locked_role(path: &Path, file: &File, role: DirectoryRole) -> Result<()> {
    (&*file).write_all(role.marker())?;
    file.sync_all()?;
    // On Windows external-WAL reset is refused before we reach this point.
    sync_marker_parent(path)
}

/// Persist a newly created marker directory entry. Unix fsyncs the parent
/// directory; other platforms have no directory fsync.
pub(super) fn sync_marker_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// Resolve actual ancestors, never interpreting a WAL or root lock as a
/// symlink alias. Called both before creation and after taking our lease.
pub(crate) fn reject_nested(path: &Path) -> Result<()> {
    let resolved = resolve_identity_path(path)?;
    let mut ancestor = resolved.parent();
    while let Some(dir) = ancestor {
        let marker = dir.join(LOCK_MARKER);
        match fs::symlink_metadata(&marker) {
            Ok(_) => {
                return Err(Error::storage(format!(
                    "database path {} is nested under persistent storage identity {}",
                    resolved.display(),
                    marker.display()
                )))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        ancestor = dir.parent();
    }
    Ok(())
}
// Resolve existing symlink aliases and the nearest existing ancestor of an
// absent path. A nonexistent component followed by `..` is ambiguous (the OS
// cannot traverse it); refuse rather than guess its eventual identity.
pub(super) fn resolve_identity_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut cursor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(_) => {
                let mut resolved = std::fs::canonicalize(cursor)?;
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if cursor.components().next_back() == Some(std::path::Component::ParentDir) {
                    return Err(Error::storage(format!(
                        "cannot resolve reset path through a missing parent: {}",
                        path.display()
                    )));
                }
                let part = cursor.file_name().ok_or_else(|| {
                    Error::storage(format!("cannot resolve reset path {}", path.display()))
                })?;
                missing.push(part.to_os_string());
                cursor = cursor.parent().ok_or_else(|| {
                    Error::storage(format!("cannot resolve reset path {}", path.display()))
                })?;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

// Do not recurse through symlinks or Windows junctions. A reset unlinks the
// alias itself, not the independently owned tree it points to.
pub(super) fn scan_nested_databases(root: &Path, wal: Option<&Path>) -> Result<()> {
    scan_nested_databases_with_trusted_wal(root, wal, false)
}

// The exemption is available for a WAL just created by this open, a typed
// direct WAL already verified under its lease, or a direct WAL explicitly
// verified by the operator for offline adoption.
pub(super) fn scan_nested_databases_with_trusted_wal(
    root: &Path,
    wal: Option<&Path>,
    trusted_direct_wal: bool,
) -> Result<()> {
    // The root may retain a typed default WAL after switching to an external
    // override. A pre-existing untyped direct WAL never proves its ownership,
    // even when the parent has a manifest or is still using that WAL path.
    let default_wal = root.join("wal");
    let owned_wal = if let Some(wal) = wal {
        if direct_wal(root, &default_wal)? {
            let direct = direct_wal(root, wal)?;
            if trusted_direct_wal && direct {
                true
            } else {
                preflight_role(
                    &default_wal,
                    DirectoryRole::Wal,
                    RoleCheck {
                        reset: true,
                        direct_wal: true,
                        newly_created: false,
                    },
                )?
            }
        } else {
            false
        }
    } else {
        false
    };
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let lock_name = name
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(LOCK_MARKER));
            let bundle_name = is_bundle_path(&path);
            let kind = entry.file_type()?;
            #[cfg(windows)]
            let reparse = {
                use std::os::windows::fs::MetadataExt;
                // A junction may report as a directory. Inspect directories
                // before recursing, and candidates before claiming identity.
                (kind.is_dir() || lock_name || bundle_name)
                    && std::fs::symlink_metadata(&path)?.file_attributes() & 0x0400 != 0
            };
            #[cfg(not(windows))]
            let reparse = false;
            if lock_name && !owned_identity_marker(&path, root, owned_wal)? {
                return Err(Error::storage(format!(
                    "reset refuses nested database identity {}",
                    path.display()
                )));
            }
            if kind.is_symlink() || reparse {
                if bundle_name {
                    let target = std::fs::canonicalize(&path)?;
                    if reset_path_starts_with(&target, root)? {
                        return Err(Error::storage(format!(
                            "reset refuses nested .cl bundle alias {}",
                            path.display()
                        )));
                    }
                }
                continue;
            }
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() && bundle_name {
                return Err(Error::storage(format!(
                    "reset refuses nested .cl bundle {}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

// Existing prefixes are canonicalized by resolve_identity_path. For missing
// components, macOS and Windows may resolve spellings with different case to
// the same future directory. Treat such paths as overlapping even when the
// current volume is case-sensitive (a conservative rejection).
pub(super) fn reset_path_starts_with(path: &Path, prefix: &Path) -> Result<bool> {
    let mut path = path.components();
    #[cfg(any(target_os = "macos", windows))]
    let (mut path_seen, mut prefix_seen) = (PathBuf::new(), PathBuf::new());
    for part in prefix.components() {
        let Some(candidate) = path.next() else {
            return Ok(false);
        };
        #[cfg(any(target_os = "macos", windows))]
        let equal = {
            path_seen.push(candidate.as_os_str());
            prefix_seen.push(part.as_os_str());
            let left = candidate.as_os_str().to_string_lossy();
            let right = part.as_os_str().to_string_lossy();
            candidate == part
                || left.to_lowercase() == right.to_lowercase()
                || ((!left.is_ascii() || !right.is_ascii())
                    && !reset_provably_distinct(&path_seen, &prefix_seen)?)
        };
        #[cfg(not(any(target_os = "macos", windows)))]
        let equal = candidate == part;
        if !equal {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(any(target_os = "macos", windows))]
fn reset_provably_distinct(left: &Path, right: &Path) -> Result<bool> {
    // Missing components may normalize or case-fold under the eventual
    // volume. An absent prefix cannot prove non-overlap, so it stays
    // conservatively "not distinct"; a resolution failure on an existing
    // path is a real error and propagates.
    let canonical = |path: &Path| -> Result<Option<PathBuf>> {
        match std::fs::canonicalize(path) {
            Ok(resolved) => Ok(Some(resolved)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    };
    match (canonical(left)?, canonical(right)?) {
        (Some(left), Some(right)) => Ok(!is_same_dir(&left, &right)?),
        _ => Ok(false),
    }
}

/// Whether two paths name the same directory.
///
/// Resolves existing symlink aliases and the nearest existing ancestor of an
/// absent path, so a missing database path still compares by its eventual
/// location. Resolution failures propagate; they are never read as "different".
pub(super) fn is_same_dir(left: &Path, right: &Path) -> Result<bool> {
    Ok(resolve_identity_path(left)? == resolve_identity_path(right)?)
}

pub(super) fn same_physical_identity(left: &Path, right: &Path) -> Result<bool> {
    Ok(file_identity::of_path(left)? == file_identity::of_path(right)?)
}

fn owned_identity_entry(actual: &Path, expected: &Path, directory: bool) -> Result<bool> {
    let actual_meta = std::fs::symlink_metadata(actual)?;
    let expected_meta = match std::fs::symlink_metadata(expected) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    #[cfg(windows)]
    let reparse = {
        use std::os::windows::fs::MetadataExt;
        expected_meta.file_attributes() & 0x0400 != 0
    };
    #[cfg(not(windows))]
    let reparse = false;
    Ok(!expected_meta.file_type().is_symlink()
        && crate::vfs::native::plain_entry(&actual_meta)
        && !reparse
        && if directory {
            actual_meta.is_dir() && expected_meta.is_dir()
        } else {
            actual_meta.is_file() && expected_meta.is_file()
        }
        && same_physical_identity(actual, expected)?)
}

fn owned_identity_marker(path: &Path, root: &Path, owned_wal: bool) -> Result<bool> {
    if path.parent() == Some(root) && owned_identity_entry(path, &root.join(LOCK_MARKER), false)? {
        return Ok(true);
    }
    if owned_wal && path.parent().and_then(Path::parent) == Some(root) {
        let parent = path.parent().expect("marker has a parent");
        return Ok(owned_identity_entry(parent, &root.join("wal"), true)?
            && owned_identity_entry(path, &root.join("wal").join(LOCK_MARKER), false)?);
    }
    Ok(false)
}

fn direct_reset_wal(root: &Path, wal: &Path) -> Result<bool> {
    if wal == root.join("wal") {
        return Ok(true);
    }
    // A differently-cased *existing* direct directory can be exempted only
    // when the directory reached by <root>/wal is physically the WAL itself.
    // A similarly spelled but independent directory on a case-sensitive
    // volume, or an outbound wal symlink, never qualifies.
    let lexically_contained = match wal.parent() {
        Some(parent) => reset_path_starts_with(parent, root)?,
        None => false,
    };
    if !lexically_contained || wal.components().count() != root.components().count() + 1 {
        return Ok(false);
    }
    match std::fs::symlink_metadata(wal) {
        Ok(_) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    owned_identity_entry(wal, &root.join("wal"), true)
}

pub(super) fn validate_reset_overlap(root: &Path, wal: &Path) -> Result<()> {
    if (reset_path_starts_with(root, wal)? || reset_path_starts_with(wal, root)?)
        && !direct_reset_wal(root, wal)?
    {
        return Err(Error::storage(format!(
            "reset WAL directory {} overlaps database root {}; only a direct contained wal directory is safe",
            wal.display(),
            root.display()
        )));
    }
    Ok(())
}
