//! Shared physical path, nested-owner and role admission policy for Native open and reset.
//!
//! Split along the policy seams (the no-1k-line rule, LABS-311):
//! [`evidence`] classifies directory evidence, [`preflight`] admits roles
//! against it, and [`scan`] walks nested owners. The path/shape facts every
//! seam shares stay here.
use std::path::{Path, PathBuf};

use cypherlite_core::{Error, Result};

use super::LOCK_MARKER;

mod evidence;
mod preflight;
mod scan;

pub(crate) use evidence::{direct_wal, empty_except_lock, lstat_exists};
pub(crate) use preflight::{
    accept_readonly_marker, check_locked_role, preflight_manual_wal, preflight_role,
    sync_marker_parent, write_locked_role, write_shared_lease_role, RoleCheck,
};
pub(crate) use scan::{
    overlap, scan_nested_databases, validate_reset_overlap, Overlap, WalExemption,
};
// The direction-sensitive prefix primitive is crate-facing only for the
// case-folded-path tests; production consumers go through `overlap`.
#[cfg(all(test, any(target_os = "macos", windows)))]
pub(crate) use scan::reset_path_starts_with;

/// Stable role bytes for the root database directory lock marker (ADR-0090).
///
/// Exported so ownership admission, the WAL magic check and tests share one
/// byte string instead of private copies.
pub const ROOT_ROLE: &[u8] = b"CYPHERLITE-LOCK-v1 root\n";
/// Stable role bytes for the WAL directory lock marker (ADR-0090).
pub const WAL_ROLE: &[u8] = b"CYPHERLITE-LOCK-v1 wal\n";

/// Which lock marker a directory admits: derived from [`RoleCheck`]'s
/// variant, never a second argument that can disagree with it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DirectoryRole {
    Root,
    Wal,
}

impl DirectoryRole {
    pub(super) fn marker(self) -> &'static [u8] {
        match self {
            Self::Root => ROOT_ROLE,
            Self::Wal => WAL_ROLE,
        }
    }
}

/// The root's storage shape (ADR-0406): an ordinary engine-owned root, or a
/// markdown vault whose content is hand-authored and whose WAL lives under
/// `.cypherlite/wal`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootShape {
    Ordinary,
    Vault,
}

/// The markdown vault store's default WAL directory (ADR-0406 §3), built
/// from the codec layer's one spelling of the fact.
pub(super) fn markdown_wal(root: &Path) -> PathBuf {
    root.join(crate::codec::SnapshotCodecKind::Markdown.wal_dir())
}

/// The root's shape per its WAL spelling (ADR-0406 §3): the markdown default
/// `<root>/.cypherlite/wal` marks a vault root. Lexical: the open path
/// constructs the spelling; canonicalization is the caller's.
pub(crate) fn markdown_root_shape(root: &Path, wal: &Path) -> RootShape {
    if *wal == markdown_wal(root) {
        RootShape::Vault
    } else {
        RootShape::Ordinary
    }
}

/// Resolve existing symlink aliases and the nearest existing ancestor of an
/// absent path. A nonexistent component followed by `..` is ambiguous (the OS
/// cannot traverse it); refuse rather than guess its eventual identity.
// Reset inspects the caller's final named component before
// canonicalization; ordinary open is still allowed to resolve supported
// aliases. The reset-admission trio lives here, beside the
// identity/entry vocabulary it shares (the ownership layer owns reset).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn reset_request_path(path: &Path) -> PathBuf {
    path.components().collect()
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn reject_reset_alias(path: &Path) -> Result<()> {
    let path = reset_request_path(path);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if !crate::vfs::native::plain_entry(&meta) => Err(Error::storage(format!(
            "reset refuses database URI alias {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn verify_reset_identity(
    requested: &Path,
    target: &Path,
    expected: cypherlite_core::file_identity::FileIdentity,
) -> Result<()> {
    let requested = reset_request_path(requested);
    reject_reset_alias(&requested)?;
    let current = std::fs::symlink_metadata(&requested).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::storage(format!(
                "reset database URI identity changed: {}",
                requested.display()
            ))
        } else {
            e.into()
        }
    })?;
    let target_meta = std::fs::symlink_metadata(target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::storage(format!(
                "reset target identity changed: {}",
                target.display()
            ))
        } else {
            e.into()
        }
    })?;
    if cypherlite_core::file_identity::of_path(&requested)? != expected
        || cypherlite_core::file_identity::of_path(target)? != expected
        || !crate::vfs::native::plain_entry(&current)
        || !crate::vfs::native::plain_entry(&target_meta)
    {
        return Err(Error::storage(format!(
            "reset database URI identity changed: {}",
            requested.display()
        )));
    }
    Ok(())
}

pub(crate) fn resolve_identity_path(path: &Path) -> Result<PathBuf> {
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

/// Resolve actual ancestors, never interpreting a WAL or root lock as a
/// symlink alias. Called both before creation and after taking our lease.
pub(crate) fn reject_nested(path: &Path) -> Result<()> {
    let resolved = resolve_identity_path(path)?;
    let mut ancestor = resolved.parent();
    while let Some(dir) = ancestor {
        let marker = dir.join(LOCK_MARKER);
        match std::fs::symlink_metadata(&marker) {
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

/// Whether two paths name the same directory.
///
/// Resolves existing symlink aliases and the nearest existing ancestor of an
/// absent path, so a missing database path still compares by its eventual
/// location. Resolution failures propagate; they are never read as "different".
pub(crate) fn is_same_dir(left: &Path, right: &Path) -> Result<bool> {
    Ok(resolve_identity_path(left)? == resolve_identity_path(right)?)
}
