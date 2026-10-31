//! Host-file open and locking for the single-file bundle.
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cypherlite_core::{Error, Result};

use crate::native_ownership::reject_nested;

use super::super::native::{map_lock_result, NativeVfs, OpenAdmit};
use super::catalog::{init_empty, load_existing, load_peek};
use super::{Bundle, BundleVfs, Inner};

/// How the host path is admitted before the lock is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HostAdmit {
    /// Create missing parents and accept a missing file.
    Create,
    /// Create missing parents; the file itself must be created by this
    /// call. An existing entry — file, directory, or a raced-in symlink,
    /// which is never followed — is refused (ADR-0161).
    CreateExclusive,
    /// Exclusive lock of whatever is already there, including a missing file.
    Exclusive,
    /// Shared lock of an existing regular file.
    Shared,
}

fn resolve_bundle_path(path: &Path, admit: HostAdmit) -> Result<PathBuf> {
    let create = matches!(admit, HostAdmit::Create | HostAdmit::CreateExclusive);
    let exclusive = matches!(admit, HostAdmit::CreateExclusive);
    let shared = matches!(admit, HostAdmit::Shared);
    if create || shared {
        reject_nested(path)?;
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if create {
        NativeVfs::create_private_dir_all(parent)?;
    } else if shared && !parent.exists() {
        return Err(Error::storage(format!(
            "database not found: {}",
            path.display()
        )));
    }
    let parent = std::fs::canonicalize(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::storage("bundle path has no file name"))?;
    let mut resolved = parent.join(name);
    match std::fs::symlink_metadata(&resolved) {
        // An exclusive create adopts nothing: any existing entry, including
        // a symlink raced into the window, refuses the destination
        // (ADR-0161). The authoritative check is the `create_new` open.
        Ok(_) if exclusive => {
            return Err(crate::destination_exists(&resolved));
        }
        Ok(meta) if meta.file_type().is_symlink() => {
            resolved = std::fs::canonicalize(&resolved)?;
        }
        Ok(meta) if meta.file_type().is_dir() => {
            return Err(Error::storage(format!(
                "{} is a directory; .cl URIs require a file",
                resolved.display()
            )));
        }
        Ok(meta) if shared && !meta.is_file() => {
            return Err(Error::storage(format!(
                "{} is not a .cl bundle file",
                resolved.display()
            )));
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if shared {
                return Err(Error::storage(format!(
                    "database not found: {}",
                    resolved.display()
                )));
            }
        }
        Err(e) => return Err(e.into()),
    }
    Ok(resolved)
}

/// The shared-admission open primitive: a read-only, no-follow open of the
/// resolved existing host file. Both the locking read-only open
/// ([`BundleVfs::lock_host`] under [`HostAdmit::Shared`]) and the lock-free
/// peek ([`BundleVfs::open_peek`]) open the host this way.
fn open_shared_readonly(path: &Path) -> Result<File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    cypherlite_core::file_identity::nofollow(&mut opts);
    NativeVfs::open_checked_existing(path, &opts)
}

impl BundleVfs {
    pub fn open_or_create(path: impl Into<PathBuf>) -> Result<Self> {
        // Lock the very handle used for initialization, before reading or
        // writing even the first superblock; no host-side .cl sidecar.
        let (path, mut file) = Self::lock_host(&path.into(), HostAdmit::Create)?;
        let meta = file.metadata()?;
        let bundle = if meta.len() == 0 {
            init_empty(&mut file)?
        } else {
            load_existing(&mut file)?
        };
        Ok(Self::from_host_parts(path, file, bundle))
    }

    /// [`open_or_create`](Self::open_or_create) restricted to a destination
    /// this call creates (ADR-0161).
    ///
    /// The host file is created exclusively: an existing entry — including
    /// a symlink raced into the create window, which is never followed —
    /// fails with a destination-already-exists storage error
    /// ([`crate::DESTINATION_EXISTS`]) instead of being adopted. Publish
    /// paths (clone/snapshot/restore) use this so the loser of a create
    /// race never opens or deletes the winner's bundle.
    pub fn open_create_exclusive(path: impl Into<PathBuf>) -> Result<Self> {
        // Lock the very handle used for initialization, before writing even
        // the first superblock; no host-side .cl sidecar.
        let (path, mut file) = Self::lock_host(&path.into(), HostAdmit::CreateExclusive)?;
        // An exclusively created file is empty, so initialization always
        // starts from a fresh catalog.
        let bundle = init_empty(&mut file)?;
        Ok(Self::from_host_parts(path, file, bundle))
    }

    /// Shared lock on an existing bundle (ADR-0136). Does not create the host
    /// file. The writable opener still holds `LOCK_EX`, so this fails with
    /// [`Error::Locked`] while a writer has the bundle open.
    pub fn open_read_only(path: impl Into<PathBuf>) -> Result<Self> {
        let (path, mut file) = Self::lock_host(&path.into(), HostAdmit::Shared)?;
        let meta = file.metadata()?;
        if meta.len() == 0 {
            return Err(Error::storage(
                "read-only open requires an existing database",
            ));
        }
        // The lock rides the handle: an error path drops `file` and closing
        // the descriptor releases the shared lock — the same drop-based
        // release `open_or_create` relies on for its `?` paths. A read-only
        // open must never truncate: it peeks, keeping just the bundle (the
        // physical-tail truncation belongs to the lock-holding, writable
        // `load_existing` alone).
        let (bundle, _, _) = load_peek(&mut file)?;
        Ok(Self::from_host_parts(path, file, bundle))
    }

