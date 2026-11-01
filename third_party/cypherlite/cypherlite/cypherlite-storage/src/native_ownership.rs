//! Native root/WAL lease and offline adoption (ADR-0097/0088).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod identity;
mod reset;

use crate::vfs::{NativeLease, NativeVfs};
use cypherlite_core::file_identity;
use cypherlite_core::{Error, Result};
use identity::{
    accept_readonly_marker, check_locked_role, direct_wal, empty_except_lock, is_same_dir,
    markdown_root_shape, preflight_manual_wal, preflight_role, resolve_identity_path,
    scan_nested_databases, write_locked_role, write_shared_lease_role, DirectoryRole, RoleCheck,
    RootShape, WalExemption,
};
pub(crate) use identity::{reject_nested, reject_reset_alias, verify_reset_identity};
pub use identity::{ROOT_ROLE, WAL_ROLE};
pub(crate) use reset::reset_native_with;

pub(crate) const RESET_INCOMPLETE: &str = ".reset-incomplete";

/// CypherLite-unique per-database ownership marker filename (ADR-0097).
///
/// Deliberately not the generic `.lock`: unrelated `.lock` files created by
/// other tools (for example pytest's `pytest-0/.lock` in a parent directory)
/// must never be interpreted as a CypherLite database identity or ancestor.
pub const LOCK_MARKER: &str = ".cypherlite.lock";

/// Exclusive writer lease beside a shared identity marker (ADR-0136).
///
/// Not a database identity: nested scans still key on [`LOCK_MARKER`] and
/// `.cl` bundles. Never unlinked on close.
pub const WRITER_LEASE: &str = ".cypherlite.writer";

pub(crate) fn lease(root: &Path, wal: &Path) -> Result<(PathBuf, PathBuf, Arc<NativeLease>)> {
    lease_with(root, wal, || {})
}

/// [`lease`] whose root directory must be created by this call (ADR-0161).
///
/// The final root component is created exclusively first: a pre-existing
/// (or raced-in, or symlinked) entry fails with a destination-already-exists
/// storage error instead of being adopted, so a publish destination is never
/// someone else's directory. The WAL beneath a freshly created root is
/// necessarily new as well. Ordinary opens keep using [`lease`], which
/// adopts an existing store.
pub(crate) fn lease_creating(
    root: &Path,
    wal: &Path,
) -> Result<(PathBuf, PathBuf, Arc<NativeLease>)> {
    reject_nested(root)?;
    NativeVfs::create_private_dir_exclusive(root)?;
    lease_with(root, wal, || {})
}

pub(crate) fn adopt_default_wal(root: &Path) -> Result<()> {
    reject_nested(root)?;
    let root = resolve_identity_path(root)?;
    adopt_default_wal_at(&root)
}

/// The pre-lock half of the open-path admission (the seam the reset
/// path's phase functions mirror): resolved identities, the lexical
/// facts the role checks consume (WAL-aliases-root, the markdown root
/// shape, root-was-empty), both roles' preflight, and the first
/// nested-owner scan — everything the lease phase below re-checks
/// under lock.
struct PreflightedOpen {
    same: bool,
    root_shape: RootShape,
    root_was_empty: bool,
}

