//! Role admission: the [`RoleCheck`] inputs, the lease-less preflight, the
//! under-lease check, and the role-marker writes.
use std::fs::File;
use std::io::Write;
use std::path::Path;

use crate::vfs::NativeVfs;
use cypherlite_core::{Error, Result};

use super::super::LOCK_MARKER;
use super::evidence::{
    empty_except_lock, lock_and_read_marker, root_evidence, root_specific_evidence, wal_evidence,
};
use super::{DirectoryRole, RootShape};

/// The one spelling of the foreign-marker refusal — marker bytes that
/// match neither the role's marker nor the empty (legacy) marker —
/// shared by every admission site in this module; each keeps only its
/// own policy layered on the verdict.
fn wrong_role(marker: &Path) -> Error {
    Error::storage(format!(
        "wrong or malformed lock role at {}",
        marker.display()
    ))
}

/// Admission inputs for role preflight and locked-role checks: one
/// role-tagged fact, so a check can never travel beside a role it
/// disagrees with, and no WAL check can carry a meaningless root shape.
///
/// Named fields keep call sites readable; positional trailing bools made
/// the independent switches easy to transpose. `newly_created` is consulted
/// only under a held lease ([`check_locked_role`]); a lease-less preflight
/// always sees a pre-existing marker.
#[derive(Clone, Copy)]
pub(crate) enum RoleCheck {
    /// Root-directory admission. `root_shape` distinguishes a markdown
    /// vault root (ADR-0406), which adopts with vault content present,
    /// from an ordinary root, which reads that content as legacy-store
    /// ambiguity.
    Root {
        /// Reset is the explicit migration barrier that admits legacy evidence.
        reset: bool,
        /// The WAL is the physical direct child of this root.
        direct_wal: bool,
        /// The marker was created by this open, so a changed identity is refused.
        newly_created: bool,
        root_shape: RootShape,
    },
    /// WAL-directory admission. No root-shape concept applies to a WAL,
    /// so none exists here; a WAL's marker bytes and its containing
    /// directory's evidence decide everything else.
    Wal {
        /// The marker was created by this open, so a changed identity is refused.
        newly_created: bool,
    },
}

impl RoleCheck {
    /// The role this check admits — derived from the variant, never a
    /// second argument that can disagree with it.
    pub(super) fn role(self) -> DirectoryRole {
        match self {
            Self::Root { .. } => DirectoryRole::Root,
            Self::Wal { .. } => DirectoryRole::Wal,
        }
    }

    fn newly_created(self) -> bool {
        match self {
            Self::Root { newly_created, .. } | Self::Wal { newly_created } => newly_created,
        }
    }

    /// The reset-mode ordinary-root policy (ADR-0406-adjacent reset
    /// preflight): the one named spelling of the repeated admission shape,
    /// so each call site says what it means.
    pub(crate) fn reset_ordinary_root() -> Self {
        Self::Root {
            reset: true,
            direct_wal: false,
            newly_created: false,
            root_shape: RootShape::Ordinary,
        }
    }
}

/// The operator has verified the *ownership* of this direct WAL offline.
/// Layout alone cannot distinguish it from a nested legacy root == WAL.
/// This only checks eligibility; ordinary open/reset never call it.
pub(crate) fn preflight_manual_wal(path: &Path, locked: Option<&File>) -> Result<bool> {
    if root_specific_evidence(path)? || !wal_evidence(path)? {
        return Err(Error::storage(format!(
            "offline WAL adoption requires segment magic and no root evidence at {}",
            path.display()
        )));
    }
    let bytes = lock_and_read_marker(path, locked)?;
    if bytes.is_empty() {
        Ok(true)
    } else if bytes == super::WAL_ROLE {
        Ok(false)
    } else {
        Err(wrong_role(&path.join(LOCK_MARKER)))
    }
}

/// Inspect without creating a marker. Returns whether a typed role marker
/// was verified. Reset requires positive legacy evidence; ordinary open
/// may adopt only an otherwise empty, previously unused root.
pub(crate) fn preflight_role(path: &Path, check: RoleCheck) -> Result<bool> {
    preflight_role_with_lease(path, check, None)
}

fn preflight_role_with_lease(path: &Path, check: RoleCheck, locked: Option<&File>) -> Result<bool> {
    let role = check.role();
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => (),
        Ok(_) => {
            return Err(Error::storage(format!(
                "{} is not a directory",
                path.display()
            )))
        }
        // An absent directory has no marker to read and admits nothing.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    }
    let marker = path.join(LOCK_MARKER);
    let bytes = lock_and_read_marker(path, locked)?;
    if !bytes.is_empty() {
        if bytes != role.marker() || (role == DirectoryRole::Wal && root_evidence(path)?) {
            return Err(wrong_role(&marker));
        }
        return Ok(true);
    }
    // Preflight existing roles before creating either marker. The final
    // lease holder checks the marker again against concurrent changes.
    if let RoleCheck::Root {
        reset,
        direct_wal,
        root_shape,
        ..
    } = check
    {
        let has_evidence = if direct_wal {
            root_specific_evidence(path)?
        } else {
            root_evidence(path)?
        };
        // A same-directory segment cannot prove root identity, even
        // though it proves this directory contains WAL-format bytes.
        // A vault root adopts with content present: the content is the
        // vault, not a legacy store's residue (ADR-0406).
        let adoptable = empty_except_lock(path)? || root_shape == RootShape::Vault;
        if has_evidence || (!reset && adoptable && !wal_evidence(path)?) {
            return Ok(false);
        }
    }
    Err(Error::storage(format!(
        "ambiguous legacy storage role at {}; explicit offline migration required",
        path.display()
    )))
}

/// Read-only admission. Does not write the marker.
///
/// A root must be [`ROOT_ROLE`](super::ROOT_ROLE) or an empty legacy marker on a directory
/// that already has root evidence. A WAL must be
/// [`WAL_ROLE`](super::WAL_ROLE) or empty.
pub(crate) fn accept_readonly_marker(path: &Path, file: &File, role: DirectoryRole) -> Result<()> {
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
    Err(wrong_role(&marker))
}

pub(crate) fn check_locked_role(path: &Path, file: &File, check: RoleCheck) -> Result<bool> {
    let role = check.role();
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
        return Err(wrong_role(&marker));
    }
    if check.newly_created() {
        if role == DirectoryRole::Wal && !empty_except_lock(path)? {
            return Err(Error::storage(format!(
                "new storage identity changed at {}",
                path.display()
            )));
        }
    } else {
        preflight_role_with_lease(path, check, Some(file))?;
    }
    Ok(true)
}

/// Role write for a marker this process holds `LOCK_SH` (ADR-0136).
pub(crate) fn write_shared_lease_role(path: &Path, file: &File, role: DirectoryRole) -> Result<()> {
    NativeVfs::write_under_shared_lock(file, &path.join(LOCK_MARKER), role.marker())?;
    sync_marker_parent(path)
}

pub(crate) fn write_locked_role(path: &Path, file: &File, role: DirectoryRole) -> Result<()> {
    (&*file).write_all(role.marker())?;
    file.sync_all()?;
    // On Windows external-WAL reset is refused before we reach this point.
    sync_marker_parent(path)
}

/// Persist a newly created marker directory entry. Unix fsyncs the parent
/// directory; other platforms have no directory fsync.
pub(crate) fn sync_marker_parent(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
