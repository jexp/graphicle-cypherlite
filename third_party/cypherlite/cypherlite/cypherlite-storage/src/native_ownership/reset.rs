//! Identity-safe WAL-first reset staging, rollback and cleanup.
use super::identity::{
    check_locked_role, direct_wal, is_same_dir, preflight_role, reset_path_starts_with,
    resolve_identity_path, scan_nested_databases, scan_nested_databases_with_trusted_wal,
    sync_marker_parent, validate_reset_overlap, write_locked_role, DirectoryRole, RoleCheck,
};
use super::{LOCK_MARKER, RESET_INCOMPLETE};
use crate::vfs::NativeVfs;
use crate::{reject_reset_alias, verify_reset_identity};
use cypherlite_core::file_identity::{self, FileIdentity};
use cypherlite_core::{Error, Result};
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

// `staged` is also the test seam for pausing between namespace moves and
// injecting a pre-deletion failure. It runs with both old identities locked.
pub(crate) fn reset_native_with(
    path: &Path,
    wal_override: Option<&Path>,
    staged: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    reset_native_with_barrier(path, wal_override, staged, |_, _| Ok(()))
}

// The barrier seam simulates an fsync failure immediately before each real
// sync. A failed barrier is conservatively treated like an indeterminate sync:
// no rollback or deletion is safe without a proven namespace state.
pub(super) fn reset_native_with_barrier(
    path: &Path,
    wal_override: Option<&Path>,
    mut staged: impl FnMut(&str) -> Result<()>,
    mut barrier: impl FnMut(&str, &Path) -> Result<()>,
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
enum MoveKind {
    Wal,
    Root,
}

impl MoveKind {
    fn name(self) -> &'static str {
        match self {
            Self::Wal => "wal",
            Self::Root => "root",
        }
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
    // `None` only after `release_lease` on Windows (see below); Unix keeps
    // the lease held across the rename as the atomic contention guard.
    lease: Option<std::fs::File>,
    identity: FileIdentity,
}

impl LockedResetDir {
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
        reject_reset_alias(path)?;
        let wal_input = wal_override
            .map(Path::to_path_buf)
            .unwrap_or_else(|| path.join("wal"));
        // Validate before lock_directory can create even the marker. In
        // particular, locking an ancestor WAL is not ownership of its siblings.
        let root_identity = resolve_identity_path(path)?;
        let wal_identity = resolve_identity_path(&wal_input)?;
        validate_reset_overlap(&root_identity, &wal_identity)?;
        // Do not create anything on a reset of an absent path. Lock the same
        // identities as open before moving either root or overridden WAL.
        let root_exists = match std::fs::symlink_metadata(path) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        let wal_exists = match std::fs::symlink_metadata(&wal_input) {
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e.into()),
        };
        let direct = direct_wal(&root_identity, &wal_identity)?;
        // Windows has no durable directory rename barrier, so an external WAL
        // reset fails closed there. Refuse before preflight reads or locks
        // either marker: a live owner's marker lock would otherwise surface
        // this refusal as raw lock contention instead of the typed Storage
        // error, and no marker may be created for a reset that cannot run.
        #[cfg(windows)]
        if wal_exists && !direct {
            return Err(Error::storage(
            "external WAL reset requires a durable directory rename barrier unavailable on Windows",
        ));
        }
        // A reset must not manufacture a role for an ambiguous old directory.
        // Validate *both* identities before lock_directory creates either marker.
        if root_exists {
            preflight_role(
                &root_identity,
                DirectoryRole::Root,
                RoleCheck {
                    reset: true,
                    direct_wal: false,
                    newly_created: false,
                },
            )?;
        }
        if wal_exists && wal_identity != root_identity {
            preflight_role(
                &wal_identity,
                DirectoryRole::Wal,
                RoleCheck {
                    reset: true,
                    direct_wal: direct,
                    newly_created: false,
                },
            )?;
        }
        // Refuse legacy nested owners before lock_directory could create a new
        // root marker. Repeat under the root lease below to cover new entries.
        if root_exists {
            scan_nested_databases(&root_identity, Some(&wal_identity))?;
        }
        // A disjoint WAL is also recursively deleted. Check before creating even
        // a missing root marker or WAL marker, then repeat under both leases.
        if wal_exists && !reset_path_starts_with(&wal_identity, &root_identity)? {
            scan_nested_databases(&wal_identity, None)?;
        }
        reject_reset_alias(path)?;
        let root = if root_exists {
            Some(NativeVfs::lock_directory(&root_identity)?)
        } else {
            None
        };
        if let Some((root_path, file)) = &root {
            verify_reset_identity(path, root_path, file_identity::of_path(root_path)?)?;
            NativeVfs::check_identity(file, &root_path.join(LOCK_MARKER))?;
        }
        // Recheck under the root lease before acquiring or creating a WAL marker.
        if let Some((root, _)) = &root {
            scan_nested_databases(root, Some(&wal_identity))?;
        }
        let root_new_role = if let Some((root_path, file)) = &root {
            check_locked_role(
                root_path,
                file,
                DirectoryRole::Root,
                RoleCheck {
                    reset: true,
                    direct_wal: false,
                    newly_created: false,
                },
            )?
        } else {
            false
        };
        let wal_is_root = match &root {
            Some((root_path, _)) => is_same_dir(&wal_input, root_path)?,
            None => false,
        };
        let wal = if wal_exists {
            if wal_is_root {
                None
            } else {
                Some(NativeVfs::lock_directory(&wal_identity)?)
            }
        } else {
            None
        };
        let wal_new_role = if let Some((wal_path, file)) = &wal {
            check_locked_role(
                wal_path,
                file,
                DirectoryRole::Wal,
                RoleCheck {
                    reset: true,
                    direct_wal: direct_wal(
                        root.as_ref().map(|(p, _)| p).unwrap_or(&root_identity),
                        wal_path,
                    )?,
                    newly_created: false,
                },
            )?
        } else {
            false
        };
        // A containing directory takes its nested identity with it. For disjoint
        // paths, stage WAL first while the original root is still locked. Once
        // the root moves, no new owner can ever attach the old WAL.
        let root_path = root.as_ref().map(|(path, _)| path);
        let wal_path = wal.as_ref().map(|(path, _)| path);
        validate_reset_overlap(
            root_path.unwrap_or(&root_identity),
            wal_path.unwrap_or(&wal_identity),
        )?;
        if let Some(root) = root_path {
            scan_nested_databases_with_trusted_wal(
                root,
                Some(wal_path.unwrap_or(&wal_identity)),
                wal_path
                    .map(|wal| direct_wal(root, wal))
                    .transpose()?
                    .unwrap_or(false),
            )?;
        }
        if let Some(wal) = wal_path {
            if !reset_path_starts_with(wal, root_path.unwrap_or(&root_identity))? {
                scan_nested_databases(wal, None)?;
            }
        }
        // Recheck after acquiring both leases, before writing roles or markers.
        if let Some((root_path, file)) = &root {
            verify_reset_identity(path, root_path, file_identity::of_path(root_path)?)?;
            NativeVfs::check_identity(file, &root_path.join(LOCK_MARKER))?;
        } else {
            verify_reset_absent_root(path)?;
        }
        if root_new_role {
            let (path, file) = root.as_ref().unwrap();
            write_locked_role(path, file, DirectoryRole::Root)?;
        }
        if wal_new_role {
            let (path, file) = wal.as_ref().unwrap();
            write_locked_role(path, file, DirectoryRole::Wal)?;
        }
        let moves = match (root_path, wal_path) {
            (Some(root), Some(wal)) => {
                if reset_path_starts_with(root, wal)? {
                    ResetMoves::Wal
                } else if reset_path_starts_with(wal, root)? {
                    ResetMoves::Root
                } else {
                    ResetMoves::WalThenRoot
                }
            }
            (Some(_), None) => ResetMoves::Root,
            (None, Some(_)) if wal_override.is_some() => ResetMoves::Wal,
            _ => ResetMoves::None,
        };
        Ok(Self {
            requested_root: path.to_path_buf(),
            root: root
                .map(|(path, lease)| -> Result<LockedResetDir> {
                    let identity = file_identity::of_path(&path)?;
                    Ok(LockedResetDir {
                        path,
                        lease: Some(lease),
                        identity,
                    })
                })
                .transpose()?,
            wal: wal
                .map(|(path, lease)| -> Result<LockedResetDir> {
                    let identity = file_identity::of_path(&path)?;
                    Ok(LockedResetDir {
                        path,
                        lease: Some(lease),
                        identity,
                    })
                })
                .transpose()?,
            moves,
        })
    }
}

