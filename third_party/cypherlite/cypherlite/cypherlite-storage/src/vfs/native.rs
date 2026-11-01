use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use cypherlite_core::file_identity::nofollow;
use cypherlite_core::{Error, Result};

use super::{Vfs, VfsFile, VfsMeta, VfsOpen};

pub(crate) mod lease;

/// The one classification of a native entry's on-disk shape — the
/// plain file / plain directory / symlink-or-reparse / other fact
/// every native guard consults, matched as a typed kind instead of
/// re-composed per-site boolean conjuncts (lstat semantics: the entry's
/// own shape, never its target's).
///
/// Distinct from [`crate::vfs::VfsEntryKind`], the virtual-namespace
/// shape [`VfsMeta`] reports: this one classifies host `fs::Metadata`.
/// The two vocabularies stay aligned — each `Other` shape the native
/// classifier keeps is reported through [`VfsEntryKind::Other`], never
/// collapsed into the symlink refusal kind.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeEntryKind {
    /// A plain directory: a directory that is not itself a symlink,
    /// junction, or other reparse point.
    Dir,
    /// A plain regular file.
    File,
    /// A symlink, junction, or other reparse point.
    Alias,
    /// Any other entry shape (FIFO, socket, device): never admitted
    /// where a plain file or directory shape is required.
    Other,
}

/// Classify `meta`'s entry shape — the one classifier; [`plain_entry`]
/// and every native guard derive from it, never a second spelling.
pub(crate) fn native_entry_kind(meta: &fs::Metadata) -> NativeEntryKind {
    if is_alias(meta) {
        NativeEntryKind::Alias
    } else if meta.is_dir() {
        NativeEntryKind::Dir
    } else if meta.is_file() {
        NativeEntryKind::File
    } else {
        NativeEntryKind::Other
    }
}
/// The Windows `FILE_ATTRIBUTE_REPARSE_POINT` bit (symlinks, junctions,
/// and other reparse indirections) — the one spelling of the alias bit
/// both the classifier and the ownership scan consult.
#[cfg(windows)]
pub(crate) const REPARSE_POINT: u32 = 0x0400;

/// Whether `meta` names a symlink or Windows reparse point — the alias
/// half of [`native_entry_kind`]'s classification, spelled once.
fn is_alias(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_type().is_symlink() || meta.file_attributes() & REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    meta.file_type().is_symlink()
}

/// Whether `meta` is a plain entry — a regular file or directory that is
/// not itself a symlink/junction/reparse point (or any other non-plain
/// shape). Derived from [`native_entry_kind`].
pub(crate) fn plain_entry(meta: &fs::Metadata) -> bool {
    !matches!(
        native_entry_kind(meta),
        NativeEntryKind::Alias | NativeEntryKind::Other
    )
}

/// Refuse an open handle whose entry is not a plain regular file: the
/// one regular-file check over an open handle, expressed through the
/// [`native_entry_kind`] classifier (symlink, directory, reparse point,
/// and any other shape all refuse identically).
pub(crate) fn regular_handle(file: &File, path: &Path) -> Result<()> {
    let meta = file.metadata()?;
    if native_entry_kind(&meta) != NativeEntryKind::File {
        return Err(Error::storage(format!(
            "{} is not a regular file (symlinks refused)",
            path.display()
        )));
    }
    Ok(())
}

pub struct NativeVfs {
    root: PathBuf,
    _lease: Option<Arc<lease::NativeLease>>,
}

