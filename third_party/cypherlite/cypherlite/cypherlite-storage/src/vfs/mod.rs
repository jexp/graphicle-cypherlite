//! Synchronous virtual filesystem for WAL, snapshots, and CDC (ADR-0072).

#[cfg(not(target_arch = "wasm32"))]
mod bundle;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod host;
mod memory;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod native;

use std::fmt::Write as _;
use std::io::{self, SeekFrom, Write};

use cypherlite_core::{Error, Result};

#[cfg(not(target_arch = "wasm32"))]
pub use bundle::BundleVfs;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use bundle::HostAdmit;
pub use memory::MemoryVfs;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::lease::NativeLease;
#[cfg(not(target_arch = "wasm32"))]
pub use native::NativeVfs;

pub fn is_bundle_path(path: &std::path::Path) -> bool {
    let raw = path.as_os_str().to_string_lossy();
    let trimmed = raw.trim_end_matches(['/', '\\']);
    std::path::Path::new(trimmed)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cl"))
}

/// The entry's own shape (lstat semantics: the entry itself, never the
/// target of a symlink) — one typed fact instead of two independent bools
/// whose other combinations are nonsense. Backends construct the variant;
/// walkers match it totally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VfsEntryKind {
    File,
    Dir,
    /// A symlink/junction/reparse point. Walkers refuse such entries
    /// before any read, write, or delete through them.
    Symlink,
    /// Any other host entry shape (FIFO, socket, device): never admitted
    /// where a plain file or directory shape is required. The native
    /// backend reports it; the virtual backends never construct it.
    Other,
}

/// The entry shapes `Vfs::metadata` reports —
/// [`File`](Self::File), [`Dir`](Self::Dir),
/// [`Symlink`](Self::Symlink) (junction/reparse included), and
/// [`Other`](Self::Other) (FIFO, socket, device: never admitted where a
/// plain shape is required). The native backend classifies all four;
/// the virtual backends construct only `File` and `Dir`.
///
/// ```
/// use cypherlite_storage::{VfsEntryKind, VfsMeta};
///
/// // The one spelling of each entry shape, and the derived predicates
/// // a walker consults.
/// let note = VfsMeta::file(42, None);
/// assert_eq!(note.kind, VfsEntryKind::File);
/// assert!(note.is_dir() == false && note.is_symlink() == false && note.is_other() == false);
///
/// let fifo = VfsMeta::other(0);
/// assert_eq!(fifo.kind, VfsEntryKind::Other);
/// assert!(fifo.is_other());
/// ```
pub struct VfsMeta {
    pub kind: VfsEntryKind,
    pub len: u64,
    /// The entry's modification time (nanoseconds since the epoch) when
    /// the backend reports one; virtual backends report `None`.
    pub mtime: Option<u64>,
}

impl VfsMeta {
    /// A regular file entry of `len` bytes with its modification time
    /// when the backend reports one.
    pub fn file(len: u64, mtime: Option<u64>) -> Self {
        Self {
            kind: VfsEntryKind::File,
            len,
            mtime,
        }
    }

    /// A plain directory entry.
    pub fn dir() -> Self {
        Self {
            kind: VfsEntryKind::Dir,
            len: 0,
            mtime: None,
        }
    }

    /// A symlink/junction entry reporting its own length, never its
    /// target's shape.
    pub fn symlink(len: u64) -> Self {
        Self {
            kind: VfsEntryKind::Symlink,
            len,
            mtime: None,
        }
    }

    /// A host entry of any other non-plain shape (FIFO, socket, device)
    /// reporting its own length. Consumers refuse it exactly like a
    /// symlink: never a read, write, or delete through it.
    pub fn other(len: u64) -> Self {
        Self {
            kind: VfsEntryKind::Other,
            len,
            mtime: None,
        }
    }

    /// Whether the entry is a directory — derived from [`Self::kind`],
    /// the one shape fact, never an independent bool.
    pub fn is_dir(&self) -> bool {
        self.kind == VfsEntryKind::Dir
    }