// A reserved container is not a published move. A published rename becomes
// durable only after BOTH directory entries have been synced. On a failed
// barrier, MovedNotDurable is deliberately never eligible for rollback.
enum MoveOutcome {
    NotMoved { reserved: Option<PathBuf> },
    MovedNotDurable { tombstone: PathBuf },
    Durable { tombstone: PathBuf },
}

impl MoveOutcome {
    fn tombstone(&self) -> Option<&Path> {
        match self {
            Self::NotMoved { .. } => None,
            Self::MovedNotDurable { tombstone } | Self::Durable { tombstone } => Some(tombstone),
        }
    }
}

struct StageFailure {
    error: Error,
    uncertain_barrier: Option<MoveKind>,
}

impl From<Error> for StageFailure {
    fn from(error: Error) -> Self {
        Self {
            error,
            uncertain_barrier: None,
        }
    }
}

fn reset_barrier(
    barrier: &mut impl FnMut(&str, &Path) -> Result<()>,
    kind: MoveKind,
    name: &str,
    path: &Path,
) -> std::result::Result<(), StageFailure> {
    barrier(name, path)
        .and_then(|()| sync_reset_dir(path))
        .map_err(|error| StageFailure {
            error,
            uncertain_barrier: Some(kind),
        })
}

struct ResetTransaction {
    plan: ResetPlan,
    marker: Option<PathBuf>,
    wal: MoveOutcome,
    root: MoveOutcome,
}

