use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use cypherlite_core::file_identity::{self, nofollow};
use cypherlite_core::{Error, Result};
use fs4::FileExt;

use super::{Vfs, VfsFile, VfsMeta, VfsOpen};

pub struct NativeVfs {
    root: PathBuf,
    _lease: Option<Arc<NativeLease>>,
}

/// Keep both identities alive for every cloned Native VFS and WAL handle.
///
/// Writable opens also hold the exclusive `.cypherlite.writer` lease
/// (ADR-0136), the same [`crate::WriterLease`] a later `promote` takes.
/// Read-only opens leave `writer` empty. `Database::open` moves it onto
/// the engine handle so `demote` can drop it without closing the store.
pub(crate) struct NativeLease {
    writer: Mutex<Option<crate::WriterLease>>,
    pub(crate) _root: File,
    pub(crate) _wal: Option<File>,
}

impl NativeLease {
    pub(crate) fn new(root: File, wal: Option<File>, writer: Option<crate::WriterLease>) -> Self {
        Self {
            writer: Mutex::new(writer),
            _root: root,
            _wal: wal,
        }
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
        for file in [self._wal.as_ref(), Some(&self._root)]
            .into_iter()
            .flatten()
        {
            let _ = FileExt::unlock(file);
        }
    }
}

fn regular_handle(file: &File, path: &Path) -> Result<()> {
    let meta = file.metadata()?;
    let kind = meta.file_type();
    #[cfg(windows)]
    let reparse_point = {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x0400 != 0 // FILE_ATTRIBUTE_REPARSE_POINT
    };
    #[cfg(not(windows))]
    let reparse_point = false;
    if !kind.is_file() || kind.is_symlink() || reparse_point {
        return Err(Error::storage(format!(
            "{} is not a regular file (symlinks refused)",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn plain_entry(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        !meta.file_type().is_symlink() && meta.file_attributes() & 0x0400 == 0
    }
    #[cfg(not(windows))]
    {
        !meta.file_type().is_symlink()
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

impl NativeVfs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            _lease: None,
        }
    }

    pub(crate) fn with_lease(root: PathBuf, lease: Arc<NativeLease>) -> Self {
        Self {
            root,
            _lease: Some(lease),
        }
    }

    pub(crate) fn lock_directory(path: &Path) -> Result<(PathBuf, File)> {
        Self::lock_directory_mode(path, true)
    }

    /// Shared identity lock for a live opener (ADR-0136). Still creates a
    /// missing marker so a writable open can adopt the directory. Reset and
    /// adoption stay on [`Self::lock_directory`], which takes `LOCK_EX`.
    pub(crate) fn lock_directory_shared(path: &Path) -> Result<(PathBuf, File)> {
        Self::lock_directory_mode(path, false)
    }

    fn lock_directory_mode(path: &Path, exclusive: bool) -> Result<(PathBuf, File)> {
        Self::create_private_dir_all(path)?;
        let path = fs::canonicalize(path)?;
        if !fs::metadata(&path)?.is_dir() {
            return Err(Error::storage(format!(
                "{} is not a directory",
                path.display()
            )));
        }
        let marker = path.join(crate::native_ownership::LOCK_MARKER);
        let file = Self::lock_file(&marker, OpenAdmit::OpenOrCreate, exclusive)?;
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
            || !plain_entry(&current)
            || !current.is_file()
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
        Self::lock_file(path, OpenAdmit::OpenOrCreate, true)
    }

    /// Shared so role preflight does not contend with live openers, which
    /// all hold the identity marker shared (ADR-0136). Reset and adoption
    /// take their exclusive lease afterwards.
    pub(crate) fn lock_existing_regular_file(path: &Path) -> Result<PreflightLease> {
        Ok(PreflightLease(Self::lock_file(
            path,
            OpenAdmit::Existing,
            false,
        )?))
    }

    /// Exclusive lock on `.cypherlite.writer` (ADR-0136). Creates the file
    /// when it is missing. Callers hold the shared identity lock already.
    pub(crate) fn lock_writer_lease(dir: &Path) -> Result<File> {
        Self::lock_file(
            &dir.join(crate::native_ownership::WRITER_LEASE),
            OpenAdmit::OpenOrCreate,
            true,
        )
    }

    fn lock_file(path: &Path, admit: OpenAdmit, exclusive: bool) -> Result<File> {
        let file = Self::open_regular_file(path, admit)?;
        let locked = if exclusive {
            FileExt::try_lock_exclusive(&file)
        } else {
            FileExt::try_lock_shared(&file)
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
        if !meta.file_type().is_file() {
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

    pub(crate) fn create_private_dir_all(path: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(path)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(path)?;
        Ok(())
    }

    /// Report only creation of the final directory component; an existing
    /// (even just raced-in) child must still prove its legacy WAL identity.
    pub(crate) fn create_private_dir(path: &Path) -> Result<bool> {
        if let Some(parent) = path.parent() {
            Self::create_private_dir_all(parent)?;
        }
        #[cfg(unix)]
        let created = fs::DirBuilder::new().mode(0o700).create(path);
        #[cfg(not(unix))]
        let created = fs::create_dir(path);
        match created {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// [`create_private_dir`] that refuses a pre-existing final component
    /// (ADR-0161): a directory, file, or symlink already there — including
    /// one raced into the create window — fails with a
    /// destination-already-exists storage error instead of being adopted.
    /// Missing parents are still created recursively.
    pub(crate) fn create_private_dir_exclusive(path: &Path) -> Result<()> {
        if Self::create_private_dir(path)? {
            Ok(())
        } else {
            Err(crate::destination_exists(path))
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn resolve(&self, virt: &str) -> Result<PathBuf> {
        if virt.is_empty() {
            return Ok(self.root.clone());
        }
        let mut out = self.root.clone();
        for part in virt.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            if part == ".." || part.contains('\\') {
                return Err(Error::storage(format!("invalid virtual path {virt}")));
            }
            out.push(part);
        }
        Ok(out)
    }

    #[cfg(unix)]
    fn sync_host_dir(path: &Path) -> Result<()> {
        File::open(path)?.sync_all()?;
        Ok(())
    }

    #[cfg(unix)]
    fn create_dir_all_for_atomic_host(path: &Path) -> Result<()> {
        let mut missing = Vec::new();
        let mut cursor = path;
        while !cursor.exists() {
            if cursor == Path::new(".") || cursor == Path::new("/") {
                return Err(Error::io(format!(
                    "cannot access directory {}",
                    cursor.display()
                )));
            }
            missing.push(cursor.to_path_buf());
            cursor = cursor
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
        }
        for dir in missing.iter().rev() {
            let created = fs::DirBuilder::new().mode(0o700).create(dir);
            match created {
                Ok(()) => {
                    let parent = dir
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or(Path::new("."));
                    Self::sync_host_dir(parent)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => (),
                Err(e) => return Err(e.into()),
            }
        }
        // Catch an existing non-directory as well as a raced-in one.
        Self::create_private_dir_all(path)?;
        Ok(())
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

impl Vfs for NativeVfs {
    fn durable(&self) -> bool {
        true
    }

    fn take_writer_lease(&self) -> Option<crate::WriterLease> {
        self._lease.as_ref().and_then(|lease| lease.take_writer())
    }

    fn create_dir_all(&self, path: &str) -> Result<()> {
        Self::create_private_dir_all(&self.resolve(path)?)?;
        Ok(())
    }

    fn create_dir_all_for_atomic(&self, path: &str) -> Result<()> {
        let dir = self.resolve(path)?;
        #[cfg(unix)]
        return Self::create_dir_all_for_atomic_host(&dir);
        #[cfg(not(unix))]
        {
            fs::create_dir_all(dir)?;
            Ok(())
        }
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>> {
        let dir = self.resolve(path)?;
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut names = Vec::new();
        for ent in fs::read_dir(&dir)? {
            let ent = ent?;
            names.push(ent.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    fn exists(&self, path: &str) -> Result<bool> {
        Ok(self.resolve(path)?.exists())
    }

    fn metadata(&self, path: &str) -> Result<VfsMeta> {
        let meta = fs::metadata(self.resolve(path)?)?;
        Ok(VfsMeta {
            len: meta.len(),
            is_dir: meta.is_dir(),
        })
    }

    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>> {
        let p = self.resolve(path)?;
        if opts.create || opts.create_new {
            if let Some(parent) = p.parent() {
                Self::create_private_dir_all(parent)?;
            }
        }
        let mut o = OpenOptions::new();
        o.read(opts.read)
            .write(opts.write)
            .create_new(opts.create_new);
        nofollow(&mut o);
        #[cfg(unix)]
        o.mode(0o600);
        if opts.append {
            o.append(true);
        }
        // An existing final-component symlink must never be truncated or
        // appended through. A missing file is created exclusively.
        let file = if opts.create && !opts.create_new {
            match fs::symlink_metadata(&p) {
                Ok(meta) if !meta.file_type().is_file() => {
                    return Err(Error::storage(format!(
                        "{} is not a regular file (symlinks refused)",
                        p.display()
                    )));
                }
                Ok(_) => Self::open_checked_existing(&p, &o)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    o.create_new(true);
                    match o.open(&p) {
                        Ok(file) => file,
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                            o.create_new(false);
                            Self::open_checked_existing(&p, &o)?
                        }
                        Err(e) => return Err(e.into()),
                    }
                }
                Err(e) => return Err(e.into()),
            }
        } else if opts.create_new {
            o.open(&p)?
        } else {
            Self::open_checked_existing(&p, &o)?
        };
        regular_handle(&file, &p)?;
        if opts.truncate {
            file.set_len(0)?;
        }
        Ok(Box::new(NativeFile {
            file,
            _lease: self._lease.clone(),
        }))
    }

    fn open_exclusive(&self, path: &str) -> Result<Option<Box<dyn VfsFile>>> {
        let p = self.resolve(path)?;
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        match opts.open(p) {
            Ok(file) => Ok(Some(Box::new(NativeFile {
                file,
                _lease: self._lease.clone(),
            }))),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        match fs::remove_file(self.resolve(path)?) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn remove_dir_all(&self, path: &str) -> Result<()> {
        let p = self.resolve(path)?;
        match fs::remove_dir_all(&p) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn rename(&self, from: &str, to: &str) -> Result<()> {
        let dest = self.resolve(to)?;
        if let Some(parent) = dest.parent() {
            Self::create_private_dir_all(parent)?;
        }
        fs::rename(self.resolve(from)?, dest)?;
        Ok(())
    }

    fn sync_dir(&self, path: &str) -> Result<()> {
        #[cfg(unix)]
        return Self::sync_host_dir(&self.resolve(path)?);
        // A read-only directory handle is not a verified FlushFileBuffers
        // barrier on Windows. The file itself was synced before the rename.
        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(())
        }
    }
}

struct NativeFile {
    file: File,
    // A caller can retain a writable file after dropping its StoreTarget/VFS.
    // Keep both root and WAL locks until this last mutable handle is gone.
    _lease: Option<Arc<NativeLease>>,
}

impl VfsFile for NativeFile {
    fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.file.write_all(buf)?;
        Ok(())
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.file.read_exact(buf)?;
        Ok(())
    }

    fn seek(&mut self, pos: SeekFrom) -> Result<u64> {
        Ok(self.file.seek(pos)?)
    }

    fn sync_all(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }

    fn durability_barrier(&mut self) -> Result<()> {
        crate::vfs::host::host_durability_barrier(&self.file)
    }

    fn set_len(&mut self, len: u64) -> Result<()> {
        self.file.set_len(len)?;
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn checked_existing_refuses_symlink_swapped_after_inspection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let victim = dir.path().join("victim");
        fs::write(&path, b"original").unwrap();
        fs::write(&victim, b"keep").unwrap();
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        nofollow(&mut options);
        assert!(NativeVfs::open_checked_existing_with(&path, &options, || {
            fs::remove_file(&path).unwrap();
            symlink(&victim, &path).unwrap();
        })
        .is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"keep");

        let vfs = NativeVfs::new(dir.path());
        assert!(vfs.open("lock", VfsOpen::create_truncate()).is_err());
        assert_eq!(fs::read(victim).unwrap(), b"keep");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn checked_existing_refuses_swapped_reparse_point() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let victim = dir.path().join("victim");
        fs::write(&path, b"original").unwrap();
        fs::write(&victim, b"keep").unwrap();
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        nofollow(&mut options);
        let mut symlink_supported = true;
        let result = NativeVfs::open_checked_existing_with(&path, &options, || {
            fs::remove_file(&path).unwrap();
            if let Err(e) = std::os::windows::fs::symlink_file(&victim, &path) {
                // Some Windows runners do not grant the symlink privilege.
                assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
                symlink_supported = false;
            }
        });
        if symlink_supported {
            assert!(result.is_err());
            let vfs = NativeVfs::new(dir.path());
            assert!(vfs.open("lock", VfsOpen::create_truncate()).is_err());
        }
        assert_eq!(fs::read(victim).unwrap(), b"keep");
    }
}