    /// Whether the entry itself is a symlink/junction/reparse point —
    /// derived from [`Self::kind`].
    pub fn is_symlink(&self) -> bool {
        self.kind == VfsEntryKind::Symlink
    }

    /// Whether the entry is neither a plain file, a directory, nor a
    /// symlink — a FIFO, socket, or device — derived from
    /// [`Self::kind`].
    pub fn is_other(&self) -> bool {
        self.kind == VfsEntryKind::Other
    }
}

/// Whether any virtual-namespace key sits beneath `path` — the one
/// directory-shape test the virtual backends share.
pub(crate) fn is_virtual_prefix<'a>(mut keys: impl Iterator<Item = &'a str>, path: &str) -> bool {
    let prefix = format!("{path}/");
    keys.any(|key| key.starts_with(&prefix))
}

/// Classify a virtual (non-durable) namespace entry: a file hit by exact
/// name, a directory when any entry sits beneath it, otherwise missing —
/// the one policy every virtual backend's `metadata` reports.
pub(crate) fn virtual_entry_meta(
    file_len: Option<u64>,
    is_prefix: bool,
    path: &str,
) -> Result<VfsMeta> {
    if let Some(len) = file_len {
        return Ok(VfsMeta::file(len, None));
    }
    if is_prefix {
        return Ok(VfsMeta::dir());
    }
    Err(Error::io(format!("missing {path}")))
}

#[derive(Clone, Copy)]
pub struct VfsOpen {
    pub create: bool,
    /// Atomically create a new file, even if a dangling symlink occupies the name.
    pub create_new: bool,
    pub truncate: bool,
    pub append: bool,
    pub write: bool,
    pub read: bool,
}

impl VfsOpen {
    pub fn create_truncate() -> Self {
        Self {
            create: true,
            create_new: false,
            truncate: true,
            append: false,
            write: true,
            read: true,
        }
    }

    pub fn append_create() -> Self {
        Self {
            create: true,
            create_new: false,
            truncate: false,
            append: true,
            write: true,
            read: false,
        }
    }

    pub fn read_only() -> Self {
        Self {
            create: false,
            create_new: false,
            truncate: false,
            append: false,
            write: false,
            read: true,
        }
    }

    pub fn read_write() -> Self {
        Self {
            create: false,
            create_new: false,
            truncate: false,
            append: false,
            write: true,
            read: true,
        }
    }

    pub fn create_new() -> Self {
        Self {
            create: false,
            create_new: true,
            truncate: false,
            append: false,
            write: true,
            read: true,
        }
    }
}

