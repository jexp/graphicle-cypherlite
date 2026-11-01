//! The reset transaction: staged namespace moves, rollback and cleanup.
//!
//! Plan admission and the locking order live in the parent module; this
//! module owns what happens once both identities are locked — staging each
//! move behind an fsync barrier, rolling back deterministically, and never
//! rolling back an unproven namespace state. The tombstone reservation is
//! the transaction's own machinery; the exclusive-rename primitive it
//! restores through is host plumbing in the VFS layer
//! (`crate::vfs::host::host_rename_noreplace`).
use super::super::identity::sync_marker_parent as sync_reset_dir;
use super::super::identity::verify_reset_identity;
use super::super::{LOCK_MARKER, RESET_INCOMPLETE};
use super::{LockedResetDir, ResetPlan};
use crate::vfs::host::host_rename_noreplace;
use crate::vfs::NativeVfs;
use cypherlite_core::{Error, Result};
use std::path::{Path, PathBuf};

/// One staged namespace move the transaction performs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MoveKind {
    Wal,
    Root,
}

/// One barrier site within a single namespace move.
#[derive(Clone, Copy)]
enum BarrierSite {
    /// The moving tree's parent, before the tombstone is reserved.
    ReserveParent,
    /// The moving tree's parent, after the rename removed the old name.
    OldParent,
    /// The tombstone container, after the rename published the new name.
    Tombstone,
}

/// One stage of the reset transaction's fsync-barrier sequence — the typed
/// vocabulary the `staged` and barrier test seams receive, never a string
/// built by concatenation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResetStage {
    /// The reset-incomplete marker's own directory, before the marker is durable.
    MarkerDir,
    WalReserveParent,
    WalOldParent,
    WalTombstone,
    RootReserveParent,
    RootOldParent,
    RootTombstone,
}

impl MoveKind {
    /// The typed stage of one barrier site in this move.
    fn stage(self, site: BarrierSite) -> ResetStage {
        match (self, site) {
            (Self::Wal, BarrierSite::ReserveParent) => ResetStage::WalReserveParent,
            (Self::Wal, BarrierSite::OldParent) => ResetStage::WalOldParent,
            (Self::Wal, BarrierSite::Tombstone) => ResetStage::WalTombstone,
            (Self::Root, BarrierSite::ReserveParent) => ResetStage::RootReserveParent,
            (Self::Root, BarrierSite::OldParent) => ResetStage::RootOldParent,
            (Self::Root, BarrierSite::Tombstone) => ResetStage::RootTombstone,
        }
    }
}

/// A reserved reset tombstone: the private `.<name>.reset-<hex>` container
/// created beside the moving tree's parent, and the `data` entry inside it
/// the tree is renamed onto. Carried together so no consumer re-derives
/// the container by parentage.
struct ResetTombstone {
    container: PathBuf,
    data: PathBuf,
}

// A reserved container is not a published move. A published rename becomes
// durable only after BOTH directory entries have been synced. On a failed
// barrier, MovedNotDurable is deliberately never eligible for rollback.
enum MoveOutcome {
    NotMoved { reserved: Option<PathBuf> },
    MovedNotDurable { tombstone: ResetTombstone },
    Durable { tombstone: ResetTombstone },
}

impl MoveOutcome {
    /// The tombstoned tree's `data` path, once the move published it.
    fn tombstone(&self) -> Option<&Path> {
        match self {
            Self::NotMoved { .. } => None,
            Self::MovedNotDurable { tombstone } | Self::Durable { tombstone } => {
                Some(&tombstone.data)
            }
        }
    }

    /// The reserved container: carried since reservation for a not-yet
    /// moved tree, and from the tombstone itself once the move published it.
    fn reserved(&self) -> Option<&Path> {
        match self {
            Self::NotMoved { reserved } => reserved.as_deref(),
            Self::MovedNotDurable { tombstone } | Self::Durable { tombstone } => {
                Some(&tombstone.container)
            }
        }
    }
}

pub(super) struct StageFailure {
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
    barrier: &mut impl FnMut(ResetStage, &Path) -> Result<()>,
    kind: MoveKind,
    site: BarrierSite,
    path: &Path,
) -> std::result::Result<(), StageFailure> {
    barrier(kind.stage(site), path)
        .and_then(|()| sync_reset_dir(path))
        .map_err(|error| StageFailure {
            error,
            uncertain_barrier: Some(kind),
        })
}