impl ResetTransaction {
    fn new(plan: ResetPlan, barrier: &mut impl FnMut(&str, &Path) -> Result<()>) -> Result<Self> {
        if let Some(root) = &plan.root {
            verify_locked_reset_dir(root, &plan.requested_root)?;
        } else {
            verify_reset_absent_root(&plan.requested_root)?;
        }
        // When an external WAL cannot be rolled back, the old root must never
        // open with a newly created WAL. The marker is created and synced before
        // the WAL namespace moves and checked by every ordinary opener.
        let marker = if plan.moves == ResetMoves::WalThenRoot {
            let root = &plan.root.as_ref().expect("two moves require a root").path;
            let marker = root.join(RESET_INCOMPLETE);
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(&marker)?;
            file.sync_all().map_err(|e| {
                Error::io(format!(
                    "reset marker {} remains after sync failure: {e}",
                    marker.display()
                ))
            })?;
            barrier("marker-dir", root)
                .and_then(|()| sync_reset_dir(root))
                .map_err(|e| {
                    Error::io(format!(
                        "reset marker {} remains after sync failure: {e}",
                        marker.display()
                    ))
                })?;
            Some(marker)
        } else {
            None
        };
        Ok(Self {
            plan,
            marker,
            wal: MoveOutcome::NotMoved { reserved: None },
            root: MoveOutcome::NotMoved { reserved: None },
        })
    }

