//! Identity-safe WAL-first reset staging, rollback and cleanup.
//!
//! Plan admission and the locking order live here; the [`transaction`]
//! module owns what happens once both identities are locked — staging each
//! move behind an fsync barrier, rolling back deterministically, and never
//! rolling back an unproven namespace state.
#[cfg(windows)]
use super::identity::direct_wal;
use super::identity::{
    check_locked_role, is_same_dir, lstat_exists, overlap, preflight_role, resolve_identity_path,
    scan_nested_databases, validate_reset_overlap, write_locked_role, DirectoryRole, Overlap,
    RoleCheck, WalExemption,
};
use super::identity::{reject_reset_alias, reset_request_path, verify_reset_identity};
use super::LOCK_MARKER;
use crate::vfs::NativeVfs;
use cypherlite_core::file_identity::{self, FileIdentity};
use cypherlite_core::{Error, Result};
use std::path::{Path, PathBuf};

mod transaction;

/// The test seam for the exclusive-rename primitive (the swap-refusal
/// tests call it directly) — cfg(test)-gated, so the primitive's own
/// visibility stays module-internal. The primitive itself is host
/// plumbing in the VFS layer (`vfs::host::host_rename_noreplace`);
/// the never-replace policy that calls it stays in the transaction.
#[cfg(test)]
pub(super) fn rename_reset_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    crate::vfs::host::host_rename_noreplace(from, to)
}
use transaction::ResetTransaction;
pub(crate) use transaction::{MoveKind, ResetStage};
// The tombstone-name mark's only consumer besides `reset_tombstone`
// itself is the tests' no-residue sweep.
#[cfg(test)]
pub(crate) use transaction::RESET_TOMBSTONE_MARK;

// `staged` is also the test seam for pausing between namespace moves and
// injecting a pre-deletion failure. It runs with both old identities locked.
pub(crate) fn reset_native_with(
    path: &Path,
    wal_override: Option<&Path>,
    staged: impl FnMut(MoveKind) -> Result<()>,
) -> Result<()> {
    reset_native_with_barrier(path, wal_override, staged, |_, _| Ok(()))
}