/// The marker's own failure text: a marker that outlives a failed sync
/// stays put and blocks the old root until recovery — the one spelling
/// shared by the marker's file sync and its directory barrier.
fn marker_remains(marker: &Path, error: impl std::fmt::Display) -> Error {
    Error::io(format!(
        "reset marker {} remains after sync failure: {error}",
        marker.display()
    ))
}

/// A marker left by a previous incomplete reset already blocks this root:
/// the `create_new` open surfaces it as the same authored marker-state
/// contract (mirroring [`marker_remains`]) instead of a raw
/// `AlreadyExists` io error — resetting a blocked root fails with the
/// recovery procedure, like every other marker-state surprise.
fn marker_blocks_reset(marker: &Path) -> Error {
    Error::storage(format!(
        "reset marker {} remains after an incomplete reset; recover the old WAL before resetting",
        marker.display()
    ))
}

/// The reset-incomplete marker together with the locked root directory
/// it was joined onto — carried since construction (the one place the
/// root is in hand), so rollback syncs the marker's own directory
/// through the root it already holds instead of re-deriving the
/// marker-implies-root correlation by expect.
struct ResetMarker {
    path: PathBuf,
    root: PathBuf,
}

/// Create and durably publish the reset-incomplete marker in the locked
/// root, and carry it with that root: an exclusive `create_new` open (a
/// marker left by a previous incomplete reset already blocks this root),
/// the marker file's own sync, then its directory barrier — the whole
/// recipe the two-move transaction runs before any namespace move, with
/// its failure texts ([`marker_remains`], [`marker_blocks_reset`])
/// beside it.
fn create_reset_marker(
    root: &Path,
    barrier: &mut impl FnMut(ResetStage, &Path) -> Result<()>,
) -> Result<ResetMarker> {
    let marker = root.join(RESET_INCOMPLETE);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&marker).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            marker_blocks_reset(&marker)
        } else {
            Error::from(e)
        }
    })?;
    file.sync_all().map_err(|e| marker_remains(&marker, e))?;
    barrier(ResetStage::MarkerDir, root)
        .and_then(|()| sync_reset_dir(root))
        .map_err(|e| marker_remains(&marker, e))?;
    Ok(ResetMarker {
        path: marker,
        root: root.to_path_buf(),
    })
}

/// One cleanup-failure record: the failed step, the path it failed on,
/// and the cause — the shared shape every `cleanup_failed` push carries,
/// so the rollback error's cleanup list never mixes bare paths with
/// sentence fragments or drops the reason a cleanup step failed.
fn cleanup_failed_at(step: &str, path: &Path, error: impl std::fmt::Display) -> String {
    format!("{step} {}: {error}", path.display())
}

pub(super) struct ResetTransaction {
    plan: ResetPlan,
    marker: Option<ResetMarker>,
    wal: MoveOutcome,
    root: MoveOutcome,
}

impl ResetTransaction {
    pub(super) fn new(
        mut plan: ResetPlan,
        barrier: &mut impl FnMut(ResetStage, &Path) -> Result<()>,
    ) -> Result<Self> {
        plan.recheck_requested_root()?;
        // When an external WAL cannot be rolled back, the old root must never
        // open with a newly created WAL. The marker is created and synced before
        // the WAL namespace moves and checked by every ordinary opener. Only
        // the two-move plan needs it, and that plan always holds the root.
        let marker = plan
            .two_move_root()
            .map(|root| create_reset_marker(&root.path, barrier))
            .transpose()?;
        Ok(Self {
            plan,
            marker,
            wal: MoveOutcome::NotMoved { reserved: None },
            root: MoveOutcome::NotMoved { reserved: None },
        })
    }