pub trait Vfs: Send + Sync {
    fn durable(&self) -> bool;
    fn create_dir_all(&self, path: &str) -> Result<()>;
    /// Create parents for an atomic write, persisting new directory entries if needed.
    /// Durable implementations must override this fail-closed default.
    fn create_dir_all_for_atomic(&self, path: &str) -> Result<()> {
        if self.durable() {
            return Err(Error::storage(
                "durable VFS must implement create_dir_all_for_atomic",
            ));
        }
        self.create_dir_all(path)
    }
    fn list_dir(&self, path: &str) -> Result<Vec<String>>;
    fn exists(&self, path: &str) -> Result<bool>;
    fn metadata(&self, path: &str) -> Result<VfsMeta>;
    fn open(&self, path: &str, opts: VfsOpen) -> Result<Box<dyn VfsFile>>;
    /// `None` means the name was occupied; the existing object was not opened.
    /// Implementations of `open` must honor `create_new` atomically. Native
    /// overrides this fallback to recognize dangling symlink collisions too.
    fn open_exclusive(&self, path: &str) -> Result<Option<Box<dyn VfsFile>>> {
        match self.open(path, VfsOpen::create_new()) {
            Ok(file) => Ok(Some(file)),
            Err(_) if self.exists(path)? => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn remove_file(&self, path: &str) -> Result<()>;
    /// Remove several files as one logical batch. The default removes each
    /// path in turn; durable implementations may publish the whole batch with
    /// a single catalog generation and barrier pair (Bundle). Callers validate
    /// every path before calling.
    fn remove_files(&self, paths: &[String]) -> Result<()> {
        for path in paths {
            self.remove_file(path)?;
        }
        Ok(())
    }
    fn remove_dir_all(&self, path: &str) -> Result<()>;
    fn rename(&self, from: &str, to: &str) -> Result<()>;
    /// Persist a renamed entry in this virtual directory. Durable implementations
    /// must override this fail-closed default (Bundle uses virtual barriers).
    fn sync_dir(&self, _path: &str) -> Result<()> {
        if self.durable() {
            Err(Error::storage("durable VFS must implement sync_dir"))
        } else {
            Ok(())
        }
    }

    /// Move this opener's `.cypherlite.writer` lease out, once.
    /// `None` for a read-only open, a non-native VFS, or a lease already taken.
    #[cfg(not(target_arch = "wasm32"))]
    fn take_writer_lease(&self) -> Option<crate::WriterLease> {
        None
    }
}

pub trait VfsFile: Send {
    fn write_all(&mut self, buf: &[u8]) -> Result<()>;
    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()>;
    fn seek(&mut self, pos: SeekFrom) -> Result<u64>;
    fn sync_all(&mut self) -> Result<()>;
    /// Cheapest correct durability barrier (ADR-0077). Defaults to [`sync_all`].
    fn durability_barrier(&mut self) -> Result<()> {
        self.sync_all()
    }
    fn set_len(&mut self, len: u64) -> Result<()>;
    fn len(&self) -> Result<u64>;

    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    fn stream_position(&mut self) -> Result<u64> {
        self.seek(SeekFrom::Current(0))
    }
}

/// A [`Write`] adapter over a VFS file which fails before a write would
/// exceed `limit`. It keeps Parquet checkpoint candidates bounded without
/// collecting their complete contents in heap memory.
#[cfg(any(test, parquet_codec))]
pub(crate) struct CappedVfsWriter<'a> {
    file: &'a mut dyn VfsFile,
    bytes: u64,
    limit: u64,
    exceeded: bool,
    message: &'static str,
}

#[cfg(any(test, parquet_codec))]
impl<'a> CappedVfsWriter<'a> {
    pub(crate) fn new(file: &'a mut dyn VfsFile, limit: u64, message: &'static str) -> Self {
        Self {
            file,
            bytes: 0,
            limit,
            exceeded: false,
            message,
        }
    }

    pub(crate) fn exceeded(&self) -> bool {
        self.exceeded
    }
}

#[cfg(any(test, parquet_codec))]
impl Write for CappedVfsWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Some(total) = self.bytes.checked_add(buf.len() as u64) else {
            self.exceeded = true;
            return Err(io::Error::other(self.message));
        };
        if total > self.limit {
            self.exceeded = true;
            return Err(io::Error::other(self.message));
        }
        self.file
            .write_all(buf)
            .map_err(|error| io::Error::other(error.to_string()))?;
        self.bytes = total;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn virt_parent(path: &str) -> Option<&str> {
    let i = path.rfind('/')?;
    if i == 0 {
        None
    } else {
        Some(&path[..i])
    }
}

pub(crate) fn virt_join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else if name.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// 16 bytes from the OS CSPRNG. The single source of name/token randomness
/// for exclusive temp files, shard generations, pull-identity objects and
/// reset tombstones (ADR-0098/0087/0090). Failures share one storage error.
pub(crate) fn random_bytes_16() -> Result<[u8; 16]> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|e| Error::storage(format!("OS randomness: {e}")))?;
    Ok(bytes)
}