    fn stage(
        &mut self,
        staged: &mut impl FnMut(&str) -> Result<()>,
        barrier: &mut impl FnMut(&str, &Path) -> Result<()>,
    ) -> std::result::Result<(), StageFailure> {
        if let Some(root) = &self.plan.root {
            verify_locked_reset_dir(root, &self.plan.requested_root)?;
        } else {
            verify_reset_absent_root(&self.plan.requested_root)?;
        }
        if matches!(self.plan.moves, ResetMoves::Wal | ResetMoves::WalThenRoot) {
            let wal = self.plan.wal.as_mut().expect("WAL move has a lease");
            let wal_path = wal.path.clone();
            stage_reset_move(
                wal,
                &wal_path,
                MoveKind::Wal,
                &mut self.wal,
                staged,
                barrier,
            )?;
        }
        if matches!(self.plan.moves, ResetMoves::Root | ResetMoves::WalThenRoot) {
            let root = self.plan.root.as_mut().expect("root move has a lease");
            // A WAL tree contained in the root moves with the root rename;
            // its lease would pin that rename on Windows.
            #[cfg(windows)]
            if let Some(wal) = self.plan.wal.as_mut() {
                if reset_path_starts_with(&wal.path, &root.path)? {
                    wal.release_lease();
                }
            }
            stage_reset_move(
                root,
                &self.plan.requested_root,
                MoveKind::Root,
                &mut self.root,
                staged,
                barrier,
            )?;
        }
        Ok(())
    }

    fn rollback(self, failure: StageFailure) -> Error {
        let marker = self.marker.as_deref();
        let outcomes = [&self.root, &self.wal];
        if let Some(kind) = failure.uncertain_barrier {
            // A failed sync may have persisted either entry. Never reattach
            // an old WAL based on an unproven namespace state.
            let tombstones: Vec<_> = outcomes
                .iter()
                .filter_map(|state| state.tombstone())
                .collect();
            let mut err = reset_stranded_error(&failure.error, &tombstones, marker);
            let current = match kind {
                MoveKind::Wal => &self.wal,
                MoveKind::Root => &self.root,
            };
            let reserved = match current {
                MoveOutcome::NotMoved { reserved } => reserved.as_deref(),
                _ => current.tombstone().and_then(Path::parent),
            };
            if let Some(reserved) = reserved {
                err = Error::storage(format!("{err}; reserved tombstone {}", reserved.display()));
            }
            return err;
        }

        let mut stranded = Vec::new();
        let mut cleanup_failed = Vec::new();
        // Root-before-WAL: an absent old root (or a root whose restoration is
        // not durable) cannot be trusted to own a restored old WAL.
        let root_safe = match (&self.plan.root, &self.root) {
            (Some(root), outcome) => restore_reset_move(
                &root.path,
                outcome,
                true,
                &mut stranded,
                &mut cleanup_failed,
            ),
            (None, _) => self.plan.wal.is_none(),
        };
        if let Some(wal) = &self.plan.wal {
            restore_reset_move(
                &wal.path,
                &self.wal,
                root_safe,
                &mut stranded,
                &mut cleanup_failed,
            );
        }
        if stranded.is_empty() && cleanup_failed.is_empty() {
            if let Some(marker) = marker {
                if let Err(e) = std::fs::remove_file(marker) {
                    cleanup_failed.push(format!("{}: {e}", marker.display()));
                } else if let Err(e) = sync_reset_dir(marker.parent().unwrap()) {
                    cleanup_failed.push(format!("{}: {e}", marker.display()));
                }
            }
        }
        if !stranded.is_empty() {
            Error::storage(format!(
                "reset failed ({}); recover old data at {}{}",
                failure.error,
                stranded.join(", "),
                if marker.is_some() {
                    "; reset-incomplete marker blocks the old root until recovery"
                } else {
                    "; do not attach the old WAL to a new root"
                }
            ))
        } else if !cleanup_failed.is_empty() {
            Error::io(format!(
                "reset failed ({}); could not remove empty tombstones at {}",
                failure.error,
                cleanup_failed.join(", ")
            ))
        } else {
            failure.error
        }
    }