    pub(super) fn stage(
        &mut self,
        staged: &mut impl FnMut(MoveKind) -> Result<()>,
        barrier: &mut impl FnMut(ResetStage, &Path) -> Result<()>,
    ) -> std::result::Result<(), StageFailure> {
        self.plan.recheck_requested_root()?;
        // Stage the WAL first: for disjoint paths, once the root moves no
        // new owner can ever attach the old WAL.
        if let Some(wal) = self.plan.move_target(MoveKind::Wal) {
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
        // A WAL tree contained in the root moves with the root rename; its
        // lease would pin that rename on Windows. Released while the plan is
        // still borrowable, before the root move takes its own borrow.
        #[cfg(windows)]
        self.plan.release_contained_wal_lease()?;
        let requested_root = self.plan.requested_root.clone();
        if let Some(root) = self.plan.move_target(MoveKind::Root) {
            stage_reset_move(
                root,
                &requested_root,
                MoveKind::Root,
                &mut self.root,
                staged,
                barrier,
            )?;
        }
        Ok(())
    }

    pub(super) fn rollback(self, failure: StageFailure) -> Error {
        let marker = self.marker.as_ref();
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
            if let Some(reserved) = current.reserved() {
                err = Error::storage(format!("{err}; reserved tombstone {}", reserved.display()));
            }
            return err;
        }

        let mut stranded = Vec::new();
        let mut cleanup_failed = Vec::new();
        // Root-before-WAL: an absent old root (or a root whose restoration is
        // not durable) cannot be trusted to own a restored old WAL.
        let root_safe = match &self.plan.root {
            Some(root) => restore_reset_move(
                root,
                &self.root,
                RestoreAllowed::RootMove,
                &mut stranded,
                &mut cleanup_failed,
            ),
            // An unlocked plan never proved an old root at all — the same
            // verdict the WAL-restore arm reaches for any restoration that
            // is not proven durable.
            None => false,
        };
        if let Some(wal) = &self.plan.wal {
            restore_reset_move(
                wal,
                &self.wal,
                RestoreAllowed::WalMove {
                    root_restored: root_safe,
                },
                &mut stranded,
                &mut cleanup_failed,
            );
        }
        // The marker is removed only once every restoration proved clean: it
        // outlives any stranded data or failed cleanup, keeping the old root
        // blocked until recovery.
        if stranded.is_empty() && cleanup_failed.is_empty() {
            if let Some(marker) = marker {
                remove_reset_marker(marker, &mut cleanup_failed);
            }
        }
        rollback_error(failure, stranded, cleanup_failed, marker)
    }

    pub(super) fn cleanup(self) -> Result<()> {
        let Self {
            plan, wal, root, ..
        } = self;
        // On Windows open lock handles can prevent removal of their tree.
        drop(plan);
        for outcome in [wal, root] {
            if let MoveOutcome::Durable { tombstone } = outcome {
                std::fs::remove_dir_all(&tombstone.container).map_err(|e| {
                    Error::io(format!(
                        "reset could not remove {}: {e}",
                        tombstone.container.display()
                    ))
                })?;
            }
        }
        Ok(())
    }
}

/// Remove the reset-incomplete marker once every restoration proved
/// clean, syncing the marker's own directory — the locked root the
/// marker has carried since construction — so the removal is durable.
fn remove_reset_marker(marker: &ResetMarker, cleanup_failed: &mut Vec<String>) {
    if let Err(e) = std::fs::remove_file(&marker.path) {
        cleanup_failed.push(cleanup_failed_at("could not remove", &marker.path, e));
    } else if let Err(e) = sync_reset_dir(&marker.root) {
        cleanup_failed.push(cleanup_failed_at("could not sync", &marker.path, e));
    }
}

