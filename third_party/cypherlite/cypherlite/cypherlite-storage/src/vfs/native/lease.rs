//! Lease, locking, and identity half of the Native VFS (ADR-0072/0136).
//!
//! Everything that opens, locks, or verifies a host file by inode identity
//! lives here: the shared/exclusive marker locks behind open and reset, the
//! writer lease, and the one regular-file open primitive (ADR-0161) every
//! lock taker shares. The entry-shape classifiers
//! ([`native_entry_kind`](super::native_entry_kind)/[`plain_entry`](super::plain_entry)/
//! [`regular_handle`](super::regular_handle)) and the write-surface half
//! stay in `native.rs` — the lowest module every consumer shares
//! (the no-1k-line rule, LABS-311).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use cypherlite_core::file_identity::{self, nofollow};
use cypherlite_core::{Error, Result};
use fs4::FileExt;

use super::NativeVfs;
use super::{native_entry_kind, regular_handle, NativeEntryKind};

/// Keep both identities alive for every cloned Native VFS and WAL handle.
///
/// Writable opens also hold the exclusive `.cypherlite.writer` lease
/// (ADR-0136), the same [`crate::WriterLease`] a later `promote` takes.
/// Read-only opens leave `writer` empty. `Database::open` moves it onto
/// the engine handle so `demote` can drop it without closing the store.
pub(crate) struct NativeLease {
    writer: Mutex<Option<crate::WriterLease>>,
    root: File,
    wal: Option<File>,
}

impl NativeLease {
    pub(crate) fn new(root: File, wal: Option<File>, writer: Option<crate::WriterLease>) -> Self {
        Self {
            writer: Mutex::new(writer),
            root,
            wal,
        }
    }

    /// The locked root marker's owning handle — the `LeaseBuild`
    /// precedent: held for its lock, read through this accessor, never a
    /// crate-visible field. Production holds the lock for the handle's
    /// lifetime; the marker readers are tests (and future under-lease
    /// checks), so the accessor carries the same test-seam allowance
    /// [`Self::lock_regular_file`] does.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn root_file(&self) -> &File {
        &self.root
    }

    /// The locked WAL marker's owning handle; `None` when the WAL
    /// directory is the root.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn wal_file(&self) -> Option<&File> {
        self.wal.as_ref()
    }

    /// Hand the writer lease to the caller once. A second call is `None`.
    pub(crate) fn take_writer(&self) -> Option<crate::WriterLease> {
        self.writer
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take()
    }
}

// Temporary preflight locks need the same explicit unlock as long-lived
// leases: closing a descriptor alone can delay immediate reacquisition on macOS.
pub(crate) struct PreflightLease(File);

impl std::ops::Deref for PreflightLease {
    type Target = File;

    fn deref(&self) -> &File {
        &self.0
    }
}

impl Drop for PreflightLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

impl Drop for NativeLease {
    fn drop(&mut self) {
        // An explicit unlock finishes the ownership handoff before the last
        // lease returns from Drop. On macOS, closing the descriptor alone can
        // leave a concurrent immediate reopen seeing EWOULDBLOCK briefly.
        // Release the writer lease first so a waiting writer can proceed once
        // the shared identity locks drop.
        drop(
            self.writer
                .get_mut()
                .unwrap_or_else(|err| err.into_inner())
                .take(),
        );
        for file in [self.wal.as_ref(), Some(&self.root)].into_iter().flatten() {
            let _ = FileExt::unlock(file);
        }
    }
}

/// How [`NativeVfs::open_regular_file`] admits the destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenAdmit {
    /// Open an existing regular file; never create.
    Existing,
    /// Create when missing; adopt the existing file otherwise.
    OpenOrCreate,
    /// Create exclusively (ADR-0161): an existing entry fails with a
    /// destination-already-exists storage error instead of being adopted.
    CreateExclusive,
}

/// Which OS lock a lock taker holds on its file — the typed mode behind
/// every [`NativeVfs::lock_file`] call, never a bare `bool` (mirrors
/// [`OpenAdmit`] one module up).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LockMode {
    /// `LOCK_EX`: the admission half — reset, offline adoption, the
    /// writer lease, and the bundle preflight tests.
    Exclusive,
    /// `LOCK_SH`: the shared identity lock a live opener or a role
    /// preflight holds (ADR-0136).
    Shared,
}

impl NativeVfs {
    /// Take the exclusive identity lock on `path`'s `marker` (the
    /// admission half: reset and offline adoption). The marker filename
    /// comes from the policy owner — `native_ownership` — at the call
    /// site; this layer never hard-codes which file constitutes the
    /// identity mark.
    pub(crate) fn lock_directory(path: &Path, marker: &str) -> Result<(PathBuf, File)> {
        Self::lock_directory_mode(path, LockMode::Exclusive, marker)
    }