    fn cleanup(self) -> Result<()> {
        let Self {
            plan, wal, root, ..
        } = self;
        // On Windows open lock handles can prevent removal of their tree.
        drop(plan);
        for outcome in [wal, root] {
            if let MoveOutcome::Durable { tombstone } = outcome {
                let container = tombstone.parent().expect("tombstone has a container");
                std::fs::remove_dir_all(container).map_err(|e| {
                    Error::io(format!(
                        "reset could not remove {}: {e}",
                        container.display()
                    ))
                })?;
            }
        }
        Ok(())
    }
}

fn verify_locked_reset_dir(dir: &LockedResetDir, requested: &Path) -> Result<()> {
    verify_reset_identity(requested, &dir.path, dir.identity)?;
    let lease = dir
        .lease
        .as_ref()
        .expect("reset lease is released only after its final identity check");
    NativeVfs::check_identity(lease, &dir.path.join(LOCK_MARKER))
}

fn verify_reset_absent_root(requested: &Path) -> Result<()> {
    reject_reset_alias(requested)?;
    let requested = crate::reset_request_path(requested);
    match std::fs::symlink_metadata(&requested) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(Error::storage(format!(
            "reset database URI appeared during reset: {}",
            requested.display()
        ))),
        Err(e) => Err(e.into()),
    }
}

fn stage_reset_move(
    dir: &mut LockedResetDir,
    requested: &Path,
    kind: MoveKind,
    outcome: &mut MoveOutcome,
    staged: &mut impl FnMut(&str) -> Result<()>,
    barrier: &mut impl FnMut(&str, &Path) -> Result<()>,
) -> std::result::Result<(), StageFailure> {
    let original = dir.path.clone();
    verify_locked_reset_dir(dir, requested)?;
    let tombstone = reset_tombstone(&original)?;
    let container = tombstone
        .parent()
        .expect("tombstone has a container")
        .to_path_buf();
    *outcome = MoveOutcome::NotMoved {
        reserved: Some(container.clone()),
    };
    let parent = original.parent().expect("reset tree has a parent");
    let name = kind.name();
    reset_barrier(barrier, kind, &format!("{name}-reserve-parent"), parent)?;
    verify_locked_reset_dir(dir, requested)?;
    // Windows refuses to rename a directory tree containing an open handle;
    // the lease is released only after its final identity check, and the
    // rename below stays the fail-closed handoff against concurrent openers.
    #[cfg(windows)]
    dir.release_lease();
    if let Err(e) = std::fs::rename(&original, &tombstone) {
        if let Err(cleanup) = std::fs::remove_dir(&container) {
            return Err(Error::io(format!(
                "reset rename failed ({e}); could not remove {}: {cleanup}",
                container.display()
            ))
            .into());
        }
        *outcome = MoveOutcome::NotMoved { reserved: None };
        return Err(Error::from(e).into());
    }
    *outcome = MoveOutcome::MovedNotDurable {
        tombstone: tombstone.clone(),
    };
    reset_barrier(barrier, kind, &format!("{name}-old-parent"), parent)?;
    reset_barrier(barrier, kind, &format!("{name}-tombstone"), &container)?;
    *outcome = MoveOutcome::Durable { tombstone };
    staged(name)?;
    Ok(())
}