impl NativeVfs {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            _lease: None,
        }
    }

    pub(crate) fn with_lease(root: PathBuf, lease: Arc<lease::NativeLease>) -> Self {
        Self {
            root,
            _lease: Some(lease),
        }
    }

    /// Create `dir` (and missing ancestors) after refusing a symlinked
    /// directory anywhere below the store root — the one write-surface
    /// preparation shared by `create_dir_all`, atomic parent creation,
    /// `open`-with-create, and `rename`.
    fn create_dir_below_root(&self, dir: &Path) -> Result<()> {
        self.refuse_symlinked_below_root(dir)?;
        Self::create_private_dir_all(dir)
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

    /// Refuse a `path` whose existing directory chain below this VFS's
    /// root runs through a symlink/junction/reparse point, before
    /// anything is created beneath it — the write-surface twin of
    /// `validate_dir`'s read-side refusal. Ancestors at or above the
    /// store root are the operator's own filesystem and are not policed
    /// here.
    fn refuse_symlinked_below_root(&self, path: &Path) -> Result<()> {
        if path == self.root || !path.starts_with(&self.root) {
            return Ok(());
        }
        // `path` is a proper component-descendant of the store root (the
        // guard above refused everything else, and callers pass `resolve`
        // output, which rejects `..`), so the ancestor walk below runs
        // strictly between `path` and the root and always terminates at
        // the root — it never runs off the top of the tree.
        let mut cursor = path
            .parent()
            .expect("a path below the store root has a parent")
            .to_path_buf();
        while cursor != self.root {
            match fs::symlink_metadata(&cursor) {
                Ok(meta) if matches!(native_entry_kind(&meta), NativeEntryKind::Dir) => {}
                Ok(_) => {
                    return Err(Error::storage(format!(
                        "{} is not a regular directory (symlinks refused)",
                        cursor.display()
                    )))
                }
                // A missing ancestor is about to be created; ancestors
                // above it still need the check, so keep walking up.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            cursor = cursor
                .parent()
                .expect("an ancestor below the store root has a parent")
                .to_path_buf();
        }
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

    /// The create-or-existing arm of [`Vfs::open`]: an existing final
    /// component must be a regular file — a symlink or directory is refused
    /// before it can be truncated or appended through — a missing file is
    /// created exclusively, and an entry raced into the create window is
    /// adopted only after the same regular-file recheck.
    fn open_create_or_existing(p: &Path, o: &mut OpenOptions) -> Result<File> {
        // An existing entry of any non-regular shape (symlink, directory,
        // FIFO) is refused by the one canonical check inside
        // `open_checked_existing`, which every existing-open path shares.
        match fs::symlink_metadata(p) {
            Ok(_) => Self::open_checked_existing(p, o),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                o.create_new(true);
                match o.open(p) {
                    Ok(file) => Ok(file),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        o.create_new(false);
                        Self::open_checked_existing(p, o)
                    }
                    Err(e) => Err(e.into()),
                }
            }
            Err(e) => Err(e.into()),
        }
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
        let dir = self.resolve(path)?;
        self.create_dir_below_root(&dir)
    }

    fn create_dir_all_for_atomic(&self, path: &str) -> Result<()> {
        // The same below-root refusal as create_dir_below_root; the
        // creation itself is the durable per-component host form.
        let dir = self.resolve(path)?;
        self.refuse_symlinked_below_root(&dir)?;
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
        // lstat semantics: an entry reports its own shape, never its
        // target's — symlinked files are refused at open, and a walker
        // must not classify (let alone recurse into or write through) a
        // symlinked directory as a directory.
        let meta = fs::symlink_metadata(self.resolve(path)?)?;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64);
        Ok(match native_entry_kind(&meta) {
            NativeEntryKind::Dir => VfsMeta::dir(),
            NativeEntryKind::File => VfsMeta::file(meta.len(), mtime),
            // Each non-plain shape reports its own variant — the shape the
            // classifier actually decided, never a collapse: aliases under
            // the symlink refusal kind, any other shape (FIFO, socket,
            // device) under the other kind. Both carry the entry's own
            // length, and every consumer refuses both before any read,
            // write, or delete through them.
            NativeEntryKind::Alias => VfsMeta::symlink(meta.len()),
            NativeEntryKind::Other => VfsMeta::other(meta.len()),
        })
    }

    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>> {
        let p = self.resolve(path)?;
        if opts.create || opts.create_new {
            if let Some(parent) = p.parent() {
                self.create_dir_below_root(parent)?;
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
            Self::open_create_or_existing(&p, &mut o)?
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
        // The source is confirmed before the destination's parent is
        // created, so a missing source leaves no new empty directory.
        // `metadata` (stat) keeps `Path::exists`'s follow-symlink
        // semantics — a dangling symlink still reads as not-found — while
        // an unstatable source (EACCES on the parent, …) propagates
        // instead of folding into the not-found refusal.
        let source = self.resolve(from)?;
        match fs::metadata(&source) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::io(format!(
                    "rename source not found: {}",
                    source.display()
                )));
            }
            Err(e) => return Err(e.into()),
        }
        let dest = self.resolve(to)?;
        if let Some(parent) = dest.parent() {
            self.create_dir_below_root(parent)?;
        }
        fs::rename(source, dest)?;
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
    _lease: Option<Arc<lease::NativeLease>>,
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
#[path = "native/tests_unix.rs"]
mod tests;

#[cfg(all(test, windows))]
#[path = "native/tests_windows.rs"]
mod windows_tests;