fn preflight_open_roles(root: &Path, wal: &Path) -> Result<PreflightedOpen> {
    reject_nested(root)?;
    let resolved_root = resolve_identity_path(root)?;
    let resolved_wal = resolve_identity_path(wal)?;
    let root_was_empty = !resolved_root.exists() || empty_except_lock(&resolved_root)?;
    let same = is_same_dir(&resolved_root, &resolved_wal)?;
    let root_shape = markdown_root_shape(&resolved_root, &resolved_wal);
    preflight_role(
        &resolved_root,
        RoleCheck::Root {
            reset: false,
            direct_wal: same,
            newly_created: false,
            root_shape,
        },
    )?;
    if !same {
        preflight_role(
            &resolved_wal,
            RoleCheck::Wal {
                newly_created: false,
            },
        )?;
    }
    match fs::metadata(&resolved_root) {
        Ok(meta) if meta.is_dir() => {
            scan_nested_databases(
                &resolved_root,
                WalExemption::VerifyByPreflight(&resolved_wal),
            )?;
        }
        Ok(_) => (), // lock_directory reports the invalid root type
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(PreflightedOpen {
        same,
        root_shape,
        root_was_empty,
    })
}

/// The post-root-lease WAL inspection: every existing WAL tree is
/// scanned before its lock marker is created. Case-folded overlap is
/// conservative on macOS/Windows: a WAL whose spelling overlaps the
/// root may actually be a separate directory.
fn inspect_wal_trees(wal: &Path, root: &Path) -> Result<()> {
    let resolved_wal = resolve_identity_path(wal)?;
    match fs::metadata(&resolved_wal) {
        Ok(meta) if meta.is_dir() => {
            // When the WAL aliases the root, its own default
            // wal/.cypherlite.lock belongs to this root. Other WAL scans
            // exempt only their own marker; the first-root scan above handles
            // a markerless root with an ambiguous pre-existing direct
            // wal/.cypherlite.lock.
            let same_root = is_same_dir(&resolved_wal, root)?;
            scan_nested_databases(
                &resolved_wal,
                if same_root {
                    WalExemption::VerifyByPreflight(&resolved_wal)
                } else {
                    WalExemption::Unset
                },
            )?;
        }
        Ok(_) => (), // lock_directory reports the invalid WAL type
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

// The callback exercises a raced-in WAL between the last inspection and
// the exclusive final-component directory creation in tests.
fn lease_with(
    root: &Path,
    wal: &Path,
    before_wal_create: impl FnOnce(),
) -> Result<(PathBuf, PathBuf, Arc<NativeLease>)> {
    let preflighted = preflight_open_roles(root, wal)?;
    let (root, root_lock) = NativeVfs::lock_directory_shared(root, LOCK_MARKER)?;
    let mut held = LeaseBuild::from_root(root_lock);
    reject_nested(&root)?;
    NativeVfs::check_identity(held.root_file(), &root.join(LOCK_MARKER))?;
    reject_incomplete_reset(&root)?;
    // Check the root before the distinct WAL lease: legacy root evidence
    // may contain a typed direct WAL marker that must be leased during
    // inspection, and cannot be re-locked by this owner later.
    let root_new_role = check_locked_role(
        &root,
        held.root_file(),
        RoleCheck::Root {
            reset: false,
            direct_wal: preflighted.same,
            newly_created: preflighted.root_was_empty,
            root_shape: preflighted.root_shape,
        },
    )?;
    inspect_wal_trees(wal, &root)?;
    before_wal_create();
    let created_wal = NativeVfs::create_private_dir(wal)?;
    let wal = fs::canonicalize(wal)?;
    // A directory that appeared after our last inspection is not ours.
    // Validate its existing role before lock_directory can create the marker;
    // an empty raced-in directory remains markerless and ambiguous.
    let wal_is_root = is_same_dir(&wal, &root)?;
    if !created_wal && !wal_is_root {
        preflight_role(
            &wal,
            RoleCheck::Wal {
                newly_created: false,
            },
        )?;
    }
    let (wal, wal_lock) = if wal_is_root {
        (root.clone(), None)
    } else {
        let (path, file) = NativeVfs::lock_directory_shared(&wal, LOCK_MARKER)?;
        (path, Some(file))
    };
    held.wal = wal_lock;
    let wal_new_role = if let Some(file) = held.wal_file() {
        NativeVfs::check_identity(file, &wal.join(LOCK_MARKER))?;
        check_locked_role(
            &wal,
            file,
            RoleCheck::Wal {
                newly_created: created_wal,
            },
        )?
    } else {
        false
    };
    // A newly established parent must not open over an already established
    // child. Once its marker exists, concurrent child openers recheck it.
    // A WAL created by this open, or already typed under its lease, is this
    // root's own: the nested scan exempts its marker without re-verification.
    scan_nested_databases(
        &root,
        if created_wal || (held.wal.is_some() && !wal_new_role) {
            WalExemption::Owned(&wal)
        } else {
            WalExemption::VerifyByPreflight(&wal)
        },
    )?;
    // A child can appear between the pre-lock scan and lock acquisition.
    // Recheck under the lease before accessing database contents. If this
    // race is detected after creating a new marker, leave the stable lock
    // inode in place: unlinking it can admit two concurrent WAL owners.
    // `wal_is_root` is the one derivation of the WAL-aliases-root fact:
    // between it and here `wal` is only rebound to `root.clone()` or to
    // `lock_directory_shared`'s canonicalization of the same path, so it
    // still names the same directory.
    scan_nested_databases(
        &wal,
        if wal_is_root {
            WalExemption::VerifyByPreflight(&wal)
        } else {
            WalExemption::Unset
        },
    )?;
    reject_nested(&root)?;
    NativeVfs::check_identity(held.root_file(), &root.join(LOCK_MARKER))?;
    // A recognizable legacy store can open without rewriting its lock
    // inode. Reset is the explicit migration barrier for those markers;
    // only newly adopted ordinary-open identities receive new roles.
    if root_new_role && preflighted.root_was_empty {
        write_shared_lease_role(&root, held.root_file(), DirectoryRole::Root)?;
    }
    if wal_new_role && created_wal {
        write_shared_lease_role(&wal, held.wal_file().unwrap(), DirectoryRole::Wal)?;
    }
    held.writer = Some(take_writer_lease(&root, &wal)?);
    Ok((root, wal, Arc::new(held.into_lease())))
}

/// Files acquired on the way to a [`NativeLease`]. Drop unlocks anything
/// still held, in writer-then-identity order, so an early `?` cannot skip
/// the explicit unlock.
struct LeaseBuild {
    root: Option<std::fs::File>,
    wal: Option<std::fs::File>,
    writer: Option<WriterLease>,
}

impl LeaseBuild {
    fn from_root(root: std::fs::File) -> Self {
        Self {
            root: Some(root),
            wal: None,
            writer: None,
        }
    }

    fn root_file(&self) -> &std::fs::File {
        self.root.as_ref().expect("root lease")
    }

    fn wal_file(&self) -> Option<&std::fs::File> {
        self.wal.as_ref()
    }

    fn into_lease(mut self) -> NativeLease {
        NativeLease::new(
            self.root.take().expect("root lease"),
            self.wal.take(),
            self.writer.take(),
        )
    }
}

impl Drop for LeaseBuild {
    fn drop(&mut self) {
        use fs4::FileExt;
        // WriterLease's Drop unlocks `.cypherlite.writer` before the
        // identity markers below.
        drop(self.writer.take());
        for file in [self.wal.as_ref(), self.root.as_ref()]
            .into_iter()
            .flatten()
        {
            let _ = FileExt::unlock(file);
        }
    }
}

/// Shared identity locks for an existing native database (ADR-0136).
///
/// Creates nothing: a missing root, marker, or WAL marker is `Error::Storage`.
/// Does not take the writer lease, so a live writable opener can share the
/// directory.
pub(crate) fn lease_readonly(
    root: &Path,
    wal: &Path,
) -> Result<(PathBuf, PathBuf, Arc<NativeLease>)> {
    reject_nested(root)?;
    let root = resolve_identity_path(root)?;
    if !root.is_dir() || !root.join(LOCK_MARKER).is_file() {
        return Err(Error::storage(format!(
            "read-only open requires an existing database at {}",
            root.display()
        )));
    }
    reject_incomplete_reset(&root)?;
    // Resolve before the shared lock so a path error drops nothing.
    let wal = resolve_identity_path(wal)?;
    let same = is_same_dir(&root, &wal)?;
    let root_lock = NativeVfs::lock_existing_shared_readonly(&root.join(LOCK_MARKER))?;
    let mut held = LeaseBuild::from_root(root_lock);
    accept_readonly_marker(&root, held.root_file(), DirectoryRole::Root)?;
    if !same {
        if !wal.is_dir() || !wal.join(LOCK_MARKER).is_file() {
            return Err(Error::storage(format!(
                "read-only open requires an existing WAL at {}",
                wal.display()
            )));
        }
        let wal_lock = NativeVfs::lock_existing_shared_readonly(&wal.join(LOCK_MARKER))?;
        accept_readonly_marker(&wal, &wal_lock, DirectoryRole::Wal)?;
        held.wal = Some(wal_lock);
    }
    Ok((root, wal, Arc::new(held.into_lease())))
}

/// The exclusive writer lease, held independently of the shared identity
/// lock a native opener keeps for the life of the process (ADR-0136).
///
/// Built only by [`take_writer_lease`]: a [`crate::StoreTarget`] already
/// open as a WAL follower (ADR-0137) holds the shared marker but not this
/// file, so promoting it to the writable owner takes this lease
/// separately instead of reopening the store. Dropping it releases the
/// lock without touching the shared identity lock, which is what lets the
/// same process demote back to a follower.
pub struct WriterLease {
    root: std::fs::File,
    /// `None` when the WAL directory is the root (the common case).
    wal: Option<std::fs::File>,
}

impl Drop for WriterLease {
    fn drop(&mut self) {
        use fs4::FileExt;
        if let Some(wal) = &self.wal {
            let _ = FileExt::unlock(wal);
        }
        let _ = FileExt::unlock(&self.root);
    }
}

/// Take the exclusive `.cypherlite.writer` lease(s) beside an already-held
/// shared identity lock. `root` and `wal` are the canonical directories a
/// live [`crate::StoreTarget::filesystem_dir`] / [`crate::wal_dir`] report;
/// the caller is already a live opener of that store (its shared
/// `.cypherlite.lock` is held for the life of the process), so this does
/// not repeat the identity scan `lease`/`lease_readonly` run at open. A
/// lease already held by another writer (including another process) fails
/// with [`Error::Locked`], the same error a second writable open reports.
pub fn take_writer_lease(root: &Path, wal: &Path) -> Result<WriterLease> {
    let root_file = NativeVfs::lock_writer_lease(root, WRITER_LEASE)?;
    // The WAL-aliases-root fact through its one canonical derivation
    // (physical identity, like every sibling site): the caller reports
    // the canonical directories of a live opener, where aliasing and
    // lexical equality agree.
    let wal_file = if is_same_dir(root, wal)? {
        None
    } else {
        match NativeVfs::lock_writer_lease(wal, WRITER_LEASE) {
            Ok(file) => Some(file),
            Err(e) => {
                use fs4::FileExt;
                let _ = FileExt::unlock(&root_file);
                return Err(e);
            }
        }
    };
    Ok(WriterLease {
        root: root_file,
        wal: wal_file,
    })
}

fn adopt_default_wal_at(root: &Path) -> Result<()> {
    let wal = root.join(crate::codec::SnapshotCodecKind::Json.wal_dir());
    let root_meta = fs::metadata(root)?;
    if !root_meta.is_dir() || !direct_wal(root, &wal)? {
        return Err(Error::storage(format!(
            "offline adoption requires a physical direct WAL at {}",
            wal.display()
        )));
    }
    let root_identity = file_identity::of_path(root)?;
    let wal_identity = file_identity::of_path(&wal)?;
    preflight_role(root, RoleCheck::reset_ordinary_root())?;
    preflight_manual_wal(&wal, None)?;
    reject_incomplete_reset(root)?;
    scan_nested_databases(root, WalExemption::Owned(&wal))?;

    // Both identity inodes are stable. Creating an absent marker does not
    // assert its role: revalidate under both leases before writing bytes.
    let (locked_root, root_file) = NativeVfs::lock_directory(root, LOCK_MARKER)?;
    if root_identity != file_identity::of_path(&locked_root)? {
        return Err(Error::storage(
            "database root changed during offline adoption",
        ));
    }
    reject_nested(&locked_root)?;
    NativeVfs::check_identity(&root_file, &locked_root.join(LOCK_MARKER))?;
    reject_incomplete_reset(&locked_root)?;
    let root_new_role =
        check_locked_role(&locked_root, &root_file, RoleCheck::reset_ordinary_root())?;
    let (locked_wal, wal_file) = NativeVfs::lock_directory(&wal, LOCK_MARKER)?;
    if wal_identity != file_identity::of_path(&locked_wal)?
        || !direct_wal(&locked_root, &locked_wal)?
    {
        return Err(Error::storage("WAL changed during offline adoption"));
    }
    let wal_new_role = preflight_manual_wal(&locked_wal, Some(&wal_file))?;
    scan_nested_databases(&locked_root, WalExemption::Owned(&locked_wal))?;
    // Write WAL first: if the root sync fails, a typed direct WAL is still
    // recognizable by a markerless root. Neither data tree is moved.
    if wal_new_role {
        write_locked_role(&locked_wal, &wal_file, DirectoryRole::Wal)?;
    }
    if root_new_role {
        write_locked_role(&locked_root, &root_file, DirectoryRole::Root)?;
    }
    // Explicitly unlock before the handles close: on macOS a close-only release
    // can delay immediate reacquisition of the same inode by a later opener
    // (same discipline as NativeLease/PreflightLease).
    use fs4::FileExt;
    let _ = FileExt::unlock(&wal_file);
    let _ = FileExt::unlock(&root_file);
    Ok(())
}

fn reject_incomplete_reset(root: &Path) -> Result<()> {
    match fs::symlink_metadata(root.join(RESET_INCOMPLETE)) {
        Ok(_) => {
            return Err(Error::storage(
                "reset incomplete; recover the old WAL before opening",
            ))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