// The barrier seam simulates an fsync failure immediately before each real
// sync. A failed barrier is conservatively treated like an indeterminate sync:
// no rollback or deletion is safe without a proven namespace state.
pub(super) fn reset_native_with_barrier(
    path: &Path,
    wal_override: Option<&Path>,
    mut staged: impl FnMut(MoveKind) -> Result<()>,
    mut barrier: impl FnMut(ResetStage, &Path) -> Result<()>,
) -> Result<()> {
    let plan = ResetPlan::preflight(path, wal_override)?;
    let mut reset = ResetTransaction::new(plan, &mut barrier)?;
    let result = reset.stage(&mut staged, &mut barrier);
    match result {
        Ok(()) => reset.cleanup(),
        Err(failure) => Err(reset.rollback(failure)),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ResetMoves {
    None,
    Root,
    Wal,
    WalThenRoot,
}

struct LockedResetDir {
    path: PathBuf,
    /// The locked dir's parent, derived once here: a `lock_directory` path
    /// is canonical, so it always has one.
    parent: PathBuf,
    // `None` only after `release_lease` on Windows (see below); Unix keeps
    // the lease held across the rename as the atomic contention guard.
    lease: Option<std::fs::File>,
    identity: FileIdentity,
}

impl LockedResetDir {
    fn from_target(target: LockedTarget) -> Result<Self> {
        // The same typed parentless policy the tombstone builder
        // applies: only a filesystem root has no parent, and it is
        // never a resettable target.
        let parent = target
            .path
            .parent()
            .ok_or_else(|| Error::storage("cannot reset a filesystem root"))?
            .to_path_buf();
        Ok(Self {
            identity: file_identity::of_path(&target.path)?,
            path: target.path,
            parent,
            lease: Some(target.file),
        })
    }

    /// Release the OS lock and close the handle before renaming the tree.
    ///
    /// Windows refuses to rename a directory while any handle inside its
    /// tree is open (ERROR_ACCESS_DENIED), so the lease cannot outlive the
    /// final identity check there. The rename itself remains the fail-closed
    /// handoff: a concurrent opener holding a handle inside the tree makes
    /// the rename fail instead of moving a live database.
    #[cfg(windows)]
    fn release_lease(&mut self) {
        if let Some(lease) = self.lease.take() {
            let _ = fs4::FileExt::unlock(&lease);
        }
    }
}

impl Drop for LockedResetDir {
    fn drop(&mut self) {
        // Finish the ownership handoff explicitly: on macOS, closing the
        // descriptor alone can delay an immediate reopen reacquiring the
        // lease, which a reset followed by an immediate reopen would surface
        // as a spurious `Locked`. Mirrors NativeLease/PreflightLease.
        if let Some(lease) = &self.lease {
            let _ = fs4::FileExt::unlock(lease);
        }
    }
}

/// One locked reset target under its exclusive lease, carrying the role
/// decision its under-lease check made — never a separately-tracked bool
/// to keep consistent with an unwrap.
struct LockedTarget {
    path: PathBuf,
    file: std::fs::File,
    new_role: bool,
}

/// The resolved reset inputs: both identities, their existence, and the
/// direct-WAL layout fact, before any marker can be created.
struct ResetTargets {
    requested: PathBuf,
    wal_input: PathBuf,
    root_identity: PathBuf,
    wal_identity: PathBuf,
    root_exists: bool,
    wal_exists: bool,
}

impl ResetTargets {
    fn resolve(path: &Path, wal_override: Option<&Path>) -> Result<Self> {
        reject_reset_alias(path)?;
        let wal_input = wal_override
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.join(crate::codec::SnapshotCodecKind::Json.wal_dir()));
        // Validate before lock_directory can create even the marker. In
        // particular, locking an ancestor WAL is not ownership of its siblings.
        let root_identity = resolve_identity_path(path)?;
        let wal_identity = resolve_identity_path(&wal_input)?;
        validate_reset_overlap(&root_identity, &wal_identity)?;
        // Do not create anything on a reset of an absent path. Lock the same
        // identities as open before moving either root or overridden WAL.
        let root_exists = lstat_exists(path)?;
        let wal_exists = lstat_exists(&wal_input)?;
        // Windows has no durable directory rename barrier, so an external WAL
        // reset fails closed there. Refuse before preflight reads or locks
        // either marker: a live owner's marker lock would otherwise surface
        // this refusal as raw lock contention instead of the typed Storage
        // error, and no marker may be created for a reset that cannot run.
        #[cfg(windows)]
        let direct = direct_wal(&root_identity, &wal_identity)?;
        #[cfg(windows)]
        if wal_exists && !direct {
            return Err(Error::storage(
            "external WAL reset requires a durable directory rename barrier unavailable on Windows",
        ));
        }
        Ok(Self {
            requested: path.to_path_buf(),
            wal_input,
            root_identity,
            wal_identity,
            root_exists,
            wal_exists,
        })
    }
}

/// A reset must not manufacture a role for an ambiguous old directory, nor
/// delete a tree containing another owner: validate both identities and every
/// recursively deleted tree before lock_directory can create either marker.
fn preflight_target_roles(targets: &ResetTargets) -> Result<()> {
    if targets.root_exists {
        preflight_role(&targets.root_identity, RoleCheck::reset_ordinary_root())?;
    }
    if targets.wal_exists && targets.wal_identity != targets.root_identity {
        preflight_role(
            &targets.wal_identity,
            RoleCheck::Wal {
                newly_created: false,
            },
        )?;
    }
    // Refuse legacy nested owners before lock_directory could create a new
    // root marker. Repeat under the root lease below to cover new entries.
    if targets.root_exists {
        scan_nested_databases(
            &targets.root_identity,
            WalExemption::VerifyByPreflight(&targets.wal_identity),
        )?;
    }
    // A WAL the root does not contain is also recursively deleted. Check
    // before creating even a missing root marker or WAL marker, then repeat
    // under both leases; a root-contained WAL is covered by the root's own
    // scan above.
    if targets.wal_exists
        && overlap(&targets.root_identity, &targets.wal_identity)? != Overlap::RootContainsWal
    {
        scan_nested_databases(&targets.wal_identity, WalExemption::Unset)?;
    }
    reject_reset_alias(&targets.requested)
}

struct ResetPlan {
    requested_root: PathBuf,
    root: Option<LockedResetDir>,
    wal: Option<LockedResetDir>,
    moves: ResetMoves,
}

impl ResetPlan {
    // No marker, role, or tombstone may be written before both identities and
    // every recursively deleted tree have passed the ownership checks.
    fn preflight(path: &Path, wal_override: Option<&Path>) -> Result<Self> {
        let targets = ResetTargets::resolve(path, wal_override)?;
        preflight_target_roles(&targets)?;
        let locked = lock_reset_targets(&targets)?;
        let moves = plan_reset_moves(
            locked.root.as_ref().map(|dir| dir.path.as_path()),
            locked.wal.as_ref().map(|dir| dir.path.as_path()),
            wal_override,
        )?;
        Ok(Self {
            requested_root: targets.requested,
            root: locked.root,
            wal: locked.wal,
            moves,
        })
    }

    /// Re-verify the requested root against the locked plan — the
    /// double-in-time check both lifecycle entry points run: a locked root
    /// must still be the requested tree, an unlocked plan must still find
    /// its root absent.
    fn recheck_requested_root(&self) -> Result<()> {
        if let Some(root) = &self.root {
            transaction::verify_locked_reset_dir(root, &self.requested_root)
        } else {
            verify_reset_absent_root(&self.requested_root)
        }
    }

    /// The locked directory a move of `kind` stages, if the plan's
    /// sequence includes that move — the one derivation of the correlation
    /// `plan_reset_moves` established between moves and locked dirs: a WAL
    /// move plan always locked a WAL, a root move plan always locked a root.
    fn move_target(&mut self, kind: MoveKind) -> Option<&mut LockedResetDir> {
        match (kind, self.moves) {
            (MoveKind::Wal, ResetMoves::Wal | ResetMoves::WalThenRoot) => {
                Some(self.wal.as_mut().expect("a WAL move plan locked its WAL"))
            }
            (MoveKind::Root, ResetMoves::Root | ResetMoves::WalThenRoot) => Some(
                self.root
                    .as_mut()
                    .expect("a root move plan locked its root"),
            ),
            _ => None,
        }
    }

    /// The locked root of the two-move plan — the only plan the
    /// reset-incomplete marker exists for. Guard and lookup are fused
    /// into this one accessor: `plan_reset_moves` derives `WalThenRoot`
    /// only for a plan that locked both directories, so this is the one
    /// place that correlation lives, with its invariant stated once
    /// instead of re-proved by expect at every use site.
    fn two_move_root(&mut self) -> Option<&mut LockedResetDir> {
        match self.moves {
            ResetMoves::WalThenRoot => Some(
                self.root
                    .as_mut()
                    .expect("a WalThenRoot plan locked its root"),
            ),
            _ => None,
        }
    }

    /// Windows only: release the lease of a WAL tree contained in a root
    /// this plan still holds — that tree moves with the root rename, and an
    /// open handle inside it would pin the rename (ERROR_ACCESS_DENIED).
    #[cfg(windows)]
    fn release_contained_wal_lease(&mut self) -> Result<()> {
        if let (Some(root), Some(wal)) = (&self.root, &mut self.wal) {
            if overlap(&root.path, &wal.path)? == Overlap::RootContainsWal {
                wal.release_lease();
            }
        }
        Ok(())
    }
}

/// Lock both identities in open-path order, re-verify each under its lease,
/// re-run every nested-owner scan under the leases, and write the roles of
/// any newly established markers.
/// The locked root and WAL directories, by name — never a positional
/// pair whose slots a consumer could swap.
struct LockedTargets {
    root: Option<LockedResetDir>,
    wal: Option<LockedResetDir>,
}

fn lock_reset_targets(targets: &ResetTargets) -> Result<LockedTargets> {
    let root = lock_and_check_root(targets)?;
    let wal = lock_and_check_wal(targets, root.as_ref())?;
    rescan_under_leases(targets, root.as_ref(), wal.as_ref())?;
    reverify_under_leases(targets, root.as_ref())?;
    write_new_roles(root.as_ref(), wal.as_ref())?;
    Ok(LockedTargets {
        root: root.map(LockedResetDir::from_target).transpose()?,
        wal: wal.map(LockedResetDir::from_target).transpose()?,
    })
}

/// Phase 1-4 for the root: take the exclusive lease (creating the marker
/// only for an existing root), verify the requested identity under it,
/// rescan nested owners under it, and check its role.
fn lock_and_check_root(targets: &ResetTargets) -> Result<Option<LockedTarget>> {
    if !targets.root_exists {
        return Ok(None);
    }
    let (path, file) = NativeVfs::lock_directory(&targets.root_identity, LOCK_MARKER)?;
    verify_reset_identity(&targets.requested, &path, file_identity::of_path(&path)?)?;
    NativeVfs::check_identity(&file, &path.join(LOCK_MARKER))?;
    // Recheck under the root lease before acquiring or creating a WAL marker.
    scan_nested_databases(
        &path,
        WalExemption::VerifyByPreflight(&targets.wal_identity),
    )?;
    let new_role = check_locked_role(&path, &file, RoleCheck::reset_ordinary_root())?;
    Ok(Some(LockedTarget {
        path,
        file,
        new_role,
    }))
}

/// Phase 5-7 for the WAL: a WAL aliasing the locked root moves with it and
/// needs no lease of its own; a distinct WAL is locked and role-checked
/// under its own lease.
fn lock_and_check_wal(
    targets: &ResetTargets,
    root: Option<&LockedTarget>,
) -> Result<Option<LockedTarget>> {
    let wal_is_root = match root {
        Some(root) => is_same_dir(&targets.wal_input, &root.path)?,
        None => false,
    };
    if !targets.wal_exists || wal_is_root {
        return Ok(None);
    }
    let (path, file) = NativeVfs::lock_directory(&targets.wal_identity, LOCK_MARKER)?;
    let new_role = check_locked_role(
        &path,
        &file,
        RoleCheck::Wal {
            newly_created: false,
        },
    )?;
    Ok(Some(LockedTarget {
        path,
        file,
        new_role,
    }))
}

/// Phase 8-10: with both leases held, revalidate the overlap and re-run
/// every nested-owner scan. The locked WAL was verified under its lease, so
/// the root scan exempts its marker without re-deriving the layout fact.
fn rescan_under_leases(
    targets: &ResetTargets,
    root: Option<&LockedTarget>,
    wal: Option<&LockedTarget>,
) -> Result<()> {
    let root_path = root.map(|dir| dir.path.as_path());
    let wal_path = wal.map(|dir| dir.path.as_path());
    // A containing directory takes its nested identity with it. For disjoint
    // paths, stage WAL first while the original root is still locked. Once
    // the root moves, no new owner can ever attach the old WAL. The layout
    // the refusal just validated is the same fact the WAL-scan containment
    // gate below consults — computed once, shared by both.
    let layout = validate_reset_overlap(
        root_path.unwrap_or(&targets.root_identity),
        wal_path.unwrap_or(&targets.wal_identity),
    )?;
    if let Some(root) = root_path {
        let exemption = match wal_path {
            Some(wal) => WalExemption::Owned(wal),
            None => WalExemption::VerifyByPreflight(&targets.wal_identity),
        };
        scan_nested_databases(root, exemption)?;
    }
    if let Some(wal) = wal_path {
        if layout != Overlap::RootContainsWal {
            scan_nested_databases(wal, WalExemption::Unset)?;
        }
    }
    Ok(())
}

/// Phase 11: recheck after acquiring both leases, before writing roles or
/// markers — the requested root must still be the locked tree, or still be
/// absent for an unlocked plan.
fn reverify_under_leases(targets: &ResetTargets, root: Option<&LockedTarget>) -> Result<()> {
    match root {
        Some(root) => {
            verify_reset_identity(
                &targets.requested,
                &root.path,
                file_identity::of_path(&root.path)?,
            )?;
            NativeVfs::check_identity(&root.file, &root.path.join(LOCK_MARKER))
        }
        None => verify_reset_absent_root(&targets.requested),
    }
}

/// Phase 12: write the role of every marker whose under-lease check
/// established a new one — bound to the locked dir it decided about.
///
/// Root before WAL, the reverse of `adopt_default_wal_at`'s order, for a
/// stated reason rather than drift: adoption writes a typed direct WAL
/// first so a crash between the writes leaves a markerless root that is
/// still recognizable beside it. A reset publishes nothing to recognize
/// later — both trees are locked and about to be tombstoned, and a
/// crash between these two writes leaves them in place and half-typed,
/// a pair every ordinary opener refuses fail-closed (ambiguous legacy
/// storage, explicit offline migration), never a wrong adoption. Once
/// staging begins, the durable reset-incomplete marker blocks the old
/// root through the whole move window.
fn write_new_roles(root: Option<&LockedTarget>, wal: Option<&LockedTarget>) -> Result<()> {
    if let Some(root) = root.filter(|root| root.new_role) {
        write_locked_role(&root.path, &root.file, DirectoryRole::Root)?;
    }
    if let Some(wal) = wal.filter(|wal| wal.new_role) {
        write_locked_role(&wal.path, &wal.file, DirectoryRole::Wal)?;
    }
    Ok(())
}

/// Which namespace moves the staged transaction will perform.
fn plan_reset_moves(
    root: Option<&Path>,
    wal: Option<&Path>,
    wal_override: Option<&Path>,
) -> Result<ResetMoves> {
    Ok(match (root, wal) {
        (Some(root), Some(wal)) => match overlap(root, wal)? {
            Overlap::WalContainsRoot => ResetMoves::Wal,
            Overlap::RootContainsWal => ResetMoves::Root,
            Overlap::Disjoint => ResetMoves::WalThenRoot,
        },
        (Some(_), None) => ResetMoves::Root,
        (None, Some(_)) if wal_override.is_some() => ResetMoves::Wal,
        _ => ResetMoves::None,
    })
}

fn verify_reset_absent_root(requested: &Path) -> Result<()> {
    reject_reset_alias(requested)?;
    let requested = reset_request_path(requested);
    match std::fs::symlink_metadata(&requested) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(Error::storage(format!(
            "reset database URI appeared during reset: {}",
            requested.display()
        ))),
        Err(e) => Err(e.into()),
    }
}