/// Render 16 bytes as 32 lowercase hex characters.
pub(crate) fn hex_16(bytes: [u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

/// 128-bit OS-random token as lowercase hex.
pub(crate) fn random_hex_16() -> Result<String> {
    Ok(hex_16(random_bytes_16()?))
}

/// Exactly 32 lowercase hexadecimal characters, the token shape that
/// [`hex_16`] and [`random_hex_16`] produce.
#[cfg_attr(not(parquet_codec), allow(dead_code))]
pub(crate) fn is_lower_hex_32(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

const TEMP_ATTEMPTS: usize = 8;

fn temp_path(path: &str) -> Result<String> {
    Ok(format!("{path}.tmp-{}", random_hex_16()?))
}

fn cleanup_temp(vfs: &dyn Vfs, tmp: &str, original: Error) -> Error {
    // A failing rename can have published the inode already (notably Bundle).
    // Never remove the destination; touch the temp only if it still exists.
    let cleanup = vfs
        .exists(tmp)
        .and_then(|exists| if exists { vfs.remove_file(tmp) } else { Ok(()) });
    match cleanup {
        Ok(()) => original,
        Err(e) => Error::io(format!("{original}; temporary file cleanup failed: {e}")),
    }
}

/// Write to an exclusive, OS-random temp, sync and close it, rename, then
/// persist the containing directory on Native Unix (ADR-0098).
///
/// The handle is scoped so it is provably closed before the rename.
/// Two Windows rules force this exact shape (POSIX tolerates both
/// violations, so only the structure pins them):
///  * `sync_all` needs a write handle: FlushFileBuffers requires
///    GENERIC_WRITE, so a read-only open + `sync_all` fails with
///    "Access is denied" (os error 5) — hence create/truncate.
///  * renaming a file that still has an open handle fails with
///    os error 5 — hence the scope ends before `rename`.
pub fn atomic_write(vfs: &dyn Vfs, path: &str, bytes: &[u8]) -> Result<()> {
    atomic_write_stream(vfs, path, |file| file.write_all(bytes))
}

/// Produce a replacement directly into an exclusive temporary file. The
/// producer finishes (and drops any source handles it owns) before rename.
/// Before publication, errors retain the original destination and clean up
/// the temporary file; directory-sync errors occur after publication.
pub(crate) fn atomic_write_stream(
    vfs: &dyn Vfs,
    path: &str,
    produce: impl FnOnce(&mut dyn VfsFile) -> Result<()>,
) -> Result<()> {
    let parent = virt_parent(path).unwrap_or("");
    vfs.create_dir_all_for_atomic(parent)?;
    for _ in 0..TEMP_ATTEMPTS {
        let tmp = temp_path(path)?;
        let Some(mut file) = vfs.open_exclusive(&tmp)? else {
            continue;
        };
        let written = produce(&mut *file).and_then(|()| file.sync_all());
        // In particular, Windows requires the write handle closed before rename.
        drop(file);
        if let Err(e) = written {
            return Err(cleanup_temp(vfs, &tmp, e));
        }
        if let Err(e) = vfs.rename(&tmp, path) {
            return Err(cleanup_temp(vfs, &tmp, e));
        }
        // Once published, never clean up or retry after a failed directory sync.
        return vfs.sync_dir(parent);
    }
    Err(Error::io(format!(
        "could not create an exclusive temporary file for {path} after {TEMP_ATTEMPTS} collisions"
    )))
}

/// A `Write` sink bounded by a byte `limit`.
///
/// The one shared shape for every bounded serializer in this crate: count the
/// bytes written, and on the first write that would cross `limit` mark
/// `exceeded` and return the configured error. Captured bytes go to a
/// caller-owned `Vec` ([`BoundedSink::capture`]).
///
/// `BoundedSink::capture_capped` additionally bounds the backing allocation
/// with `try_reserve_exact` and geometric growth, never asking the allocator
/// for more than `min(limit, capacity_cap)` (used by the Parquet shard writer).
pub(crate) struct BoundedSink<'a> {
    bytes: u64,
    limit: u64,
    output: Option<&'a mut Vec<u8>>,
    exceeded: bool,
    message: &'static str,
    capacity_cap: Option<u64>,
}

impl<'a> BoundedSink<'a> {
    /// Count bytes without retaining the serialized payload.
    pub(crate) fn count(limit: u64, message: &'static str) -> Self {
        Self {
            bytes: 0,
            limit,
            output: None,
            exceeded: false,
            message,
            capacity_cap: None,
        }
    }

    /// Append captured bytes into `output`, up to `limit`.
    pub(crate) fn capture(output: &'a mut Vec<u8>, limit: u64, message: &'static str) -> Self {
        Self {
            bytes: 0,
            limit,
            output: Some(output),
            exceeded: false,
            message,
            capacity_cap: None,
        }
    }

    pub(crate) fn exceeded(&self) -> bool {
        self.exceeded
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
}

#[cfg(parquet_codec)]
impl<'a> BoundedSink<'a> {
    /// Append into `output` while keeping its capacity below
    /// `min(limit, capacity_cap)` with `try_reserve_exact` growth.
    #[cfg(test)]
    pub(crate) fn capture_capped(
        output: &'a mut Vec<u8>,
        limit: u64,
        capacity_cap: u64,
        message: &'static str,
    ) -> Self {
        Self {
            capacity_cap: Some(capacity_cap),
            ..Self::capture(output, limit, message)
        }
    }

    /// Backing capacity of the capture buffer.
    #[cfg(test)]
    pub(crate) fn capacity(&self) -> usize {
        self.output.as_ref().map_or(0, |out| out.capacity())
    }

    /// Bytes captured so far.
    #[cfg(test)]
    pub(crate) fn captured_len(&self) -> usize {
        self.output.as_ref().map_or(0, |out| out.len())
    }
}

impl Write for BoundedSink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let message = self.message;
        let Some(total) = self.bytes.checked_add(buf.len() as u64) else {
            self.exceeded = true;
            return Err(io::Error::other(message));
        };
        if total > self.limit {
            self.exceeded = true;
            return Err(io::Error::other(message));
        }
        if let Some(out) = self.output.as_mut() {
            let result = match self.capacity_cap {
                None => {
                    out.extend_from_slice(buf);
                    Ok(())
                }
                Some(cap) => append_bounded(out, buf, cap.min(self.limit), message),
            };
            if let Err(error) = result {
                self.exceeded = true;
                return Err(error);
            }
        }
        self.bytes = total;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Grow `out` by `buf` without ever asking the allocator for more than
/// `limit` total capacity. `try_reserve_exact` reserves relative to `len`, so
/// growth is geometric but clamped to the cap.
fn append_bounded(
    out: &mut Vec<u8>,
    buf: &[u8],
    limit: u64,
    message: &'static str,
) -> io::Result<()> {
    let envelope_error = || io::Error::other(message);
    let limit = usize::try_from(limit).map_err(|_| envelope_error())?;
    if out.capacity() > limit {
        return Err(envelope_error());
    }
    let end = out
        .len()
        .checked_add(buf.len())
        .filter(|&end| end <= limit)
        .ok_or_else(envelope_error)?;
    if end > out.capacity() {
        let target = out.capacity().max(1).saturating_mul(2).max(end).min(limit);
        out.try_reserve_exact(target - out.len())
            .map_err(io::Error::other)?;
        // An allocator may provide more capacity than requested. Do not rely
        // on shrinking it to enforce the envelope.
        if out.capacity() > limit {
            return Err(envelope_error());
        }
    }
    out.extend_from_slice(buf);
    Ok(())
}

pub fn read_to_vec(vfs: &dyn Vfs, path: &str) -> Result<Vec<u8>> {
    let mut f = vfs.open(path, VfsOpen::read_only())?;
    let len = f.len()? as usize;
    let mut buf = vec![0u8; len];
    if len > 0 {
        f.read_exact(&mut buf)?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests;

#[cfg(all(test, parquet_codec))]
mod sink_tests;