fn restore_reset_move(
    original: &Path,
    outcome: &MoveOutcome,
    allowed: bool,
    stranded: &mut Vec<String>,
    cleanup_failed: &mut Vec<String>,
) -> bool {
    let tombstone = match outcome {
        MoveOutcome::NotMoved { .. } => return allowed,
        MoveOutcome::MovedNotDurable { tombstone } => {
            stranded.push(tombstone.display().to_string());
            return false;
        }
        MoveOutcome::Durable { tombstone } => tombstone,
    };
    if !allowed || rename_reset_noreplace(tombstone, original).is_err() {
        stranded.push(tombstone.display().to_string());
        return false;
    }
    let source = tombstone.parent().unwrap();
    let dest = original.parent().unwrap();
    if let Err(e) = sync_reset_dir(source).and_then(|()| sync_reset_dir(dest)) {
        cleanup_failed.push(format!("restored {}: {e}", original.display()));
        return false;
    }
    if std::fs::remove_dir(source).is_err() {
        cleanup_failed.push(source.display().to_string());
    } else if let Err(e) = sync_reset_dir(dest) {
        cleanup_failed.push(format!("removed {}: {e}", source.display()));
        return false;
    }
    true
}

fn reset_stranded_error(error: &Error, tombstones: &[&Path], marker: Option<&Path>) -> Error {
    Error::storage(format!(
        "reset barrier failed ({error}); retain old data at {}{}; do not attach old WAL to a new root",
        tombstones
            .iter()
            .map(|tombstone| tombstone.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        marker
            .map(|path| format!("; reset-incomplete marker {} remains", path.display()))
            .unwrap_or_default()
    ))
}

fn sync_reset_dir(path: &Path) -> Result<()> {
    sync_marker_parent(path)
}

/// Restore a tombstoned directory without *ever* replacing a new occupant,
/// including a dangling symlink. No existence precheck can provide this.
pub(super) fn rename_reset_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        #[cfg(target_os = "linux")]
        // SAFETY: both CStrings own NUL-terminated paths for the duration of
        // the syscall; AT_FDCWD and RENAME_NOREPLACE are valid libc constants.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from.as_ptr(),
                libc::AT_FDCWD,
                to.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        #[cfg(target_os = "macos")]
        // SAFETY: the owned CStrings remain alive during renamex_np, and the
        // exclusive-rename flag prevents replacing a new destination.
        let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if rc == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (&from, &to);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "exclusive reset rename is unsupported on this target",
            ))
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        // MoveFileW does not replace an existing destination, unlike
        // std::fs::rename on Windows. Both UTF-16 buffers are NUL terminated.
        #[link(name = "Kernel32")]
        extern "system" {
            fn MoveFileW(from: *const u16, to: *const u16) -> i32;
        }
        let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the owned UTF-16 buffers above are NUL-terminated and stay
        // alive for this call; MoveFileW does not retain their pointers.
        if unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) } != 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

fn reset_tombstone(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::storage("cannot reset a filesystem root"))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::storage("cannot reset a filesystem root"))?
        .to_string_lossy();
    for _ in 0..16 {
        let suffix = crate::vfs::random_hex_16()?;
        let candidate = parent.join(format!(".{name}.reset-{suffix}"));
        #[cfg(unix)]
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        builder.mode(0o700);
        match builder.create(&candidate) {
            Ok(()) => return Ok(candidate.join("data")),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::io("could not reserve a unique reset tombstone"))
}

#[cfg(all(test, unix))]
mod transaction_tests {
    use super::*;
    use crate::parse_uri_with_wal;

    use std::os::unix::fs::symlink;

    #[test]
    fn reset_rechecks_requested_root_after_lease_and_before_staging() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let moved = dir.path().join("moved");
        let unrelated = dir.path().join("unrelated");
        drop(parse_uri_with_wal(root.to_str().unwrap(), None).unwrap());
        std::fs::write(root.join("sentinel"), b"owned").unwrap();
        std::fs::create_dir(&unrelated).unwrap();
        std::fs::write(unrelated.join("sentinel"), b"unrelated").unwrap();