    /// Shared identity lock for a live opener (ADR-0136). Still creates a
    /// missing marker so a writable open can adopt the directory. Reset and
    /// adoption stay on [`Self::lock_directory`], which takes `LOCK_EX`.
    pub(crate) fn lock_directory_shared(path: &Path, marker: &str) -> Result<(PathBuf, File)> {
        Self::lock_directory_mode(path, LockMode::Shared, marker)
    }

    fn lock_directory_mode(path: &Path, mode: LockMode, marker: &str) -> Result<(PathBuf, File)> {
        Self::create_private_dir_all(path)?;
        let path = fs::canonicalize(path)?;
        if !fs::metadata(&path)?.is_dir() {
            return Err(Error::storage(format!(
                "{} is not a directory",
                path.display()
            )));
        }
        // The marker is joined onto the canonical directory, so the locked
        // inode is exactly the one `check_identity` re-verifies against the
        // canonical path the caller receives.
        let file = Self::lock_file(&path.join(marker), OpenAdmit::OpenOrCreate, mode)?;
        Ok((path, file))
    }

    /// Shared lock on an existing marker, opened read-only. Does not create
    /// the file (ADR-0136).
    pub(crate) fn lock_existing_shared_readonly(path: &Path) -> Result<File> {
        let mut opts = OpenOptions::new();
        opts.read(true);
        nofollow(&mut opts);
        let file = Self::open_checked_existing(path, &opts)?;
        map_lock_result(FileExt::try_lock_shared(&file), path)?;
        Ok(file)
    }

    /// A reset may rename a parent just as an opener creates its own marker.
    /// Refuse a handle that no longer names the path we intend to open.
    pub(crate) fn check_identity(file: &File, path: &Path) -> Result<()> {
        let current = fs::symlink_metadata(path)?;
        if file_identity::of_file(file)? != file_identity::of_path(path)?
            || native_entry_kind(&current) != NativeEntryKind::File
        {
            return Err(Error::storage(format!(
                "storage identity changed while opening {}",
                path.display()
            )));
        }
        Ok(())
    }

