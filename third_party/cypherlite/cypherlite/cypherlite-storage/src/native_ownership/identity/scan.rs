//! Nested-owner scans: walk every descendant of a root (or WAL tree) and
//! refuse any nested database identity, exempting only the caller's own.
use std::path::{Path, PathBuf};

use cypherlite_core::file_identity;
use cypherlite_core::{Error, Result};

use super::super::LOCK_MARKER;
use super::evidence::direct_wal;
use super::preflight::{preflight_role, RoleCheck};
use super::{markdown_root_shape, markdown_wal, RootShape};
use crate::vfs::native::{native_entry_kind, NativeEntryKind};

/// The caller's WAL context for a nested-owner scan: one three-state fact
/// instead of two independent Options whose fourth combination (an owned
/// WAL with no WAL context at all) was nonsense.
pub(crate) enum WalExemption<'a> {
    /// No WAL context: no marker is exempt.
    Unset,
    /// The caller's WAL spelling names the root's default-WAL slot, which
    /// must verify as a typed WAL by preflight before its marker is exempt.
    VerifyByPreflight(&'a Path),
    /// The caller has already verified this WAL as its own — created by
    /// this open, typed under its lease, or verified under the reset
    /// lease — so its marker is exempt without re-verification.
    Owned(&'a Path),
}

/// The default-WAL slot whose marker this scan may exempt, and whether it
/// is exempt (the caller owns it) or must first verify as a typed WAL.
///
/// The one spelling of the exemption decision (ADR-0406 §3): the slot is
/// the direct `wal/` child when the root retains one, else the markdown
/// vault's `.cypherlite/wal/` when the caller's WAL is that spelling. An
/// owned WAL is trusted exactly when it is that slot itself — the direct
/// layout proves it by physical alias, the vault layout by spelling; every
/// other WAL-shaped candidate re-verifies by preflight. A pre-existing
/// untyped direct WAL never proves its ownership, even when the parent has
/// a manifest or is still using that WAL path.
fn owned_wal_exemption(root: &Path, exemption: WalExemption<'_>) -> Result<Option<PathBuf>> {
    let wal = match exemption {
        WalExemption::Unset => return Ok(None),
        WalExemption::VerifyByPreflight(wal) | WalExemption::Owned(wal) => wal,
    };
    let direct = root.join(crate::codec::SnapshotCodecKind::Json.wal_dir());
    // The vault shape is decided first: a markdown store's engine WAL is
    // the `.cypherlite/` sidecar layout, so a root-level `wal/`
    // directory in a vault is an ordinary label directory of user
    // content (the engine itself writes `wal/<name>.md` for a `:wal`
    // label) — never the classic direct-WAL slot, and never an
    // ambiguity the open must refuse.
    let (slot, direct_layout) = if markdown_root_shape(root, wal) == RootShape::Vault {
        (markdown_wal(root), false)
    } else if direct_wal(root, &direct)? {
        (direct, true)
    } else {
        return Ok(None);
    };
    let trusted = match exemption {
        WalExemption::Owned(_) => !direct_layout || direct_wal(root, wal)?,
        WalExemption::VerifyByPreflight(_) | WalExemption::Unset => false,
    };
    if trusted {
        return Ok(Some(slot));
    }
    let check = RoleCheck::Wal {
        newly_created: false,
    };
    Ok(preflight_role(&slot, check)?.then_some(slot))
}

// Do not recurse through symlinks or Windows junctions. A reset unlinks the
// alias itself, not the independently owned tree it points to.
pub(crate) fn scan_nested_databases(root: &Path, exemption: WalExemption<'_>) -> Result<()> {
    let exempt_wal = owned_wal_exemption(root, exemption)?;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let lock_name = name
                .to_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(LOCK_MARKER));
            let bundle_name = crate::vfs::is_bundle_path(&path);
            let kind = entry.file_type()?;
            #[cfg(windows)]
            let reparse = {
                use std::os::windows::fs::MetadataExt;
                // A junction may report as a directory. Inspect directories
                // before recursing, and candidates before claiming identity.
                (kind.is_dir() || lock_name || bundle_name)
                    && std::fs::symlink_metadata(&path)?.file_attributes()
                        & crate::vfs::native::REPARSE_POINT
                        != 0
            };
            #[cfg(not(windows))]
            let reparse = false;
            if lock_name && !owned_identity_marker(&path, root, exempt_wal.as_deref())? {
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
pub(crate) fn reset_path_starts_with(path: &Path, prefix: &Path) -> Result<bool> {
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
        (Some(left), Some(right)) => Ok(!super::is_same_dir(&left, &right)?),
        _ => Ok(false),
    }
}

fn same_physical_identity(left: &Path, right: &Path) -> Result<bool> {
    Ok(file_identity::of_path(left)? == file_identity::of_path(right)?)
}

/// The entry shape an owned identity must match between the actual and
/// expected spellings — the typed replacement for a one-off `directory:
/// bool` mode parameter; each arm owns its shape comparison, expressed
/// through the one [`native_entry_kind`] classifier.
#[derive(Clone, Copy)]
enum EntryShape {
    Directory,
    File,
}

impl EntryShape {
    fn matches(self, meta: &std::fs::Metadata) -> bool {
        match self {
            Self::Directory => matches!(native_entry_kind(meta), NativeEntryKind::Dir),
            Self::File => matches!(native_entry_kind(meta), NativeEntryKind::File),
        }
    }
}

fn owned_identity_entry(actual: &Path, expected: &Path, shape: EntryShape) -> Result<bool> {
    let actual_meta = std::fs::symlink_metadata(actual)?;
    let expected_meta = match std::fs::symlink_metadata(expected) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    Ok(shape.matches(&actual_meta)
        && shape.matches(&expected_meta)
        && same_physical_identity(actual, expected)?)
}

fn owned_identity_marker(path: &Path, root: &Path, exempt_wal: Option<&Path>) -> Result<bool> {
    if path.parent() == Some(root)
        && owned_identity_entry(path, &root.join(LOCK_MARKER), EntryShape::File)?
    {
        return Ok(true);
    }
    // The exempt WAL's own marker — the direct `wal/` child (including a
    // differently-cased alias of it) or the markdown vault's
    // `.cypherlite/wal/` (ADR-0406 §3) — is owned exactly like the root's:
    // verified through inode identity, never by lexical path spelling.
    let Some(wal) = exempt_wal else {
        return Ok(false);
    };
    // Cheap lexical prefilter before the inode checks: the marker sits in
    // the exempt WAL itself (exact spelling), or one level under the root
    // (a differently-cased alias of the direct `wal/` child).
    let parent = match path.parent() {
        Some(parent) if parent == wal || parent.parent() == Some(root) => parent,
        _ => return Ok(false),
    };
    Ok(owned_identity_entry(parent, wal, EntryShape::Directory)?
        && owned_identity_entry(path, &wal.join(LOCK_MARKER), EntryShape::File)?)
}

fn direct_reset_wal(root: &Path, wal: &Path) -> Result<bool> {
    let direct = root.join(crate::codec::SnapshotCodecKind::Json.wal_dir());
    if wal == direct {
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
    owned_identity_entry(wal, &direct, EntryShape::Directory)
}

/// The pairwise root↔WAL containment decision — one named concept
/// instead of the direction-sensitive [`reset_path_starts_with`]
/// primitive re-composed at every consumer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Overlap {
    /// The WAL tree contains the root (the root sits beneath the WAL).
    WalContainsRoot,
    /// The root contains the WAL (the WAL sits beneath the root).
    RootContainsWal,
    /// Neither tree contains the other.
    Disjoint,
}

/// Classify how `root` and `wal` overlap — conservative for missing
/// components on case-folding platforms, exactly as
/// [`reset_path_starts_with`] is.
pub(crate) fn overlap(root: &Path, wal: &Path) -> Result<Overlap> {
    if reset_path_starts_with(root, wal)? {
        Ok(Overlap::WalContainsRoot)
    } else if reset_path_starts_with(wal, root)? {
        Ok(Overlap::RootContainsWal)
    } else {
        Ok(Overlap::Disjoint)
    }
}

/// Refuse every root↔WAL overlap but the one safe layout (a WAL that is
/// the root's direct `wal/` child), returning the [`Overlap`] it
/// validated — so a caller that also needs the layout fact for its own
/// containment gate consults this verdict instead of re-computing
/// `overlap` on the same pair.
pub(crate) fn validate_reset_overlap(root: &Path, wal: &Path) -> Result<Overlap> {
    let layout = overlap(root, wal)?;
    if layout != Overlap::Disjoint && !direct_reset_wal(root, wal)? {
        return Err(Error::storage(format!(
            "reset WAL directory {} overlaps database root {}; only a direct contained wal directory is safe",
            wal.display(),
            root.display()
        )));
    }
    Ok(layout)
}