    /// Lock-free read-only peek at an existing bundle (ADR-0165). Unlike
    /// [`open_read_only`](Self::open_read_only) no lock is taken, so a cold
    /// metadata reader can inspect the store beside a live writer. The host
    /// handle is read-only: any write through the returned Vfs fails at the
    /// OS, and the peek load never mutates the file (the physical-tail
    /// truncation belongs to the lock-holding `load_existing` alone).
    /// Returns the selected superblock's `VERSION` and the host file length,
    /// statted once inside the load.
    ///
    /// Beside a live writer the outcome is platform-split. Unix locks are
    /// advisory, so the lock-free load reads beside the writer. On Windows
    /// the writer's lock is mandatory (see [`windows_lock_probe`]), so the
    /// peek fails with the same typed [`Error::Locked`] a locking open
    /// returns (ADR-0136).
    pub(crate) fn open_peek(path: &Path) -> Result<(Self, u32, u64)> {
        let path = resolve_bundle_path(path, HostAdmit::Shared)?;
        let mut file = open_shared_readonly(&path)?;
        #[cfg(windows)]
        windows_lock_probe(&file, &path)?;
        let (bundle, version, host_len) = load_peek(&mut file)?;
        Ok((Self::from_host_parts(path, file, bundle), version, host_len))
    }

    /// Owns the "bundle.file is the exact handle that acquired the lock"
    /// invariant: a temporary cloned descriptor is used during
    /// initialization, but closing the original on Windows could otherwise
    /// release its lock. The lock-free peek ([`open_peek`](Self::open_peek))
    /// shares the constructor; its handle holds no lock to release.
    fn from_host_parts(path: PathBuf, file: File, mut bundle: Bundle) -> Self {
        bundle.file = file;
        Self {
            inner: Arc::new(Inner {
                bundle: Mutex::new(bundle),
            }),
            path,
        }
    }

    /// Resolve, open and lock the host file under `admit`. The admit policy
    /// selects the open primitive; the lock is shared for
    /// [`HostAdmit::Shared`] and exclusive otherwise.
    pub(crate) fn lock_host(path: &Path, admit: HostAdmit) -> Result<(PathBuf, File)> {
        use fs4::FileExt;
        let path = resolve_bundle_path(path, admit)?;
        let file = match admit {
            HostAdmit::Create => NativeVfs::open_regular_file(&path, OpenAdmit::OpenOrCreate)?,
            HostAdmit::CreateExclusive => {
                NativeVfs::open_regular_file(&path, OpenAdmit::CreateExclusive)?
            }
            HostAdmit::Exclusive => NativeVfs::open_regular_file(&path, OpenAdmit::Existing)?,
            HostAdmit::Shared => open_shared_readonly(&path)?,
        };
        let locked = if matches!(admit, HostAdmit::Shared) {
            fs4::FileExt::try_lock_shared(&file)
        } else {
            file.try_lock_exclusive()
        };
        map_lock_result(locked, &path)?;
        // A reset can rename the entry between resolve and lock; prove the
        // locked handle still names `path`. Reset's own preflight
        // (HostAdmit::Exclusive) skips this re-check by design.
        if admit != HostAdmit::Exclusive {
            reject_nested(&path)?;
            NativeVfs::check_identity(&file, &path)?;
        }
        Ok((path, file))
    }
}

/// Windows probe for the lock-free peek ([`BundleVfs::open_peek`]). The
/// writer's bundle lock is one fs4 `LockFileEx` byte-range lock over the
/// whole host file (`[0, u64::MAX)`), and Windows enforces it against every
/// handle but the owner's — same process included — so the peek's first
/// superblock read on its fresh handle fails with `ERROR_LOCK_VIOLATION`
/// (os error 33) while a writer holds the bundle. Probe the lock instead of
/// surfacing that raw I/O error: contention maps to the same typed
/// [`Error::Locked`] a locking open returns (ADR-0136). The probe releases
/// immediately — a diagnostic must never block a writer — so a writer
/// racing in after it still fails the load with a typed `Error::Io`, never
/// a panic. Only Windows needs the probe: Unix locks are advisory, and a
/// `flock` probe there would report contention the lock-free read ignores.
#[cfg(windows)]
fn windows_lock_probe(file: &File, path: &Path) -> Result<()> {
    map_lock_result(fs4::FileExt::try_lock_shared(file), path)?;
    // The release is load-bearing: `file` is borrowed here but lives on in
    // the returned BundleVfs for the whole peek session, so a failed unlock
    // must fail the peek through the same typed mapping rather than be
    // silently dropped.
    map_lock_result(fs4::FileExt::unlock(file), path)?;
    Ok(())
}