        let plan = ResetPlan::preflight(&root, None).unwrap();
        std::fs::rename(&root, &moved).unwrap();
        symlink(&unrelated, &root).unwrap();
        let err = ResetTransaction::new(plan, &mut |_, _| Ok(()))
            .err()
            .unwrap();
        assert!(matches!(err, Error::Storage(_)), "{err}");
        assert_eq!(std::fs::read_link(&root).unwrap(), unrelated);
        assert_eq!(
            std::fs::read(unrelated.join("sentinel")).unwrap(),
            b"unrelated"
        );
        assert_eq!(std::fs::read(moved.join("sentinel")).unwrap(), b"owned");
        assert!(!moved.join(RESET_INCOMPLETE).exists());
    }

    #[test]
    fn reset_rechecks_identity_immediately_before_root_rename() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let moved = dir.path().join("moved");
        let unrelated = dir.path().join("unrelated");
        drop(parse_uri_with_wal(root.to_str().unwrap(), None).unwrap());
        std::fs::write(root.join("sentinel"), b"owned").unwrap();
        std::fs::create_dir(&unrelated).unwrap();
        std::fs::write(unrelated.join("sentinel"), b"unrelated").unwrap();
        let plan = ResetPlan::preflight(&root, None).unwrap();
        let mut tx = ResetTransaction::new(plan, &mut |_, _| Ok(())).unwrap();
        let failure = tx
            .stage(&mut |_| Ok(()), &mut |phase, _| {
                if phase == "root-reserve-parent" {
                    std::fs::rename(&root, &moved)?;
                    symlink(&unrelated, &root)?;
                }
                Ok(())
            })
            .err()
            .unwrap();
        assert!(matches!(failure.error, Error::Storage(_)));
        assert_eq!(std::fs::read_link(&root).unwrap(), unrelated);
        assert_eq!(
            std::fs::read(unrelated.join("sentinel")).unwrap(),
            b"unrelated"
        );
        assert_eq!(std::fs::read(moved.join("sentinel")).unwrap(), b"owned");
    }

    #[test]
    fn reset_marker_barrier_failure_leaves_both_original_names_and_no_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let wal = dir.path().join("external");
        drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
        std::fs::write(root.join("old-root"), b"old root").unwrap();
        std::fs::write(wal.join("old-wal"), b"old WAL").unwrap();
        let plan = ResetPlan::preflight(&root, Some(&wal)).unwrap();
        let result = ResetTransaction::new(plan, &mut |stage, _| {
            assert_eq!(stage, "marker-dir");
            Err(Error::io("injected marker sync failure"))
        });
        assert!(matches!(result, Err(Error::Io(_))));
        assert_eq!(std::fs::read(root.join("old-root")).unwrap(), b"old root");
        assert_eq!(std::fs::read(wal.join("old-wal")).unwrap(), b"old WAL");
        assert!(root.join(RESET_INCOMPLETE).exists());
        assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".reset-")));
    }

    #[test]
    fn reset_move_outcomes_match_each_barrier_and_preserve_old_bytes() {
        for failed in [
            "wal-reserve-parent",
            "wal-old-parent",
            "wal-tombstone",
            "root-reserve-parent",
            "root-old-parent",
            "root-tombstone",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            let wal = dir.path().join("external");
            drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
            std::fs::write(root.join("old-root"), b"old root").unwrap();
            std::fs::write(wal.join("old-wal"), b"old WAL").unwrap();
            let plan = ResetPlan::preflight(&root, Some(&wal)).unwrap();
            assert!(plan.moves == ResetMoves::WalThenRoot);
            let mut transaction = ResetTransaction::new(plan, &mut |_, _| Ok(())).unwrap();
            let Err(failure) = transaction.stage(&mut |_| Ok(()), &mut |stage, _| {
                if stage == failed {
                    Err(Error::io("injected sync failure"))
                } else {
                    Ok(())
                }
            }) else {
                panic!("did not reach {failed}");
            };
            let wal_moved = !matches!(failed, "wal-reserve-parent");
            let root_moved = matches!(failed, "root-old-parent" | "root-tombstone");
            assert!(
                matches!(
                    (&transaction.wal, failed),
                    (
                        MoveOutcome::NotMoved { reserved: Some(_) },
                        "wal-reserve-parent"
                    ) | (
                        MoveOutcome::MovedNotDurable { .. },
                        "wal-old-parent" | "wal-tombstone"
                    ) | (
                        MoveOutcome::Durable { .. },
                        "root-reserve-parent" | "root-old-parent" | "root-tombstone"
                    )
                ),
                "{failed}: unexpected WAL stage"
            );
            assert!(
                matches!(
                    (&transaction.root, failed),
                    (
                        MoveOutcome::NotMoved { reserved: None },
                        "wal-reserve-parent" | "wal-old-parent" | "wal-tombstone"
                    ) | (
                        MoveOutcome::NotMoved { reserved: Some(_) },
                        "root-reserve-parent"
                    ) | (
                        MoveOutcome::MovedNotDurable { .. },
                        "root-old-parent" | "root-tombstone"
                    )
                ),
                "{failed}: unexpected root stage"
            );
            assert_eq!(root.exists(), !root_moved, "{failed}");
            assert_eq!(wal.exists(), !wal_moved, "{failed}");
            let old_root = if root_moved {
                transaction.root.tombstone().unwrap().join("old-root")
            } else {
                root.join("old-root")
            };
            let old_wal = if wal_moved {
                transaction.wal.tombstone().unwrap().join("old-wal")
            } else {
                wal.join("old-wal")
            };
            assert_eq!(std::fs::read(old_root).unwrap(), b"old root", "{failed}");
            assert_eq!(std::fs::read(old_wal).unwrap(), b"old WAL", "{failed}");
            let marker = if root_moved {
                transaction.root.tombstone().unwrap().join(RESET_INCOMPLETE)
            } else {
                root.join(RESET_INCOMPLETE)
            };
            assert!(marker.exists(), "{failed}");
            let error = transaction.rollback(failure);
            assert!(
                error.to_string().contains("reset barrier failed"),
                "{failed}: {error}"
            );
            assert_eq!(
                root.exists(),
                !root_moved,
                "{failed}: must not rollback uncertain sync"
            );
            assert_eq!(
                wal.exists(),
                !wal_moved,
                "{failed}: must not rollback uncertain sync"
            );
        }
    }

    #[test]
    fn reset_durable_stage_failure_restores_root_before_wal() {
        for failed in ["wal", "root"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            let wal = dir.path().join("external");
            drop(parse_uri_with_wal(root.to_str().unwrap(), Some(wal.clone())).unwrap());
            std::fs::write(root.join("old-root"), b"old root").unwrap();
            std::fs::write(wal.join("old-wal"), b"old WAL").unwrap();
            let plan = ResetPlan::preflight(&root, Some(&wal)).unwrap();
            let mut transaction = ResetTransaction::new(plan, &mut |_, _| Ok(())).unwrap();
            let Err(failure) = transaction.stage(
                &mut |stage| {
                    if stage == failed {
                        Err(Error::io("injected staged failure"))
                    } else {
                        Ok(())
                    }
                },
                &mut |_, _| Ok(()),
            ) else {
                panic!("did not reach {failed}");
            };
            assert!(matches!(transaction.wal, MoveOutcome::Durable { .. }));
            assert!(matches!(
                (&transaction.root, failed),
                (MoveOutcome::NotMoved { .. }, "wal") | (MoveOutcome::Durable { .. }, "root")
            ));
            assert!(!wal.exists());
            assert_eq!(root.exists(), failed == "wal");
            let error = transaction.rollback(failure);
            assert!(error.to_string().contains("injected staged failure"));
            assert_eq!(std::fs::read(root.join("old-root")).unwrap(), b"old root");
            assert_eq!(std::fs::read(wal.join("old-wal")).unwrap(), b"old WAL");
            assert!(!root.join(RESET_INCOMPLETE).exists());
            assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".reset-")));
        }
    }
}