/// The rollback verdict, in precedence order: stranded old data outranks
/// cleanup failures, and both outrank the original failure. The stranded
/// text carries the marker-state suffix — a surviving marker blocks the
/// old root until recovery; no marker means the old WAL must never
/// attach to a new root.
fn rollback_error(
    failure: StageFailure,
    stranded: Vec<String>,
    cleanup_failed: Vec<String>,
    marker: Option<&ResetMarker>,
) -> Error {
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

fn stage_reset_move(
    dir: &mut LockedResetDir,
    requested: &Path,
    kind: MoveKind,
    outcome: &mut MoveOutcome,
    staged: &mut impl FnMut(MoveKind) -> Result<()>,
    barrier: &mut impl FnMut(ResetStage, &Path) -> Result<()>,
) -> std::result::Result<(), StageFailure> {
    verify_locked_reset_dir(dir, requested)?;
    let ResetTombstone { container, data } = reset_tombstone(&dir.path)?;
    *outcome = MoveOutcome::NotMoved {
        reserved: Some(container.clone()),
    };
    reset_barrier(barrier, kind, BarrierSite::ReserveParent, &dir.parent)?;
    verify_locked_reset_dir(dir, requested)?;
    // Windows refuses to rename a directory tree containing an open handle;
    // the lease is released only after its final identity check, and the
    // rename below stays the fail-closed handoff against concurrent openers.
    #[cfg(windows)]
    dir.release_lease();
    if let Err(e) = std::fs::rename(&dir.path, &data) {
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
        tombstone: ResetTombstone {
            container: container.clone(),
            data: data.clone(),
        },
    };
    reset_barrier(barrier, kind, BarrierSite::OldParent, &dir.parent)?;
    reset_barrier(barrier, kind, BarrierSite::Tombstone, &container)?;
    *outcome = MoveOutcome::Durable {
        tombstone: ResetTombstone { container, data },
    };
    staged(kind)?;
    Ok(())
}

/// When a moved tree may be restored to its original name.
#[derive(Clone, Copy)]
enum RestoreAllowed {
    /// The root move restores first and unconditionally.
    RootMove,
    /// A WAL restores only once the old root's own restoration proved
    /// durable — an absent or unproven old root can never own a restored
    /// old WAL.
    WalMove { root_restored: bool },
}

/// Restore a tombstoned directory without *ever* replacing a new occupant,
/// including a dangling symlink. No existence precheck can provide this.
/// Returns whether the restoration is proven durable.
fn restore_reset_move(
    original: &LockedResetDir,
    outcome: &MoveOutcome,
    allowed: RestoreAllowed,
    stranded: &mut Vec<String>,
    cleanup_failed: &mut Vec<String>,
) -> bool {
    let allowed = match allowed {
        RestoreAllowed::RootMove => true,
        RestoreAllowed::WalMove { root_restored } => root_restored,
    };
    let tombstone = match outcome {
        MoveOutcome::NotMoved { .. } => return allowed,
        MoveOutcome::MovedNotDurable { tombstone } => {
            stranded.push(tombstone.data.display().to_string());
            return false;
        }
        MoveOutcome::Durable { tombstone } => tombstone,
    };
    if !allowed || host_rename_noreplace(&tombstone.data, &original.path).is_err() {
        stranded.push(tombstone.data.display().to_string());
        return false;
    }
    let source = &tombstone.container;
    let dest = &original.parent;
    if let Err(e) = sync_reset_dir(source).and_then(|()| sync_reset_dir(dest)) {
        cleanup_failed.push(cleanup_failed_at("restored", &original.path, e));
        return false;
    }
    if let Err(e) = std::fs::remove_dir(source) {
        cleanup_failed.push(cleanup_failed_at("could not remove", source, e));
    } else if let Err(e) = sync_reset_dir(dest) {
        cleanup_failed.push(cleanup_failed_at("removed", source, e));
        return false;
    }
    true
}

fn reset_stranded_error(
    error: &Error,
    tombstones: &[&Path],
    marker: Option<&ResetMarker>,
) -> Error {
    Error::storage(format!(
        "reset barrier failed ({error}); retain old data at {}{}; do not attach old WAL to a new root",
        tombstones
            .iter()
            .map(|tombstone| tombstone.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        marker
            .map(|marker| format!("; reset-incomplete marker {} remains", marker.path.display()))
            .unwrap_or_default()
    ))
}

/// Re-verify a locked reset directory against the requested path: the
/// identity must be unchanged and the lease must still name the marker.
pub(super) fn verify_locked_reset_dir(dir: &LockedResetDir, requested: &Path) -> Result<()> {
    verify_reset_identity(requested, &dir.path, dir.identity)?;
    let lease = dir
        .lease
        .as_ref()
        .expect("reset lease is released only after its final identity check");
    NativeVfs::check_identity(lease, &dir.path.join(LOCK_MARKER))
}

/// The tombstone container's name mark: the `.<name>.reset-<hex>`
/// containers every reset move stages. Named here, beside
/// [`reset_tombstone`] — the one place that builds the names — so the
/// refusal tests' no-residue sweep keys on the same fact instead of a
/// re-spelled literal.
pub(crate) const RESET_TOMBSTONE_MARK: &str = ".reset-";

fn reset_tombstone(path: &Path) -> Result<ResetTombstone> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::storage("cannot reset a filesystem root"))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::storage("cannot reset a filesystem root"))?
        .to_string_lossy();
    for _ in 0..16 {
        let suffix = crate::vfs::random_hex_16()?;
        let container = parent.join(format!(".{name}{RESET_TOMBSTONE_MARK}{suffix}"));
        // The crate's one private-dir-create policy (single component,
        // 0o700, `AlreadyExists` = retry signal): the container's parent is
        // the moving tree's own parent and always exists, so the
        // recursive-parent half is a no-op.
        match NativeVfs::create_private_dir(&container) {
            Ok(true) => {
                let data = container.join("data");
                return Ok(ResetTombstone { container, data });
            }
            Ok(false) => (),
            Err(e) => return Err(e),
        }
    }
    Err(Error::io("could not reserve a unique reset tombstone"))
}

/// The transaction's own tests (unix: the swap-attack and crash legs
/// exercise rename/symlink semantics): a sibling file, keeping this
/// module's production surface under the no-1k-line rule.
#[cfg(all(test, unix))]
#[path = "transaction/tests.rs"]
mod transaction_tests;