    /// Open an existing regular file without truncation or exclusively create it.
    /// Canonicalize supported aliases first; never open a symlink as the final
    /// lock target. The returned file itself owns the OS lock.
    /// Exclusive lock used by bundle preflight tests. Live opens go through
    /// [`Self::lock_directory`] / [`Self::lock_directory_shared`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn lock_regular_file(path: &Path) -> Result<File> {
        Self::lock_file(path, OpenAdmit::OpenOrCreate, LockMode::Exclusive)
    }

    /// Shared so role preflight does not contend with live openers, which
    /// all hold the identity marker shared (ADR-0136). Reset and adoption
    /// take their exclusive lease afterwards.
    pub(crate) fn lock_existing_regular_file(path: &Path) -> Result<PreflightLease> {
        Ok(PreflightLease(Self::lock_file(
            path,
            OpenAdmit::Existing,
            LockMode::Shared,
        )?))
    }

    /// Exclusive lock on the writer lease file beside an already-held
    /// shared identity lock (ADR-0136). Creates the file when it is
    /// missing. The lease filename comes from the policy owner —
    /// `native_ownership::take_writer_lease` — at the call site; this
    /// layer never hard-codes which file the writer lease locks.
    pub(crate) fn lock_writer_lease(dir: &Path, lease: &str) -> Result<File> {
        Self::lock_file(
            &dir.join(lease),
            OpenAdmit::OpenOrCreate,
            LockMode::Exclusive,
        )
    }

    fn lock_file(path: &Path, admit: OpenAdmit, mode: LockMode) -> Result<File> {
        let file = Self::open_regular_file(path, admit)?;
        let locked = match mode {
            LockMode::Exclusive => FileExt::try_lock_exclusive(&file),
            LockMode::Shared => FileExt::try_lock_shared(&file),
        };
        if let Err(e) = map_lock_result(locked, path) {
            let _ = FileExt::unlock(&file);
            return Err(e);
        }
        Ok(file)
    }

    /// Write `bytes` while `file` holds `LOCK_SH` (ADR-0136).
    ///
    /// A Windows shared `LockFileEx` rejects `WriteFile` on every handle,
    /// including the owner. The write takes a brief exclusive lock and then
    /// restores the shared one. Unix `flock` lets the owner write under
    /// `LOCK_SH`.
    pub(crate) fn write_under_shared_lock(file: &File, path: &Path, bytes: &[u8]) -> Result<()> {
        #[cfg(windows)]
        {
            FileExt::unlock(file)?;
            if let Err(e) = map_lock_result(FileExt::try_lock_exclusive(file), path) {
                let _ = FileExt::try_lock_shared(file);
                return Err(e);
            }
            let wrote = (&*file).write_all(bytes).and_then(|_| file.sync_all());
            let _ = FileExt::unlock(file);
            map_lock_result(FileExt::try_lock_shared(file), path)?;
            // The exclusive upgrade unlocked the marker. A reset can rename
            // the directory in that window; refuse the lease if the handle
            // no longer names `path`.
            Self::check_identity(file, path)?;
            wrote?;
            return Ok(());
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            (&*file).write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        }
    }

    pub(crate) fn read_locked_marker(file: &File, path: &Path) -> Result<Vec<u8>> {
        Self::check_identity(file, path)?;
        let mut handle = file;
        handle.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        handle.take(128).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Open or create `path` as a regular file under `admit`. The
    /// created-vs-adopted distinction lives in this one open primitive:
    /// every caller shares the same `OpenOptions` (read+write, nofollow,
    /// 0600) and the same `AlreadyExists` dispatch instead of carrying a
    /// parallel copy (ADR-0161). The returned file itself owns any OS lock
    /// the caller takes on it.
    pub(crate) fn open_regular_file(path: &Path, admit: OpenAdmit) -> Result<File> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        nofollow(&mut opts);
        #[cfg(unix)]
        opts.mode(0o600);
        let file = match admit {
            OpenAdmit::Existing => Self::open_existing_regular(path)?,
            OpenAdmit::OpenOrCreate => match opts.open(path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Self::open_existing_regular(path)?
                }
                Err(e) => return Err(e.into()),
            },
            // An exclusive create adopts nothing: any `create_new` failure
            // is arbitrated by `symlink_metadata` — if ANYTHING occupies the
            // path (file, directory, or symlink, dangling included; a symlink
            // raced into the create window is one `nofollow`+`create_new`
            // never follows), the refusal is destination-already-exists.
            // Only a genuinely absent path propagates the original I/O error
            // (permissions, a `NotADirectory` parent, …). Matching on
            // `ErrorKind` alone is not portable: Windows fails `create_new`
            // over a directory with `AccessDenied`, not `IsADirectory`, and a
            // generic I/O error here would let publish cleanup
            // `remove_dir_all` a directory this run never created (ADR-0161).
            OpenAdmit::CreateExclusive => match opts.open(path) {
                Ok(file) => file,
                Err(e) => {
                    if fs::symlink_metadata(path).is_ok() {
                        return Err(crate::destination_exists(path));
                    }
                    return Err(e.into());
                }
            },
        };
        regular_handle(&file, path)?;
        Ok(file)
    }

    fn open_existing_regular(path: &Path) -> Result<File> {
        let mut opts = OpenOptions::new();
        opts.read(true).write(true);
        nofollow(&mut opts);
        Self::open_checked_existing(path, &opts)
    }

    /// Open an existing regular file after refusing a final-component
    /// symlink by pre-checking its metadata.
    pub(crate) fn open_checked_existing(path: &Path, opts: &OpenOptions) -> Result<File> {
        Self::open_checked_existing_inner(path, opts, || {})
    }

    // The callback is a deterministic test seam between inspection and open.
    // Kernel no-follow and post-open handle validation, not the precheck,
    // enforce the identity actually opened.
    fn open_checked_existing_inner(
        path: &Path,
        opts: &OpenOptions,
        before_open: impl FnOnce(),
    ) -> Result<File> {
        let meta = fs::symlink_metadata(path)?;
        if native_entry_kind(&meta) != NativeEntryKind::File {
            return Err(Error::storage(format!(
                "{} is not a regular file (symlinks refused)",
                path.display()
            )));
        }
        before_open();
        let file = opts.open(path)?;
        regular_handle(&file, path)?;
        Ok(file)
    }

    /// Test-only variant of [`Self::open_checked_existing`] that runs
    /// `before_open` after the metadata inspection and before the open, so a
    /// test can race a symlink swap deterministically.
    #[cfg(test)]
    pub(crate) fn open_checked_existing_with<F: FnOnce()>(
        path: &Path,
        opts: &OpenOptions,
        before_open: F,
    ) -> Result<File> {
        Self::open_checked_existing_inner(path, opts, before_open)
    }
}

/// Map a `try_lock_*`/`unlock` outcome to a typed result: success passes
/// through and lock contention becomes [`Error::Locked`]; every other I/O
/// failure is returned unchanged. Shared by the Native and `.cl` bundle hosts,
/// both of which take an OS exclusive lock on the same marker file.
pub(crate) fn map_lock_result(result: std::io::Result<()>, path: &Path) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == fs4::lock_contended_error().raw_os_error() => {
            Err(Error::Locked(path.display().to_string()))
        }
        Err(e) => Err(e.into()),
    }
}
